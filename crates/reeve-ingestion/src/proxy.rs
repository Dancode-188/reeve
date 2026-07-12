//! The HTTP proxy input path: point `ANTHROPIC_BASE_URL` at this server
//! and an uninstrumented tool appears in the cockpit. Requests forward to
//! the real API; spans are synthesized from what passes through and fed
//! into the same pipeline the OTel receiver uses.
//!
//! The Authorization and x-api-key headers are forwarded in memory and
//! never logged, persisted, or attached to any synthesized span.

use crate::normalize::PipelineSpan;
use crate::sse::SseAccumulator;
use crate::threading::{ConversationTracker, ResponseInfo, ToolCall, TurnPlacement, TurnRoot};
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::Response;
use futures_util::StreamExt;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue, any_value};
use opentelemetry_proto::tonic::trace::v1::Span as OtlpSpan;
use opentelemetry_proto::tonic::trace::v1::Status as OtlpStatus;
use reeve_model::entity::{IntegrationPath, ProxyInterventions, ProxyPayload};
use reeve_model::signal::IngestionEvent;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, mpsc};

const DEFAULT_UPSTREAM: &str = "https://api.anthropic.com";
/// A stream that goes silent for this long is dead: cancel upstream and
/// finalize with what accumulated.
const DEFAULT_STREAM_CHUNK_TIMEOUT_MS: u64 = 30_000;

struct ProxyState {
    client: reqwest::Client,
    upstream: String,
    pipeline_tx: mpsc::Sender<PipelineSpan>,
    signal_tx: broadcast::Sender<IngestionEvent>,
    /// Overrides User-Agent derivation when set (REEVE_PROXY_AGENT_NAME).
    agent_name_override: Option<String>,
    stream_chunk_timeout: std::time::Duration,
    /// Conversation threading state; see the threading module.
    tracker: std::sync::Mutex<ConversationTracker>,
    /// Commands queued by the dispatcher for proxy-path agents, applied
    /// here by modifying the next request before it forwards.
    interventions: Option<ProxyInterventions>,
    /// Traces with a Messages round trip currently in flight, shared with
    /// the assembler so a trace mid-generation is never called idle. The
    /// count handles concurrent requests on one turn.
    active_streams: Option<crate::assemble::ActiveStreams>,
    /// Traces whose turn is still open, with the conversation's last
    /// request time: the between-round-trips exemption from the idle
    /// timeout, held while the client runs its tools (#200).
    open_turns: Option<crate::assemble::OpenTurns>,
}

fn trace_key(trace_id: &[u8]) -> reeve_model::ids::TraceId {
    trace_id
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>()
        .into()
}

/// Records that this trace's turn is open and its conversation just sent
/// a request; every request refreshes the recency the assembler checks.
fn mark_turn_open(state: &ProxyState, trace_id: &[u8]) {
    if let Some(ref turns) = state.open_turns {
        turns
            .lock()
            .expect("open turns mutex poisoned")
            .insert(trace_key(trace_id), std::time::Instant::now());
    }
}

/// The turn closed: its exemption ends with it.
fn mark_turn_closed(state: &ProxyState, trace_id: &[u8]) {
    if let Some(ref turns) = state.open_turns {
        turns
            .lock()
            .expect("open turns mutex poisoned")
            .remove(&trace_key(trace_id));
    }
}

/// Marks a trace's round trip in flight for the assembler's idle check.
/// Increment when the upstream request departs, decrement on EVERY exit
/// path: a leaked entry would hold a dead trace in flight forever.
fn mark_stream(state: &ProxyState, trace_id: &[u8], delta: i64) {
    let Some(ref streams) = state.active_streams else {
        return;
    };
    let key = trace_key(trace_id);
    let mut map = streams.lock().expect("active streams mutex poisoned");
    let count = map.entry(key.clone()).or_insert(0);
    if delta > 0 {
        *count += 1;
    } else {
        *count = count.saturating_sub(1);
        if *count == 0 {
            map.remove(&key);
        }
    }
}

pub async fn run(
    addr: SocketAddr,
    pipeline_tx: mpsc::Sender<PipelineSpan>,
    signal_tx: broadcast::Sender<IngestionEvent>,
    interventions: ProxyInterventions,
    active_streams: crate::assemble::ActiveStreams,
    open_turns: crate::assemble::OpenTurns,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let upstream =
        std::env::var("REEVE_PROXY_UPSTREAM").unwrap_or_else(|_| DEFAULT_UPSTREAM.to_string());
    run_with(
        addr,
        upstream,
        std::env::var("REEVE_PROXY_AGENT_NAME").ok(),
        std::time::Duration::from_millis(DEFAULT_STREAM_CHUNK_TIMEOUT_MS),
        pipeline_tx,
        signal_tx,
        Some(interventions),
        Some(active_streams),
        Some(open_turns),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn run_with(
    addr: SocketAddr,
    upstream: String,
    agent_name_override: Option<String>,
    stream_chunk_timeout: std::time::Duration,
    pipeline_tx: mpsc::Sender<PipelineSpan>,
    signal_tx: broadcast::Sender<IngestionEvent>,
    interventions: Option<ProxyInterventions>,
    active_streams: Option<crate::assemble::ActiveStreams>,
    open_turns: Option<crate::assemble::OpenTurns>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let state = Arc::new(ProxyState {
        client: reqwest::Client::new(),
        upstream,
        pipeline_tx,
        signal_tx,
        agent_name_override,
        stream_chunk_timeout,
        tracker: std::sync::Mutex::new(ConversationTracker::default()),
        interventions,
        active_streams,
        open_turns,
    });

    let app = axum::Router::new()
        .fallback(forward)
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(addr = %addr, upstream = %state.upstream, "HTTP proxy listening");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Forwards any request to the upstream. Non-streaming Messages API
/// round trips synthesize a span; everything else passes through
/// untouched, streaming bodies included.
async fn forward(
    State(state): State<Arc<ProxyState>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let arrived = SystemTime::now();
    let path = uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or(uri.path());
    let url = format!("{}{}", state.upstream, path);

    // Threading placement happens before the forward so tool spans render
    // while the model is still thinking; its cost lands inside the
    // measured overhead below, honestly.
    let agent_name = state
        .agent_name_override
        .clone()
        .unwrap_or_else(|| derive_agent_name(&headers));

    // The circuit breaker: a killed agent's Messages requests are refused
    // instead of forwarded. Enforcement is local, so the agent cannot
    // spend another token no matter how broken its loop is. Only the
    // Messages path is refused, since that is where money burns.
    if method == Method::POST && uri.path().ends_with("/v1/messages") {
        let killed = state.interventions.as_ref().is_some_and(|iv| {
            iv.lock()
                .expect("interventions mutex poisoned")
                .killed
                .contains(&reeve_model::ids::agent_id_from_service(
                    &agent_name,
                    "proxy",
                ))
        });
        if killed {
            tracing::info!(agent = %agent_name, "circuit breaker refused a request from a killed agent");
            return Response::builder()
                .status(StatusCode::FORBIDDEN)
                .header("content-type", "application/json")
                .body(Body::from(
                    "{\"type\":\"error\",\"error\":{\"type\":\"permission_error\",\"message\":\"an operator killed this agent via Reeve; API access is stopped until Reeve restarts\"}}",
                ))
                .expect("static response construction cannot fail");
        }
    }
    let placement = if method == Method::POST && uri.path().ends_with("/v1/messages") {
        serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|req_json| {
                let messages = req_json.get("messages")?.as_array()?.clone();
                Some(
                    state
                        .tracker
                        .lock()
                        .expect("tracker mutex poisoned")
                        .place_request(&agent_name, &messages, arrived, random_bytes),
                )
            })
    } else {
        None
    };
    if let Some(ref placement) = placement {
        // The turn is open and its conversation just spoke: hold the
        // idle timeout across the client-side tool gap that follows.
        mark_turn_open(&state, &placement.trace_id);
        for tool in &placement.tools {
            emit_tool_span(&state, &agent_name, placement, tool).await;
        }
    }

    // Queued interventions apply here, after threading fingerprinted the
    // ORIGINAL body: the client never resends what it never sent, so the
    // injection cannot disturb prefix matching.
    let body = if placement.is_some() {
        apply_interventions(&state, &agent_name, body)
    } else {
        body
    };

    let mut req = state.client.request(method.clone(), &url);
    for (name, value) in headers.iter() {
        // Host belongs to the upstream; hyper sets the rest correctly.
        // Accept-Encoding is stripped so the upstream answers in plain
        // text: the proxy reads what passes through, and a compressed
        // body is unreadable to the tee while the client decompresses
        // happily. Real Claude Code sends gzip/br/zstd; every span went
        // model-unknown and cost-less until this was dropped.
        if name == axum::http::header::HOST
            || name == axum::http::header::CONTENT_LENGTH
            || name == axum::http::header::ACCEPT_ENCODING
        {
            continue;
        }
        req = req.header(name, value);
    }
    // Receipt-to-forward overhead: the measured cost of sitting in the
    // path, recorded on the span so the low-overhead claim is a number.
    let overhead_ms = arrived
        .elapsed()
        .map(|d| d.as_secs_f64() * 1e3)
        .unwrap_or(0.0);

    // From here until the span is synthesized, the turn's trace must not
    // be called idle no matter how long the model takes: the assembler's
    // timeout once flushed mid-turn and dropped a session's spans (#182).
    if let Some(ref p) = placement {
        mark_stream(&state, &p.trace_id, 1);
    }

    let upstream_resp = match req.body(body.clone()).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "proxy could not reach upstream");
            if let Some(ref p) = placement {
                mark_stream(&state, &p.trace_id, -1);
            }
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from(format!(
                    "{{\"type\":\"error\",\"error\":{{\"type\":\"api_error\",\"message\":\"reeve proxy could not reach upstream: {e}\"}}}}"
                )))
                .expect("static response construction cannot fail");
        }
    };

    let status = upstream_resp.status();
    let resp_headers = upstream_resp.headers().clone();
    let streaming = resp_headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("text/event-stream"));

    if streaming {
        let body = stream_and_accumulate(
            state.clone(),
            upstream_resp,
            agent_name,
            placement,
            arrived,
            overhead_ms,
        );
        return build_response(status, &resp_headers, body);
    }

    let resp_body = match upstream_resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "proxy failed reading upstream response");
            if let Some(ref p) = placement {
                mark_stream(&state, &p.trace_id, -1);
            }
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from("upstream response read failed"))
                .expect("static response construction cannot fail");
        }
    };

    // Unmark only after the span has entered the pipeline: the trace
    // stays exempt from the idle timeout until its evidence is in.
    let placement_trace_id = placement.as_ref().map(|p| p.trace_id.clone());
    if method == Method::POST && uri.path().ends_with("/v1/messages") {
        synthesize_span(
            &state,
            &agent_name,
            placement,
            &resp_body,
            status.as_u16(),
            arrived,
            overhead_ms,
        )
        .await;
    }
    if let Some(tid) = placement_trace_id {
        mark_stream(&state, &tid, -1);
    }

    build_response(status, &resp_headers, Body::from(resp_body))
}

fn build_response(status: reqwest::StatusCode, headers: &HeaderMap, body: Body) -> Response {
    let mut builder = Response::builder().status(status.as_u16());
    for (name, value) in headers.iter() {
        // Hop-by-hop and framing headers are hyper's job to set.
        if matches!(
            name.as_str(),
            "transfer-encoding" | "content-length" | "connection"
        ) {
            continue;
        }
        builder = builder.header(name, value);
    }
    builder
        .body(body)
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

/// How a stream ended. Every path out of a stream produces exactly one
/// of these, and every one finalizes the span.
#[derive(Debug, Clone, Copy, PartialEq)]
enum StreamOutcome {
    Completed,
    /// The client dropped the connection mid-stream. Upstream is
    /// cancelled immediately so tokens stop being generated. Not a
    /// failure: closing a tool is behavior, not breakage.
    ClientDisconnected,
    /// The upstream sent an error event or the connection to it died.
    /// The error was forwarded to the client unchanged; retrying is the
    /// client SDK's decision, never the proxy's.
    ApiFailed,
    /// No chunk arrived within the per-chunk timeout.
    StreamTimedOut,
}

impl StreamOutcome {
    fn label(self) -> &'static str {
        match self {
            StreamOutcome::Completed => "completed",
            StreamOutcome::ClientDisconnected => "client_disconnected",
            StreamOutcome::ApiFailed => "api_failed",
            StreamOutcome::StreamTimedOut => "stream_timed_out",
        }
    }
}

/// Forwards SSE chunks to the client the moment they arrive while a side
/// accumulator reconstructs the round trip. Chunks go client-first: the
/// send happens before the parse, so the proxy adds no latency the
/// client can observe. Emits StreamingUpdate per text delta so the
/// cockpit's streaming box renders the generation live, and finalizes a
/// span through every exit path.
fn stream_and_accumulate(
    state: Arc<ProxyState>,
    upstream_resp: reqwest::Response,
    agent_name: String,
    placement: Option<TurnPlacement>,
    arrived: SystemTime,
    overhead_ms: f64,
) -> Body {
    let (body_tx, body_rx) = mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(32);

    tokio::spawn(async move {
        let mut upstream = upstream_resp.bytes_stream();
        let mut acc = SseAccumulator::default();
        let (trace_id, parent_span_id) = match &placement {
            Some(p) => (p.trace_id.clone(), p.root_span_id.clone()),
            None => (random_bytes(16), Vec::new()),
        };
        let span_id = random_bytes(8);
        let span_id_hex: String = span_id.iter().map(|b| format!("{:02x}", b)).collect();
        let trace_id_hex: String = trace_id.iter().map(|b| format!("{:02x}", b)).collect();
        let mut first_chunk_at: Option<SystemTime> = None;
        let mut outcome = StreamOutcome::Completed;

        loop {
            let next = tokio::time::timeout(state.stream_chunk_timeout, upstream.next()).await;
            let chunk = match next {
                Err(_) => {
                    outcome = StreamOutcome::StreamTimedOut;
                    break;
                }
                Ok(None) => break,
                Ok(Some(Err(e))) => {
                    tracing::debug!(error = %e, "upstream stream error");
                    outcome = StreamOutcome::ApiFailed;
                    break;
                }
                Ok(Some(Ok(chunk))) => chunk,
            };
            first_chunk_at.get_or_insert_with(SystemTime::now);

            // Client first: nothing the accumulator does may delay the
            // chunk. A failed send means the client is gone; cancel
            // upstream by leaving the loop, which drops the connection.
            if body_tx.send(Ok(chunk.clone())).await.is_err() {
                outcome = StreamOutcome::ClientDisconnected;
                break;
            }

            let update = acc.feed(&chunk);
            if update.api_failed {
                outcome = StreamOutcome::ApiFailed;
                // Keep forwarding whatever follows the error event; the
                // upstream closes the stream on its own terms.
            }
            if update.content_changed {
                // The wire only reports output tokens at stream end, so
                // the running estimate counts what is already committed
                // (input and cache, usually the bulk for agentic clients)
                // plus the accumulated text at roughly four chars per
                // token. The final span cost, from real usage, corrects
                // whatever this guessed.
                let output_estimate = (acc.content.len() as u64 / 4).max(acc.output_tokens);
                let cost_so_far = acc.model.as_deref().and_then(|m| {
                    crate::pricing::estimate(
                        m,
                        acc.input_tokens,
                        output_estimate,
                        acc.cache_read_tokens,
                        acc.cache_creation_tokens,
                    )
                });
                let _ = state.signal_tx.send(IngestionEvent::StreamingUpdate {
                    trace_id: trace_id_hex.clone().into(),
                    span_id: span_id_hex.clone().into(),
                    agent_id: reeve_model::ids::agent_id_from_service(&agent_name, "proxy"),
                    content: acc.content.clone(),
                    cost_so_far,
                });
            }
        }
        drop(body_tx);

        let ttft_ms = first_chunk_at.and_then(|t| {
            t.duration_since(arrived)
                .ok()
                .map(|d| d.as_secs_f64() * 1e3)
        });
        // The idle exemption holds through every stream outcome; drop it
        // only once the finalized span has entered the pipeline.
        let placement_trace_id = placement.as_ref().map(|p| p.trace_id.clone());
        finalize_stream_span(
            &state,
            &agent_name,
            placement,
            acc,
            outcome,
            trace_id,
            parent_span_id,
            span_id,
            arrived,
            ttft_ms,
            overhead_ms,
        )
        .await;
        if let Some(tid) = placement_trace_id {
            mark_stream(&state, &tid, -1);
        }
    });

    Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(body_rx))
}

/// The streaming counterpart of synthesize_span: same shape of span,
/// built from the accumulator, plus the outcome and time-to-first-token.
#[allow(clippy::too_many_arguments)]
async fn finalize_stream_span(
    state: &ProxyState,
    agent_name: &str,
    placement: Option<TurnPlacement>,
    acc: SseAccumulator,
    outcome: StreamOutcome,
    trace_id: Vec<u8>,
    parent_span_id: Vec<u8>,
    span_id: Vec<u8>,
    arrived: SystemTime,
    ttft_ms: Option<f64>,
    overhead_ms: f64,
) {
    let model = acc.model.unwrap_or_else(|| "unknown".to_string());
    let mut attributes = vec![
        kv_str("gen_ai.system", "anthropic"),
        kv_str("gen_ai.operation.name", "chat"),
        kv_str("gen_ai.request.model", &model),
        kv_int("gen_ai.usage.input_tokens", acc.input_tokens as i64),
        kv_int("gen_ai.usage.output_tokens", acc.output_tokens as i64),
        kv_int(
            "gen_ai.usage.total_tokens",
            (acc.input_tokens + acc.output_tokens) as i64,
        ),
        kv_str("reeve.proxy.stream_outcome", outcome.label()),
        kv_double("reeve.proxy.overhead_ms", overhead_ms),
    ];
    if let Some(ttft) = ttft_ms {
        attributes.push(kv_double("reeve.proxy.ttft_ms", ttft));
    }
    if let Some(ref p) = placement {
        attributes.push(kv_int(
            "reeve.proxy.context_messages",
            p.message_count as i64,
        ));
    }
    if acc.thinking_tokens > 0 {
        attributes.push(kv_int(
            "gen_ai.usage.thinking_tokens",
            acc.thinking_tokens as i64,
        ));
    }
    surface_compaction(state, agent_name, &acc.applied_edits, &mut attributes);
    if acc.cache_read_tokens > 0 {
        attributes.push(kv_int(
            "gen_ai.usage.cache_read_tokens",
            acc.cache_read_tokens as i64,
        ));
    }
    if acc.cache_creation_tokens > 0 {
        attributes.push(kv_int(
            "gen_ai.usage.cache_creation_tokens",
            acc.cache_creation_tokens as i64,
        ));
    }
    if acc.cache_read_tokens > 0 || acc.cache_creation_tokens > 0 {
        if let Some(saved) =
            crate::pricing::cache_saved(&model, acc.cache_read_tokens, acc.cache_creation_tokens)
        {
            attributes.push(kv_double("gen_ai.usage.cache_saved", saved));
        }
    }
    if let Some(cost) = crate::pricing::estimate(
        &model,
        acc.input_tokens,
        acc.output_tokens,
        acc.cache_read_tokens,
        acc.cache_creation_tokens,
    ) {
        attributes.push(kv_double("gen_ai.usage.cost", cost));
    }

    // Upstream failures and timeouts are failures; a client disconnect
    // is not, because a developer closing their tool is behavior.
    let status_code = match outcome {
        StreamOutcome::ApiFailed | StreamOutcome::StreamTimedOut => 2,
        StreamOutcome::Completed | StreamOutcome::ClientDisconnected => 1,
    };

    let ended = SystemTime::now();
    let span = OtlpSpan {
        trace_id,
        span_id: span_id.clone(),
        parent_span_id,
        name: "gen_ai.chat".to_string(),
        start_time_unix_nano: to_nanos(arrived),
        end_time_unix_nano: to_nanos(ended),
        attributes,
        status: Some(OtlpStatus {
            code: status_code,
            message: String::new(),
        }),
        ..Default::default()
    };
    emit_pipeline_span(state, agent_name, span, arrived).await;

    if let Some(ref p) = placement {
        // A dead stream still ends its turn: whatever the outcome, the
        // assistant is not going to request more tools on this round trip,
        // so an outcome other than tool_use closes the turn honestly.
        let stop_reason = match outcome {
            StreamOutcome::Completed => acc.stop_reason,
            _ => Some(format!("proxy:{}", outcome.label())),
        };
        let root = state
            .tracker
            .lock()
            .expect("tracker mutex poisoned")
            .record_response(
                agent_name,
                &p.trace_id,
                ResponseInfo {
                    chat_span_id: span_id,
                    tool_uses: acc.tool_uses,
                    stop_reason,
                    ended_at: ended,
                },
            );
        if let Some(root) = root {
            emit_turn_root(state, agent_name, root).await;
        }
    }
}

/// Drains this agent's queued interventions into the outgoing request
/// body: each command appends an operator message, most recent last.
/// Expired commands drop silently here; the dispatcher's expiry loop
/// owns the audit line. Applications are reported through the shared
/// queue for the dispatcher to fold into its ack handling.
fn apply_interventions(
    state: &ProxyState,
    agent_name: &str,
    body: axum::body::Bytes,
) -> axum::body::Bytes {
    let Some(ref interventions) = state.interventions else {
        return body;
    };
    let agent_id = reeve_model::ids::agent_id_from_service(agent_name, "proxy");
    let now_ms = to_millis(SystemTime::now());

    let commands: Vec<reeve_model::entity::ProxyCommand> = {
        let mut q = interventions.lock().expect("interventions mutex poisoned");
        match q.pending.get_mut(&agent_id) {
            Some(queue) => std::mem::take(queue).into_iter().collect(),
            None => return body,
        }
    };
    if commands.is_empty() {
        return body;
    }

    let Ok(mut parsed) = serde_json::from_slice::<serde_json::Value>(&body) else {
        // Unparseable body: put the commands back rather than losing them.
        let mut q = interventions.lock().expect("interventions mutex poisoned");
        q.pending.entry(agent_id).or_default().extend(commands);
        return body;
    };
    let Some(messages) = parsed.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        let mut q = interventions.lock().expect("interventions mutex poisoned");
        q.pending.entry(agent_id).or_default().extend(commands);
        return body;
    };

    let mut applied_any = false;
    for cmd in commands {
        if cmd.valid_until_ms < now_ms {
            tracing::info!(command_id = %cmd.id, "queued proxy command expired before application");
            continue;
        }
        let text = match &cmd.payload {
            ProxyPayload::Redirect { instruction } => format!(
                "[Operator redirect via Reeve] Disregard the current approach and instead: {instruction}"
            ),
            ProxyPayload::InjectContext { context } => {
                format!("[Operator context via Reeve] {context}")
            }
        };
        messages.push(serde_json::json!({"role": "user", "content": text}));
        applied_any = true;
        interventions
            .lock()
            .expect("interventions mutex poisoned")
            .applied
            .push((cmd.id, agent_id.clone(), now_ms));
    }

    if !applied_any {
        return body;
    }
    match serde_json::to_vec(&parsed) {
        Ok(modified) => axum::body::Bytes::from(modified),
        Err(_) => body,
    }
}

/// Sends one synthesized span into the pipeline under the proxy agent's
/// identity.
async fn emit_pipeline_span(
    state: &ProxyState,
    agent_name: &str,
    span: OtlpSpan,
    arrived: SystemTime,
) {
    let ps = PipelineSpan {
        span,
        service_name: agent_name.to_string(),
        service_instance_id: "proxy".to_string(),
        framework: "proxy".to_string(),
        arrived_at: to_millis(arrived),
        clock_offset_ms: 0,
        integration: IntegrationPath::Proxy,
    };
    if state.pipeline_tx.send(ps).await.is_err() {
        tracing::warn!("normalize stage unavailable, proxy span discarded");
    }
}

/// A reconstructed tool call becomes a child span of the chat span whose
/// response requested it, covering the gap between that response and the
/// request that carried the result.
async fn emit_tool_span(
    state: &ProxyState,
    agent_name: &str,
    placement: &TurnPlacement,
    tool: &ToolCall,
) {
    let span = OtlpSpan {
        trace_id: placement.trace_id.clone(),
        span_id: random_bytes(8),
        parent_span_id: tool.parent_span_id.clone(),
        name: format!("gen_ai.tool:{}", tool.name),
        start_time_unix_nano: to_nanos(tool.started_at),
        end_time_unix_nano: to_nanos(tool.ended_at),
        attributes: vec![
            kv_str("gen_ai.system", "anthropic"),
            kv_str("gen_ai.operation.name", "execute_tool"),
            // The clean tool name, so the judge scores [bash, read]
            // rather than raw operation names.
            kv_str("gen_ai.tool.name", &tool.name),
        ],
        status: Some(OtlpStatus {
            code: if tool.is_error { 2 } else { 1 },
            message: String::new(),
        }),
        ..Default::default()
    };
    emit_pipeline_span(state, agent_name, span, tool.ended_at).await;
}

/// The synthetic turn root: the no-parent span whose arrival tells the
/// assembler the trace is complete, emitted only when the turn ends,
/// exactly as SDK agents emit their task root last.
async fn emit_turn_root(state: &ProxyState, agent_name: &str, root: TurnRoot) {
    // The root only exists because the turn closed: the between-round-
    // trips exemption ends here, on both proxy paths.
    mark_turn_closed(state, &root.trace_id);
    let span = OtlpSpan {
        trace_id: root.trace_id,
        span_id: root.span_id,
        name: root.name,
        start_time_unix_nano: to_nanos(root.started_at),
        end_time_unix_nano: to_nanos(root.ended_at),
        attributes: vec![kv_str("gen_ai.operation.name", "chat")],
        status: Some(OtlpStatus {
            code: 1,
            message: String::new(),
        }),
        ..Default::default()
    };
    emit_pipeline_span(state, agent_name, span, root.ended_at).await;
}

/// One Messages API round trip becomes one gen_ai.chat span carrying the
/// model, token usage, and estimated cost, threaded into its turn's trace
/// as a child of the turn root. Upstream failures (429s, 5xx) synthesize
/// failed spans so retry storms render visibly.
async fn synthesize_span(
    state: &ProxyState,
    agent_name: &str,
    placement: Option<TurnPlacement>,
    resp_body: &[u8],
    http_status: u16,
    arrived: SystemTime,
    overhead_ms: f64,
) {
    let ended = SystemTime::now();

    let parsed: serde_json::Value = serde_json::from_slice(resp_body).unwrap_or_default();
    let model = parsed
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let usage = parsed.get("usage");
    let get_u64 = |key: &str| {
        usage
            .and_then(|u| u.get(key))
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
    };
    let input_tokens = get_u64("input_tokens");
    let output_tokens = get_u64("output_tokens");
    let thinking_tokens = usage
        .and_then(|u| u.get("output_tokens_details"))
        .and_then(|d| d.get("thinking_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_read = get_u64("cache_read_input_tokens");
    let cache_creation = get_u64("cache_creation_input_tokens");
    let stop_reason = parsed
        .get("stop_reason")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let tool_uses: Vec<(String, String)> = parsed
        .get("content")
        .and_then(|c| c.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
                .filter_map(|b| {
                    Some((
                        b.get("id")?.as_str()?.to_string(),
                        b.get("name")?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();

    let mut attributes = vec![
        kv_str("gen_ai.system", "anthropic"),
        kv_str("gen_ai.operation.name", "chat"),
        kv_str("gen_ai.request.model", &model),
        kv_int("gen_ai.usage.input_tokens", input_tokens as i64),
        kv_int("gen_ai.usage.output_tokens", output_tokens as i64),
        kv_int(
            "gen_ai.usage.total_tokens",
            (input_tokens + output_tokens) as i64,
        ),
        kv_int("http.response.status_code", http_status as i64),
        kv_double("reeve.proxy.overhead_ms", overhead_ms),
    ];
    if let Some(ref p) = placement {
        attributes.push(kv_int(
            "reeve.proxy.context_messages",
            p.message_count as i64,
        ));
    }
    if thinking_tokens > 0 {
        attributes.push(kv_int(
            "gen_ai.usage.thinking_tokens",
            thinking_tokens as i64,
        ));
    }
    let applied_edits: Vec<String> = parsed
        .get("context_management")
        .and_then(|c| c.get("applied_edits"))
        .and_then(|e| e.as_array())
        .map(|edits| {
            edits
                .iter()
                .filter_map(|e| e.get("type").and_then(|t| t.as_str()))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    surface_compaction(state, agent_name, &applied_edits, &mut attributes);
    if cache_read > 0 {
        attributes.push(kv_int("gen_ai.usage.cache_read_tokens", cache_read as i64));
    }
    if cache_creation > 0 {
        attributes.push(kv_int(
            "gen_ai.usage.cache_creation_tokens",
            cache_creation as i64,
        ));
    }
    if cache_read > 0 || cache_creation > 0 {
        if let Some(saved) = crate::pricing::cache_saved(&model, cache_read, cache_creation) {
            attributes.push(kv_double("gen_ai.usage.cache_saved", saved));
        }
    }
    if let Some(cost) = crate::pricing::estimate(
        &model,
        input_tokens,
        output_tokens,
        cache_read,
        cache_creation,
    ) {
        attributes.push(kv_double("gen_ai.usage.cost", cost));
    }

    let status_code = if http_status >= 400 { 2 } else { 1 };
    let chat_span_id = random_bytes(8);
    // A request without a parseable conversation synthesizes a standalone
    // span, its own root in its own trace: the pre-threading behavior,
    // kept as the fallback so unusual clients still render.
    let (trace_id, parent_span_id) = match &placement {
        Some(p) => (p.trace_id.clone(), p.root_span_id.clone()),
        None => (random_bytes(16), Vec::new()),
    };

    let span = OtlpSpan {
        trace_id,
        span_id: chat_span_id.clone(),
        parent_span_id,
        name: "gen_ai.chat".to_string(),
        start_time_unix_nano: to_nanos(arrived),
        end_time_unix_nano: to_nanos(ended),
        attributes,
        status: Some(OtlpStatus {
            code: status_code,
            message: String::new(),
        }),
        ..Default::default()
    };
    emit_pipeline_span(state, agent_name, span, arrived).await;

    if let Some(ref p) = placement {
        let root = state
            .tracker
            .lock()
            .expect("tracker mutex poisoned")
            .record_response(
                agent_name,
                &p.trace_id,
                ResponseInfo {
                    chat_span_id,
                    tool_uses,
                    stop_reason,
                    ended_at: ended,
                },
            );
        if let Some(root) = root {
            emit_turn_root(state, agent_name, root).await;
        }
    }
}

/// The proxy path has no service.name; the client's User-Agent product
/// token is the honest stand-in ("claude-cli/1.2.3 ..." names the agent
/// claude-cli). REEVE_PROXY_AGENT_NAME overrides it.
fn derive_agent_name(headers: &HeaderMap) -> String {
    headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .and_then(|ua| ua.split_whitespace().next())
        .map(|token| token.split('/').next().unwrap_or(token).to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "proxy-agent".to_string())
}

/// Stamps applied context edits on the span and tells ALERTS. Compaction
/// changes the conversation prefix underneath threading, so the next
/// request legitimately starts a new trace; the notice is what keeps
/// that from reading as a mystery. No-op when nothing was applied,
/// which is every response seen on the wire so far.
fn surface_compaction(
    state: &ProxyState,
    agent_name: &str,
    applied_edits: &[String],
    attributes: &mut Vec<KeyValue>,
) {
    if applied_edits.is_empty() {
        return;
    }
    attributes.push(kv_int(
        "reeve.context.applied_edits",
        applied_edits.len() as i64,
    ));
    attributes.push(kv_str("reeve.context.edit_types", &applied_edits.join(",")));
    // Display names drop the trailing date revision (clear_thinking_20251015
    // reads as clear_thinking); the attribute keeps the full type.
    let mut names: Vec<&str> = applied_edits
        .iter()
        .map(|t| match t.rsplit_once('_') {
            Some((base, rev)) if rev.len() == 8 && rev.chars().all(|c| c.is_ascii_digit()) => base,
            _ => t.as_str(),
        })
        .collect();
    names.dedup();
    let _ = state.signal_tx.send(IngestionEvent::PipelineWarning {
        message: format!("{agent_name}: context compacted ({})", names.join(", ")),
    });
}

fn kv_str(key: &str, value: &str) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: Some(AnyValue {
            value: Some(any_value::Value::StringValue(value.to_string())),
        }),
        ..Default::default()
    }
}

fn kv_int(key: &str, value: i64) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: Some(AnyValue {
            value: Some(any_value::Value::IntValue(value)),
        }),
        ..Default::default()
    }
}

fn kv_double(key: &str, value: f64) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: Some(AnyValue {
            value: Some(any_value::Value::DoubleValue(value)),
        }),
        ..Default::default()
    }
}

fn to_millis(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn to_nanos(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
pub(crate) fn test_random_bytes(n: usize) -> Vec<u8> {
    random_bytes(n)
}

/// Unique ids without a rand dependency: a process-wide counter hashed
/// through a randomly seeded hasher, mixed with wall-clock nanos. The
/// receive stage dedups by span id, so uniqueness is what matters here;
/// these ids never leave the local machine.
fn random_bytes(n: usize) -> Vec<u8> {
    use std::hash::{BuildHasher, Hash, Hasher};
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    static SEED: OnceLock<std::collections::hash_map::RandomState> = OnceLock::new();

    let seed = SEED.get_or_init(std::collections::hash_map::RandomState::new);
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        let mut hasher = seed.build_hasher();
        COUNTER.fetch_add(1, Ordering::Relaxed).hash(&mut hasher);
        to_nanos(SystemTime::now()).hash(&mut hasher);
        out.extend_from_slice(&hasher.finish().to_le_bytes());
    }
    out.truncate(n);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::post;

    /// Spawns a mock upstream returning the given status and body, and the
    /// proxy in front of it. Returns the proxy's base URL and the pipeline
    /// receiver the proxy feeds.
    async fn spawn_proxy(
        upstream_status: u16,
        upstream_body: &'static str,
    ) -> (String, mpsc::Receiver<PipelineSpan>) {
        let (base, rx, _iv) = spawn_proxy_with_interventions(upstream_status, upstream_body).await;
        (base, rx)
    }

    async fn spawn_proxy_with_interventions(
        upstream_status: u16,
        upstream_body: &'static str,
    ) -> (String, mpsc::Receiver<PipelineSpan>, ProxyInterventions) {
        let upstream_app = axum::Router::new()
            .route(
                "/v1/messages",
                post(move || async move {
                    Response::builder()
                        .status(upstream_status)
                        .header("content-type", "application/json")
                        .body(Body::from(upstream_body))
                        .unwrap()
                }),
            )
            // A non-Messages endpoint the breaker must never touch.
            .route("/v1/messages/count_tokens", post(|| async { "{}" }));
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(upstream_listener, upstream_app).await.unwrap();
        });

        let (tx, rx) = mpsc::channel(8);
        let (signal_tx, _) = broadcast::channel(64);
        let interventions: ProxyInterventions = Arc::new(std::sync::Mutex::new(Default::default()));
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();
        drop(proxy_listener);
        tokio::spawn(run_with(
            proxy_addr,
            format!("http://{}", upstream_addr),
            None,
            std::time::Duration::from_millis(500),
            tx,
            signal_tx,
            Some(interventions.clone()),
            None,
            None,
        ));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        (format!("http://{}", proxy_addr), rx, interventions)
    }

    /// A mock upstream that speaks SSE with controllable behavior, plus
    /// the proxy in front of it. Returns the proxy URL, the pipeline
    /// receiver, and a subscription to the streaming signal.
    async fn spawn_sse_proxy(
        mode: &'static str,
    ) -> (
        String,
        mpsc::Receiver<PipelineSpan>,
        broadcast::Receiver<IngestionEvent>,
    ) {
        let upstream_app = axum::Router::new().route(
            "/v1/messages",
            post(move || async move {
                let (tx, rx) = mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(8);
                tokio::spawn(async move {
                    let start = r#"event: message_start
data: {"type":"message_start","message":{"model":"claude-opus-4-8","usage":{"input_tokens":1000,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}

"#;
                    let _ = tx.send(Ok(start.into())).await;
                    match mode {
                        "complete" => {
                            for word in ["one", "two", "three"] {
                                let delta = format!(
                                    r#"event: content_block_delta
data: {{"type":"content_block_delta","delta":{{"type":"text_delta","text":"{word} "}}}}

"#
                                );
                                let _ = tx.send(Ok(delta.into())).await;
                                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                            }
                            let tail = r#"event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":30}}

event: message_stop
data: {"type":"message_stop"}

"#;
                            let _ = tx.send(Ok(tail.into())).await;
                        }
                        "api_error" => {
                            let err = r#"event: error
data: {"type":"error","error":{"type":"overloaded_error","message":"busy"}}

"#;
                            let _ = tx.send(Ok(err.into())).await;
                        }
                        "hang" => {
                            // One delta, then silence far past the proxy's
                            // per-chunk timeout.
                            let delta = r#"event: content_block_delta
data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"then nothing"}}

"#;
                            let _ = tx.send(Ok(delta.into())).await;
                            tokio::time::sleep(std::time::Duration::from_secs(600)).await;
                        }
                        "endless" => {
                            // Chunks forever, for the client-walks-away case.
                            loop {
                                let delta = r#"event: content_block_delta
data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"more"}}

"#;
                                if tx.send(Ok(delta.into())).await.is_err() {
                                    break;
                                }
                                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                            }
                        }
                        _ => unreachable!(),
                    }
                });
                Response::builder()
                    .status(200)
                    .header("content-type", "text/event-stream")
                    .body(Body::from_stream(
                        tokio_stream::wrappers::ReceiverStream::new(rx),
                    ))
                    .unwrap()
            }),
        );
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(upstream_listener, upstream_app).await.unwrap();
        });

        let (tx, rx) = mpsc::channel(8);
        let (signal_tx, signal_rx) = broadcast::channel(256);
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();
        drop(proxy_listener);
        tokio::spawn(run_with(
            proxy_addr,
            format!("http://{}", upstream_addr),
            None,
            std::time::Duration::from_millis(500),
            tx,
            signal_tx,
            None,
            None,
            None,
        ));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        (format!("http://{}", proxy_addr), rx, signal_rx)
    }

    fn str_attr<'a>(span: &'a OtlpSpan, key: &str) -> Option<&'a str> {
        match attr(span, key) {
            Some(any_value::Value::StringValue(v)) => Some(v.as_str()),
            _ => None,
        }
    }

    fn attr<'a>(span: &'a OtlpSpan, key: &str) -> Option<&'a any_value::Value> {
        span.attributes
            .iter()
            .find(|kv| kv.key == key)
            .and_then(|kv| kv.value.as_ref())
            .and_then(|v| v.value.as_ref())
    }

    const OK_BODY: &str = r#"{
        "id": "msg_test",
        "model": "claude-opus-4-8",
        "content": [{"type": "text", "text": "hello"}],
        "usage": {"input_tokens": 1000, "output_tokens": 500,
                  "cache_read_input_tokens": 2000, "cache_creation_input_tokens": 0}
    }"#;

    #[tokio::test]
    async fn round_trip_synthesizes_a_priced_span() {
        let (base, mut rx) = spawn_proxy(200, OK_BODY).await;
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{base}/v1/messages"))
            .header("user-agent", "claude-cli/1.5.0 (external, cli)")
            .header("x-api-key", "sk-ant-SECRET")
            .header("authorization", "Bearer sk-ant-SECRET")
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["id"], "msg_test", "response passes through unchanged");

        let ps = rx.recv().await.expect("a span must be synthesized");
        assert_eq!(ps.service_name, "claude-cli", "agent named from User-Agent");
        assert_eq!(ps.integration, IntegrationPath::Proxy);
        assert_eq!(ps.span.name, "gen_ai.chat");
        match attr(&ps.span, "gen_ai.request.model") {
            Some(any_value::Value::StringValue(m)) => assert_eq!(m, "claude-opus-4-8"),
            other => panic!("model attribute missing: {other:?}"),
        }
        // Opus: 1000 in ($0.005) + 500 out ($0.0125) + 2000 cache reads
        // ($0.000005/tok * 0.1 * 2000 = $0.001) = $0.0185.
        match attr(&ps.span, "gen_ai.usage.cost") {
            Some(any_value::Value::DoubleValue(c)) => assert!((c - 0.0185).abs() < 1e-9),
            other => panic!("cost attribute missing: {other:?}"),
        }
        assert_eq!(ps.span.status.as_ref().map(|s| s.code), Some(1));
    }

    #[tokio::test]
    async fn accept_encoding_never_reaches_the_upstream() {
        // An upstream that answers compressed when invited would blind
        // the tee (the real API does exactly that; caught by the first
        // Claude Code dogfood run). This one refuses the invitation
        // outright, so the test fails loudly if the header ever leaks
        // through again.
        let upstream_app = axum::Router::new().route(
            "/v1/messages",
            post(|headers: HeaderMap| async move {
                if headers.contains_key(axum::http::header::ACCEPT_ENCODING) {
                    return Response::builder()
                        .status(500)
                        .body(Body::from("accept-encoding leaked to upstream"))
                        .unwrap();
                }
                Response::builder()
                    .status(200)
                    .header("content-type", "application/json")
                    .body(Body::from(OK_BODY))
                    .unwrap()
            }),
        );
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(upstream_listener, upstream_app).await.unwrap();
        });

        let (tx, mut rx) = mpsc::channel(8);
        let (signal_tx, _) = broadcast::channel(64);
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();
        drop(proxy_listener);
        tokio::spawn(run_with(
            proxy_addr,
            format!("http://{}", upstream_addr),
            None,
            std::time::Duration::from_millis(500),
            tx,
            signal_tx,
            None,
            None,
            None,
        ));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let resp = reqwest::Client::new()
            .post(format!("http://{proxy_addr}/v1/messages"))
            .header("user-agent", "claude-cli/2.0.0")
            .header("accept-encoding", "gzip, deflate, br, zstd")
            .body(r#"{"model":"claude-opus-4-8","messages":[{"role":"user","content":"hi"}]}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "upstream must never see the header");

        let ps = rx.recv().await.expect("span synthesized");
        match attr(&ps.span, "gen_ai.request.model") {
            Some(any_value::Value::StringValue(m)) => assert_eq!(m, "claude-opus-4-8"),
            other => panic!("a readable response must price the span: {other:?}"),
        }
    }

    #[tokio::test]
    async fn placement_opens_the_turn_and_the_root_closes_it() {
        // The #200 wiring: every placed request marks its turn open in
        // the shared map (holding the idle timeout across client-side
        // tool gaps), and the turn root retires the mark.
        const TOOL_USE_BODY: &str = r#"{
            "id": "msg_t", "model": "claude-opus-4-8",
            "stop_reason": "tool_use",
            "content": [{"type": "tool_use", "id": "t1", "name": "bash",
                         "input": {}}],
            "usage": {"input_tokens": 10, "output_tokens": 5}
        }"#;
        let upstream_app = axum::Router::new().route(
            "/v1/messages",
            post(move |body: axum::body::Bytes| async move {
                let req: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let n = req["messages"].as_array().unwrap().len();
                let payload = if n == 1 { TOOL_USE_BODY } else { OK_BODY };
                Response::builder()
                    .status(200)
                    .header("content-type", "application/json")
                    .body(Body::from(payload))
                    .unwrap()
            }),
        );
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(upstream_listener, upstream_app).await.unwrap();
        });

        let (tx, mut rx) = mpsc::channel(8);
        let (signal_tx, _) = broadcast::channel(64);
        let open_turns: crate::assemble::OpenTurns =
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();
        drop(proxy_listener);
        tokio::spawn(run_with(
            proxy_addr,
            format!("http://{}", upstream_addr),
            None,
            std::time::Duration::from_millis(500),
            tx,
            signal_tx,
            None,
            None,
            Some(open_turns.clone()),
        ));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let client = reqwest::Client::new();
        client
            .post(format!("http://{proxy_addr}/v1/messages"))
            .header("user-agent", "claude-cli/2.0.0")
            .body(r#"{"model":"claude-opus-4-8","messages":[{"role":"user","content":"go"}]}"#)
            .send()
            .await
            .unwrap();
        let _first_chat = rx.recv().await.expect("first chat span");
        assert_eq!(
            open_turns.lock().unwrap().len(),
            1,
            "a tool_use response leaves the turn open and marked"
        );

        client
            .post(format!("http://{proxy_addr}/v1/messages"))
            .header("user-agent", "claude-cli/2.0.0")
            .body(
                r#"{"model":"claude-opus-4-8","messages":[
                    {"role":"user","content":"go"},
                    {"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"bash","input":{}}]},
                    {"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}
                ]}"#,
            )
            .send()
            .await
            .unwrap();
        // tool span, second chat, then the turn root.
        let _tool = rx.recv().await.expect("tool span");
        let _chat = rx.recv().await.expect("second chat");
        let root = rx.recv().await.expect("turn root");
        assert!(root.span.name.starts_with("agent.turn"));
        assert!(
            open_turns.lock().unwrap().is_empty(),
            "the root retires the open-turn mark"
        );
    }

    #[tokio::test]
    async fn tool_spans_carry_the_clean_tool_name() {
        const TOOL_USE_BODY: &str = r#"{
            "id": "msg_tool",
            "model": "claude-opus-4-8",
            "stop_reason": "tool_use",
            "content": [{"type": "tool_use", "id": "toolu_T1", "name": "bash",
                         "input": {"command": "ls"}}],
            "usage": {"input_tokens": 100, "output_tokens": 10}
        }"#;
        let (base, mut rx) = spawn_proxy(200, TOOL_USE_BODY).await;
        let client = reqwest::Client::new();

        client
            .post(format!("{base}/v1/messages"))
            .header("user-agent", "claude-cli/2.0.0")
            .body(r#"{"model":"claude-opus-4-8","messages":[{"role":"user","content":"ls"}]}"#)
            .send()
            .await
            .unwrap();
        let _chat1 = rx.recv().await.expect("first chat span");

        client
            .post(format!("{base}/v1/messages"))
            .header("user-agent", "claude-cli/2.0.0")
            .body(
                r#"{"model":"claude-opus-4-8","messages":[
                    {"role":"user","content":"ls"},
                    {"role":"assistant","content":[{"type":"tool_use","id":"toolu_T1","name":"bash","input":{"command":"ls"}}]},
                    {"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_T1","content":"a.txt"}]}
                ]}"#,
            )
            .send()
            .await
            .unwrap();

        let tool = rx.recv().await.expect("tool span");
        assert_eq!(tool.span.name, "gen_ai.tool:bash");
        // The judge prefers this attribute over the raw operation name;
        // it must survive from here through normalization (which has its
        // own whitelist test) to reach the prompt as [bash], not
        // [gen_ai.tool:bash].
        match attr(&tool.span, "gen_ai.tool.name") {
            Some(any_value::Value::StringValue(n)) => assert_eq!(n, "bash"),
            other => panic!("gen_ai.tool.name missing on tool span: {other:?}"),
        }
    }

    #[tokio::test]
    async fn cache_tokens_land_as_attributes_with_net_savings() {
        const CACHE_BODY: &str = r#"{
            "id": "msg_cache",
            "model": "claude-opus-4-8",
            "content": [{"type": "text", "text": "hello"}],
            "usage": {"input_tokens": 1000, "output_tokens": 500,
                      "cache_read_input_tokens": 2000,
                      "cache_creation_input_tokens": 1000}
        }"#;
        let (base, mut rx) = spawn_proxy(200, CACHE_BODY).await;
        reqwest::Client::new()
            .post(format!("{base}/v1/messages"))
            .header("user-agent", "claude-cli/1.5.0")
            .body("{}")
            .send()
            .await
            .unwrap();

        let ps = rx.recv().await.expect("a span must be synthesized");
        match attr(&ps.span, "gen_ai.usage.cache_read_tokens") {
            Some(any_value::Value::IntValue(n)) => assert_eq!(*n, 2000),
            other => panic!("cache_read_tokens missing: {other:?}"),
        }
        match attr(&ps.span, "gen_ai.usage.cache_creation_tokens") {
            Some(any_value::Value::IntValue(n)) => assert_eq!(*n, 1000),
            other => panic!("cache_creation_tokens missing: {other:?}"),
        }
        // Opus input $5/MTok: 2000 reads save $0.009 (0.9 factor), 1000
        // writes cost an extra $0.00125 (0.25 premium). Net $0.00775.
        match attr(&ps.span, "gen_ai.usage.cache_saved") {
            Some(any_value::Value::DoubleValue(s)) => assert!((s - 0.00775).abs() < 1e-9),
            other => panic!("cache_saved missing: {other:?}"),
        }
    }

    #[tokio::test]
    async fn api_key_never_reaches_the_span() {
        let (base, mut rx) = spawn_proxy(200, OK_BODY).await;
        reqwest::Client::new()
            .post(format!("{base}/v1/messages"))
            .header("x-api-key", "sk-ant-SECRET-VALUE")
            .header("authorization", "Bearer sk-ant-SECRET-VALUE")
            .body("{}")
            .send()
            .await
            .unwrap();
        let ps = rx.recv().await.unwrap();
        let serialized = format!("{:?}", ps.span);
        assert!(
            !serialized.contains("SECRET-VALUE"),
            "no synthesized attribute may carry credential material"
        );
    }

    #[tokio::test]
    async fn upstream_failure_synthesizes_a_failed_span() {
        let (base, mut rx) = spawn_proxy(
            429,
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#,
        )
        .await;
        let resp = reqwest::Client::new()
            .post(format!("{base}/v1/messages"))
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 429, "error forwarded unchanged");

        let ps = rx.recv().await.expect("failures synthesize spans too");
        assert_eq!(
            ps.span.status.as_ref().map(|s| s.code),
            Some(2),
            "an upstream 429 renders as a failed span"
        );
        match attr(&ps.span, "http.response.status_code") {
            Some(any_value::Value::IntValue(code)) => assert_eq!(*code, 429),
            other => panic!("status attribute missing: {other:?}"),
        }
    }

    #[tokio::test]
    async fn non_messages_paths_pass_through_without_spans() {
        let (base, mut rx) = spawn_proxy(200, OK_BODY).await;
        // The mock upstream only routes /v1/messages; anything else 404s,
        // which is fine: the assertion is that no span is synthesized.
        let _ = reqwest::Client::new()
            .post(format!("{base}/v1/complete"))
            .body("{}")
            .send()
            .await
            .unwrap();
        assert!(
            rx.try_recv().is_err(),
            "only Messages API round trips synthesize spans"
        );
    }

    #[test]
    fn agent_name_derivation_handles_the_edge_cases() {
        let mut headers = HeaderMap::new();
        headers.insert("user-agent", "claude-cli/1.5.0 (external)".parse().unwrap());
        assert_eq!(derive_agent_name(&headers), "claude-cli");

        headers.insert("user-agent", "curl/8.5.0".parse().unwrap());
        assert_eq!(derive_agent_name(&headers), "curl");

        assert_eq!(derive_agent_name(&HeaderMap::new()), "proxy-agent");
    }

    #[tokio::test]
    async fn streaming_round_trip_synthesizes_and_emits_live_updates() {
        let (base, mut rx, mut signal_rx) = spawn_sse_proxy("complete").await;
        let resp = reqwest::Client::new()
            .post(format!("{base}/v1/messages"))
            .header("user-agent", "claude-cli/2.0.0")
            .body(r#"{"stream":true}"#)
            .send()
            .await
            .unwrap();
        let body = resp.text().await.unwrap();
        assert!(
            body.contains("one ") && body.contains("message_stop"),
            "SSE passes through verbatim"
        );

        let ps = rx.recv().await.expect("stream must finalize a span");
        assert_eq!(
            str_attr(&ps.span, "reeve.proxy.stream_outcome"),
            Some("completed")
        );
        assert_eq!(
            str_attr(&ps.span, "gen_ai.request.model"),
            Some("claude-opus-4-8")
        );
        assert!(
            attr(&ps.span, "reeve.proxy.ttft_ms").is_some(),
            "TTFT recorded"
        );
        // Opus: 1000 in + 30 out = 0.005 + 0.00075.
        match attr(&ps.span, "gen_ai.usage.cost") {
            Some(any_value::Value::DoubleValue(c)) => assert!((c - 0.00575).abs() < 1e-9),
            other => panic!("cost missing: {other:?}"),
        }
        assert_eq!(ps.span.status.as_ref().map(|s| s.code), Some(1));

        // The streaming box producer: accumulated content grows, each
        // update names its agent and carries a running cost estimate for
        // the header ticker.
        let mut last = String::new();
        let mut last_cost = None;
        let mut agent_id = None;
        while let Ok(ev) = signal_rx.try_recv() {
            if let IngestionEvent::StreamingUpdate {
                content,
                cost_so_far,
                agent_id: aid,
                ..
            } = ev
            {
                last = content;
                last_cost = cost_so_far;
                agent_id = Some(aid);
            }
        }
        assert_eq!(last, "one two three ", "live updates accumulate the text");
        assert_eq!(
            agent_id,
            Some(reeve_model::ids::agent_id_from_service(
                "claude-cli",
                "proxy"
            )),
            "the update names the agent the header will tick for"
        );
        // Opus, 1000 input tokens known from message_start: the running
        // estimate is at least the committed input cost ($0.005) and no
        // more than the final priced cost.
        let cost = last_cost.expect("a priced model yields a running estimate");
        assert!(
            (0.005..=0.00575).contains(&cost),
            "running estimate stays between committed input cost and final: {cost}"
        );
    }

    #[tokio::test]
    async fn upstream_error_event_finalizes_as_api_failed() {
        let (base, mut rx, _sig) = spawn_sse_proxy("api_error").await;
        let resp = reqwest::Client::new()
            .post(format!("{base}/v1/messages"))
            .body("{}")
            .send()
            .await
            .unwrap();
        let body = resp.text().await.unwrap();
        assert!(
            body.contains("overloaded_error"),
            "error forwarded to the client unchanged"
        );
        let ps = rx.recv().await.unwrap();
        assert_eq!(
            str_attr(&ps.span, "reeve.proxy.stream_outcome"),
            Some("api_failed")
        );
        assert_eq!(ps.span.status.as_ref().map(|s| s.code), Some(2));
    }

    #[tokio::test]
    async fn silent_stream_finalizes_as_timed_out() {
        let (base, mut rx, _sig) = spawn_sse_proxy("hang").await;
        let client = reqwest::Client::new();
        let handle = tokio::spawn(async move {
            let resp = client
                .post(format!("{base}/v1/messages"))
                .body("{}")
                .send()
                .await
                .unwrap();
            let _ = resp.text().await;
        });
        let ps = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("span must finalize within the chunk timeout")
            .unwrap();
        assert_eq!(
            str_attr(&ps.span, "reeve.proxy.stream_outcome"),
            Some("stream_timed_out")
        );
        assert_eq!(ps.span.status.as_ref().map(|s| s.code), Some(2));
        handle.abort();
    }

    #[tokio::test]
    async fn client_walking_away_finalizes_without_failure() {
        let (base, mut rx, _sig) = spawn_sse_proxy("endless").await;
        let resp = reqwest::Client::new()
            .post(format!("{base}/v1/messages"))
            .body("{}")
            .send()
            .await
            .unwrap();
        // Read a little, then hang up mid-generation.
        let mut stream = resp.bytes_stream();
        let _ = stream.next().await;
        drop(stream);

        let ps = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("disconnect must finalize the span")
            .unwrap();
        assert_eq!(
            str_attr(&ps.span, "reeve.proxy.stream_outcome"),
            Some("client_disconnected")
        );
        assert_eq!(
            ps.span.status.as_ref().map(|s| s.code),
            Some(1),
            "closing a tool is behavior, not breakage"
        );
    }

    #[tokio::test]
    async fn queued_intervention_applies_on_the_next_request() {
        let (base, _rx, interventions) = spawn_proxy_with_interventions(200, OK_BODY).await;
        let agent_id = reeve_model::ids::agent_id_from_service("claude-cli", "proxy");
        interventions
            .lock()
            .unwrap()
            .pending
            .entry(agent_id.clone())
            .or_default()
            .push_back(reeve_model::entity::ProxyCommand {
                id: "cmd-1".into(),
                payload: ProxyPayload::Redirect {
                    instruction: "focus on the tests".to_string(),
                },
                valid_until_ms: i64::MAX,
            });

        reqwest::Client::new()
            .post(format!("{base}/v1/messages"))
            .header("user-agent", "claude-cli/2.0.0")
            .body(r#"{"model":"claude-opus-4-8","messages":[{"role":"user","content":"hi"}]}"#)
            .send()
            .await
            .unwrap();

        let q = interventions.lock().unwrap();
        assert!(
            q.pending.get(&agent_id).is_none_or(|d| d.is_empty()),
            "the queue drains on the next request"
        );
        assert_eq!(q.applied.len(), 1, "the application is reported back");
        assert_eq!(q.applied[0].0, reeve_model::ids::CommandId::from("cmd-1"));
    }

    #[tokio::test]
    async fn expired_intervention_drops_instead_of_applying() {
        let (base, _rx, interventions) = spawn_proxy_with_interventions(200, OK_BODY).await;
        let agent_id = reeve_model::ids::agent_id_from_service("claude-cli", "proxy");
        interventions
            .lock()
            .unwrap()
            .pending
            .entry(agent_id.clone())
            .or_default()
            .push_back(reeve_model::entity::ProxyCommand {
                id: "cmd-old".into(),
                payload: ProxyPayload::InjectContext {
                    context: "too late".to_string(),
                },
                valid_until_ms: 1,
            });

        reqwest::Client::new()
            .post(format!("{base}/v1/messages"))
            .header("user-agent", "claude-cli/2.0.0")
            .body(r#"{"model":"claude-opus-4-8","messages":[{"role":"user","content":"hi"}]}"#)
            .send()
            .await
            .unwrap();

        let q = interventions.lock().unwrap();
        assert!(q.applied.is_empty(), "an expired command never applies");
    }

    #[tokio::test]
    async fn intervention_does_not_disturb_threading() {
        let (base, mut rx, interventions) = spawn_proxy_with_interventions(200, OK_BODY).await;
        let agent_id = reeve_model::ids::agent_id_from_service("claude-cli", "proxy");
        let client = reqwest::Client::new();

        // Request 1 establishes the conversation.
        client
            .post(format!("{base}/v1/messages"))
            .header("user-agent", "claude-cli/2.0.0")
            .body(r#"{"model":"claude-opus-4-8","messages":[{"role":"user","content":"start"}]}"#)
            .send()
            .await
            .unwrap();
        let first = rx.recv().await.unwrap();

        // Queue an intervention; request 2 extends the ORIGINAL history.
        interventions
            .lock()
            .unwrap()
            .pending
            .entry(agent_id)
            .or_default()
            .push_back(reeve_model::entity::ProxyCommand {
                id: "cmd-2".into(),
                payload: ProxyPayload::Redirect {
                    instruction: "change course".to_string(),
                },
                valid_until_ms: i64::MAX,
            });
        client
            .post(format!("{base}/v1/messages"))
            .header("user-agent", "claude-cli/2.0.0")
            .body(r#"{"model":"claude-opus-4-8","messages":[{"role":"user","content":"start"},{"role":"assistant","content":"ok"},{"role":"user","content":"next"}]}"#)
            .send()
            .await
            .unwrap();
        // Skip the turn root emitted between the chat spans if present.
        let mut second = rx.recv().await.unwrap();
        while second.span.name != "gen_ai.chat" {
            second = rx.recv().await.unwrap();
        }
        // OK_BODY has no stop_reason, so each request ends its own turn:
        // traces differ, but both requests threaded the same conversation,
        // which the message_count attr proves (3 = original, not 4).
        match attr(&second.span, "reeve.proxy.context_messages") {
            Some(any_value::Value::IntValue(n)) => assert_eq!(
                *n, 3,
                "threading fingerprinted the original body, not the injected one"
            ),
            other => panic!("context attr missing: {other:?}"),
        }
        let _ = first;
    }

    #[tokio::test]
    async fn engaged_breaker_refuses_messages_requests() {
        let (base, _rx, interventions) = spawn_proxy_with_interventions(200, OK_BODY).await;
        let agent_id = reeve_model::ids::agent_id_from_service("claude-cli", "proxy");
        let client = reqwest::Client::new();

        // Before the kill: requests flow.
        let ok = client
            .post(format!("{base}/v1/messages"))
            .header("user-agent", "claude-cli/2.0.0")
            .body(r#"{"model":"claude-opus-4-8","messages":[{"role":"user","content":"hi"}]}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), 200);

        interventions.lock().unwrap().killed.insert(agent_id);

        // After: refused with a clean API error naming the operator kill.
        let refused = client
            .post(format!("{base}/v1/messages"))
            .header("user-agent", "claude-cli/2.0.0")
            .body(r#"{"model":"claude-opus-4-8","messages":[{"role":"user","content":"hi"}]}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(refused.status(), 403);
        let body = refused.text().await.unwrap();
        assert!(body.contains("killed this agent via Reeve"));

        // A different agent through the same proxy is untouched.
        let other = client
            .post(format!("{base}/v1/messages"))
            .header("user-agent", "other-tool/1.0")
            .body(r#"{"model":"claude-opus-4-8","messages":[{"role":"user","content":"hi"}]}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(other.status(), 200, "other agents keep flowing");

        // The breaker cuts the money path only. Token counting is not
        // where tokens are spent, so a killed agent's count_tokens call
        // still reaches the upstream and succeeds; blocking it would break
        // clients for nothing.
        let count = client
            .post(format!("{base}/v1/messages/count_tokens"))
            .header("user-agent", "claude-cli/2.0.0")
            .body(r#"{"model":"claude-opus-4-8","messages":[{"role":"user","content":"hi"}]}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(
            count.status(),
            200,
            "the breaker is messages-only; count_tokens must still forward"
        );
    }

    #[test]
    fn synthesized_ids_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..10_000 {
            assert!(seen.insert(random_bytes(8)), "span ids must never collide");
        }
    }
}
