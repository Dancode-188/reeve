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
use std::collections::{HashMap, HashSet};
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
    /// Refuse requests carrying a detected secret instead of forwarding
    /// them. Off by default: warn-first, because a false positive that
    /// blocks legitimate traffic destroys trust in the whole feature.
    secrets_block: bool,
    /// Fingerprints of secrets already alerted, per agent. The replayed
    /// history re-sends every secret on every round trip; each one
    /// speaks in ALERTS once.
    seen_secrets: std::sync::Mutex<HashMap<reeve_model::ids::AgentId, HashSet<u64>>>,
    /// Where round trips are stored under privacy tier 2. `None` at
    /// tier 1, and its absence is what keeps the parsed request body
    /// from being retained at all.
    capture: Option<Arc<crate::capture::Capture>>,
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

/// Where the proxy forwards to, overridable so verification runs can
/// point it at a mock and never touch the real API.
pub fn upstream_from_env() -> String {
    std::env::var("REEVE_PROXY_UPSTREAM").unwrap_or_else(|_| DEFAULT_UPSTREAM.to_string())
}

/// Overrides User-Agent derivation when set.
pub fn agent_name_override_from_env() -> Option<String> {
    std::env::var("REEVE_PROXY_AGENT_NAME").ok()
}

/// A stream that goes silent for this long is dead.
pub const DEFAULT_STREAM_CHUNK_TIMEOUT: std::time::Duration =
    std::time::Duration::from_millis(DEFAULT_STREAM_CHUNK_TIMEOUT_MS);

/// Everything the proxy is handed from outside: where to forward, how it
/// is wired into the pipeline, and the shared state it coordinates
/// through. `ProxyState` adds the three pieces it builds for itself.
///
/// A struct rather than ten arguments because the tail of those ten
/// read `None, None, false` at every call site, and nothing there said
/// which `None` was which.
pub struct ProxyConfig {
    pub upstream: String,
    /// Overrides User-Agent derivation when set (REEVE_PROXY_AGENT_NAME).
    pub agent_name_override: Option<String>,
    pub stream_chunk_timeout: std::time::Duration,
    pub pipeline_tx: mpsc::Sender<PipelineSpan>,
    pub signal_tx: broadcast::Sender<IngestionEvent>,
    pub interventions: Option<ProxyInterventions>,
    pub active_streams: Option<crate::assemble::ActiveStreams>,
    pub open_turns: Option<crate::assemble::OpenTurns>,
    pub secrets_block: bool,
    /// Where round trips are stored. `Some` only at privacy tier 2, and
    /// the proxy reads it as the single switch for whether a request
    /// body is retained past the moment it is threaded.
    pub capture: Option<Arc<crate::capture::Capture>>,
}

pub async fn run_with(
    addr: SocketAddr,
    config: ProxyConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let state = Arc::new(ProxyState {
        client: reqwest::Client::new(),
        upstream: config.upstream,
        pipeline_tx: config.pipeline_tx,
        signal_tx: config.signal_tx,
        agent_name_override: config.agent_name_override,
        stream_chunk_timeout: config.stream_chunk_timeout,
        tracker: std::sync::Mutex::new(ConversationTracker::default()),
        interventions: config.interventions,
        active_streams: config.active_streams,
        open_turns: config.open_turns,
        secrets_block: config.secrets_block,
        seen_secrets: std::sync::Mutex::new(HashMap::new()),
        capture: config.capture,
    });

    // The secret patterns compile here, not inside the first request's
    // measured overhead.
    crate::secrets::warm();

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
    // Outbound secret scan, before the bytes leave the machine. The
    // scan is in memory on text already passing through; only the kind,
    // a redacted hint, and a fingerprint survive. New findings alert
    // once per agent; in block mode ANY finding refuses the request,
    // because the replayed history re-leaks a seen secret on every
    // round trip, not just the first.
    let mut secret_kinds: Vec<&'static str> = Vec::new();
    if method == Method::POST && uri.path().ends_with("/v1/messages") {
        if let Ok(text) = std::str::from_utf8(&body) {
            let findings = crate::secrets::scan(text);
            if !findings.is_empty() {
                let agent_id = reeve_model::ids::agent_id_from_service(&agent_name, "proxy");
                let new: Vec<_> = {
                    let mut seen = state.seen_secrets.lock().expect("secrets mutex poisoned");
                    let agent_seen = seen.entry(agent_id).or_default();
                    findings
                        .iter()
                        .filter(|f| agent_seen.insert(f.fingerprint))
                        .collect()
                };
                if !new.is_empty() {
                    let listed = new
                        .iter()
                        .map(|f| format!("{} {}", f.kind, f.hint))
                        .collect::<Vec<_>>()
                        .join(", ");
                    tracing::warn!(agent = %agent_name, findings = %listed, "outbound secret detected");
                    let _ = state.signal_tx.send(IngestionEvent::PipelineWarning {
                        message: format!("{agent_name}: outbound secret detected ({listed})"),
                    });
                }
                secret_kinds = new.iter().map(|f| f.kind).collect();
                if state.secrets_block {
                    tracing::warn!(agent = %agent_name, "request refused: outbound secret (block mode)");
                    return Response::builder()
                        .status(StatusCode::FORBIDDEN)
                        .header("content-type", "application/json")
                        .body(Body::from(
                            "{\"type\":\"error\",\"error\":{\"type\":\"permission_error\",\"message\":\"Reeve refused this request: the body contains what looks like a secret, and secret blocking is enabled\"}}",
                        ))
                        .expect("static response construction cannot fail");
                }
            }
        }
    }
    // Parsed once and held: threading reads `messages`, the attributes
    // below read the model, tools and cache breakpoints, and capture
    // stores the body whole.
    let is_messages = method == Method::POST && uri.path().ends_with("/v1/messages");
    let req_json = is_messages
        .then(|| serde_json::from_slice::<serde_json::Value>(&body).ok())
        .flatten();
    let placement = req_json.as_ref().and_then(|json| {
        let messages = json.get("messages")?.as_array()?;
        Some(
            state
                .tracker
                .lock()
                .expect("tracker mutex poisoned")
                .place_request(&agent_name, messages, arrived, random_bytes),
        )
    });
    // Shape of the request, not its content: which model was asked for,
    // how many tools were offered, where the cache breakpoints sit. These
    // are stamped at every privacy tier, because none of them is anything
    // the developer wrote.
    let request_attrs = req_json
        .as_ref()
        .map(request_attributes)
        .unwrap_or_default();
    // The body itself survives only under capture. At tier 1 the parsed
    // tree is dropped right here rather than held for the lifetime of a
    // stream that may run for minutes, and that drop is the whole of the
    // tier's enforcement.
    let captured_request = match state.capture {
        Some(_) => req_json,
        None => None,
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

    // Assembled before the forward rather than after it, so the two
    // failure paths below have something to speak through.
    let mut span_ctx = SpanContext {
        state: state.clone(),
        agent_name,
        placement,
        arrived,
        overhead_ms,
        secret_kinds,
        request_attrs,
        request: captured_request,
        ratelimit: Vec::new(),
    };

    // From here until the span is synthesized, the turn's trace must not
    // be called idle no matter how long the model takes: the assembler's
    // timeout once flushed mid-turn and dropped a session's spans (#182).
    if let Some(ref p) = span_ctx.placement {
        mark_stream(&state, &p.trace_id, 1);
    }

    let upstream_resp = match req.body(body.clone()).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "proxy could not reach upstream");
            let trace_id = span_ctx.placement.as_ref().map(|p| p.trace_id.clone());
            if is_messages {
                synthesize_fault_span(span_ctx, "upstream_unreachable", &e.to_string()).await;
            }
            if let Some(tid) = trace_id {
                mark_stream(&state, &tid, -1);
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

    // Rate-limit headers say how close this agent is to its ceiling and
    // when the window resets. They exist only on the response, which is
    // why they are filled in here and not at construction.
    span_ctx.ratelimit = response_header_attributes(&resp_headers);

    if streaming {
        let body = stream_and_accumulate(span_ctx, upstream_resp);
        return build_response(status, &resp_headers, body);
    }

    let resp_body = match upstream_resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "proxy failed reading upstream response");
            let trace_id = span_ctx.placement.as_ref().map(|p| p.trace_id.clone());
            if is_messages {
                synthesize_fault_span(span_ctx, "response_read_failed", &e.to_string()).await;
            }
            if let Some(tid) = trace_id {
                mark_stream(&state, &tid, -1);
            }
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from("upstream response read failed"))
                .expect("static response construction cannot fail");
        }
    };

    // Unmark only after the span has entered the pipeline: the trace
    // stays exempt from the idle timeout until its evidence is in.
    let placement_trace_id = span_ctx.placement.as_ref().map(|p| p.trace_id.clone());
    if is_messages {
        synthesize_span(span_ctx, &resp_body, status.as_u16()).await;
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

/// What every span this proxy emits needs to know about the request that
/// produced it.
///
/// Owned rather than borrowed because the streaming path moves the whole
/// thing into a spawned task that outlives the request handler.
struct SpanContext {
    state: Arc<ProxyState>,
    agent_name: String,
    placement: Option<TurnPlacement>,
    arrived: SystemTime,
    overhead_ms: f64,
    secret_kinds: Vec<&'static str>,
    /// What the request asked for, as span attributes. Metadata only, so
    /// this is filled at every privacy tier.
    request_attrs: Vec<KeyValue>,
    /// The parsed request body, retained for capture and `None` at any
    /// tier below 2. Taken by the finalizers rather than cloned: a real
    /// one is over a megabyte.
    request: Option<serde_json::Value>,
    /// Rate-limit state read off the response headers.
    ratelimit: Vec<KeyValue>,
}

/// The identity a streamed span will carry, generated when the stream
/// opens rather than when it closes, so the trace exists before the first
/// chunk lands.
struct SpanIds {
    trace_id: Vec<u8>,
    parent_span_id: Vec<u8>,
    span_id: Vec<u8>,
}

/// Forwards SSE chunks to the client the moment they arrive while a side
/// accumulator reconstructs the round trip. Chunks go client-first: the
/// send happens before the parse, so the proxy adds no latency the
/// client can observe. Emits StreamingUpdate per text delta so the
/// cockpit's streaming box renders the generation live, and finalizes a
/// span through every exit path.
fn stream_and_accumulate(ctx: SpanContext, upstream_resp: reqwest::Response) -> Body {
    let (body_tx, body_rx) = mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(32);

    tokio::spawn(async move {
        let mut upstream = upstream_resp.bytes_stream();
        let mut acc = SseAccumulator::default();
        let (trace_id, parent_span_id) = match &ctx.placement {
            Some(p) => (p.trace_id.clone(), p.root_span_id.clone()),
            None => (random_bytes(16), Vec::new()),
        };
        let ids = SpanIds {
            trace_id,
            parent_span_id,
            span_id: random_bytes(8),
        };
        let span_id_hex = hex(&ids.span_id);
        let trace_id_hex = hex(&ids.trace_id);
        let mut first_chunk_at: Option<SystemTime> = None;
        let mut outcome = StreamOutcome::Completed;

        loop {
            let next = tokio::time::timeout(ctx.state.stream_chunk_timeout, upstream.next()).await;
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
                let _ = ctx.state.signal_tx.send(IngestionEvent::StreamingUpdate {
                    trace_id: trace_id_hex.clone().into(),
                    span_id: span_id_hex.clone().into(),
                    agent_id: reeve_model::ids::agent_id_from_service(&ctx.agent_name, "proxy"),
                    content: acc.content.clone(),
                    cost_so_far,
                });
            }
        }
        drop(body_tx);

        let ttft_ms = first_chunk_at.and_then(|t| {
            t.duration_since(ctx.arrived)
                .ok()
                .map(|d| d.as_secs_f64() * 1e3)
        });
        // The idle exemption holds through every stream outcome; drop it
        // only once the finalized span has entered the pipeline. Both the
        // trace id and the handle to state are taken before the context
        // moves into the finalizer, which consumes it so that a stored
        // request body is moved out rather than copied.
        let placement_trace_id = ctx.placement.as_ref().map(|p| p.trace_id.clone());
        let state = ctx.state.clone();
        finalize_stream_span(ctx, &ids, acc, outcome, ttft_ms).await;
        if let Some(tid) = placement_trace_id {
            mark_stream(&state, &tid, -1);
        }
    });

    Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(body_rx))
}

/// The streaming counterpart of synthesize_span: same shape of span,
/// built from the accumulator, plus the outcome and time-to-first-token.
async fn finalize_stream_span(
    mut ctx: SpanContext,
    ids: &SpanIds,
    acc: SseAccumulator,
    outcome: StreamOutcome,
    ttft_ms: Option<f64>,
) {
    let model = acc.model.unwrap_or_else(|| "unknown".to_string());
    let mut attributes = vec![
        kv_str("gen_ai.provider.name", "anthropic"),
        kv_str("gen_ai.operation.name", "chat"),
        kv_str("gen_ai.request.model", &model),
        kv_int("gen_ai.usage.input_tokens", acc.input_tokens as i64),
        kv_int("gen_ai.usage.output_tokens", acc.output_tokens as i64),
        kv_int(
            "gen_ai.usage.total_tokens",
            (acc.input_tokens + acc.output_tokens) as i64,
        ),
        kv_str("reeve.proxy.stream_outcome", outcome.label()),
        kv_double("reeve.proxy.overhead_ms", ctx.overhead_ms),
    ];
    if let Some(ttft) = ttft_ms {
        attributes.push(kv_double("reeve.proxy.ttft_ms", ttft));
    }
    if let Some(ref p) = ctx.placement {
        attributes.extend(threading_attributes(p));
    }
    // Moved rather than cloned: nothing downstream of here reads the
    // context's own copies again.
    attributes.append(&mut ctx.request_attrs);
    attributes.append(&mut ctx.ratelimit);
    if let Some(ref reason) = acc.stop_reason {
        attributes.push(kv_str("gen_ai.response.finish_reason", reason));
    }
    if acc.thinking_tokens > 0 {
        attributes.push(kv_int(
            "gen_ai.usage.reasoning.output_tokens",
            acc.thinking_tokens as i64,
        ));
    }
    surface_compaction(
        &ctx.state,
        &ctx.agent_name,
        &acc.applied_edits,
        &mut attributes,
    );
    stamp_secret_findings(&ctx.secret_kinds, &mut attributes);
    if acc.cache_read_tokens > 0 {
        attributes.push(kv_int(
            "gen_ai.usage.cache_read.input_tokens",
            acc.cache_read_tokens as i64,
        ));
    }
    if acc.cache_creation_tokens > 0 {
        attributes.push(kv_int(
            "gen_ai.usage.cache_creation.input_tokens",
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

    // Tier 2 only: the round trip goes to the sidecar corpus under the
    // same ids the span carries, which is the whole of the join between
    // them. A stream has no single response body, so one is rebuilt from
    // the accumulator, reasoning kept apart from the answer because it is
    // a different thing to read.
    if let (Some(capture), Some(request)) = (ctx.state.capture.clone(), ctx.request.take()) {
        capture.record(crate::capture::Round {
            span_id: hex(&ids.span_id),
            trace_id: hex(&ids.trace_id),
            agent: ctx.agent_name.clone(),
            started_at_ms: to_millis(ctx.arrived),
            ended_at_ms: to_millis(ended),
            request,
            message_hashes: ctx
                .placement
                .as_ref()
                .map(|p| p.message_hashes.clone())
                .unwrap_or_default(),
            response: serde_json::json!({
                "model": &model,
                "content": &acc.content,
                "thinking": &acc.thinking,
                "stop_reason": &acc.stop_reason,
                "tool_uses": &acc.tool_uses,
                "stream_outcome": outcome.label(),
                "usage": {
                    "input_tokens": acc.input_tokens,
                    "output_tokens": acc.output_tokens,
                    "cache_read_input_tokens": acc.cache_read_tokens,
                    "cache_creation_input_tokens": acc.cache_creation_tokens,
                    "thinking_tokens": acc.thinking_tokens,
                },
            }),
        });
    }

    let span = OtlpSpan {
        trace_id: ids.trace_id.clone(),
        span_id: ids.span_id.clone(),
        parent_span_id: ids.parent_span_id.clone(),
        name: "gen_ai.chat".to_string(),
        start_time_unix_nano: to_nanos(ctx.arrived),
        end_time_unix_nano: to_nanos(ended),
        attributes,
        status: Some(OtlpStatus {
            code: status_code,
            message: String::new(),
        }),
        ..Default::default()
    };
    emit_pipeline_span(&ctx.state, &ctx.agent_name, span, ctx.arrived).await;

    // A dead stream still ends its turn: whatever the outcome, the
    // assistant is not going to request more tools on this round trip, so
    // an outcome other than tool_use closes the turn honestly.
    let stop_reason = match outcome {
        StreamOutcome::Completed => acc.stop_reason,
        _ => Some(format!("proxy:{}", outcome.label())),
    };
    close_turn(&ctx, ids.span_id.clone(), acc.tool_uses, stop_reason, ended).await;
}

/// Hands the turn tracker the response that ended this round trip, and
/// emits the turn root if that closed the turn.
///
/// Every path that finishes a span ends here, the two fault paths
/// included. A turn left open holds its trace exempt from the idle
/// timeout, so a failure that skipped this step used to leave the trace
/// hanging until the assembler pruned it half an hour later.
async fn close_turn(
    ctx: &SpanContext,
    chat_span_id: Vec<u8>,
    tool_uses: Vec<(String, String)>,
    stop_reason: Option<String>,
    ended: SystemTime,
) {
    let Some(ref p) = ctx.placement else { return };
    let root = ctx
        .state
        .tracker
        .lock()
        .expect("tracker mutex poisoned")
        .record_response(
            &ctx.agent_name,
            &p.trace_id,
            ResponseInfo {
                chat_span_id,
                tool_uses,
                stop_reason,
                ended_at: ended,
            },
        );
    if let Some(root) = root {
        emit_turn_root(&ctx.state, &ctx.agent_name, root).await;
    }
}

/// A round trip that never produced a response still produced a fact.
///
/// Both callers are 502 paths: the upstream was unreachable, or its body
/// could not be read. The span carries what is known, which is the shape
/// of the request, how long the attempt took, and why it ended.
async fn synthesize_fault_span(mut ctx: SpanContext, fault: &str, message: &str) {
    let ended = SystemTime::now();
    let mut attributes = vec![
        kv_str("gen_ai.provider.name", "anthropic"),
        kv_str("gen_ai.operation.name", "chat"),
        kv_str("reeve.proxy.fault", fault),
        kv_double("reeve.proxy.overhead_ms", ctx.overhead_ms),
    ];
    if let Some(ref p) = ctx.placement {
        attributes.extend(threading_attributes(p));
    }
    attributes.append(&mut ctx.request_attrs);
    attributes.append(&mut ctx.ratelimit);
    stamp_secret_findings(&ctx.secret_kinds, &mut attributes);

    let chat_span_id = random_bytes(8);
    let (trace_id, parent_span_id) = match &ctx.placement {
        Some(p) => (p.trace_id.clone(), p.root_span_id.clone()),
        None => (random_bytes(16), Vec::new()),
    };
    let span = OtlpSpan {
        trace_id,
        span_id: chat_span_id.clone(),
        parent_span_id,
        name: "gen_ai.chat".to_string(),
        start_time_unix_nano: to_nanos(ctx.arrived),
        end_time_unix_nano: to_nanos(ended),
        attributes,
        status: Some(OtlpStatus {
            code: 2,
            message: message.to_string(),
        }),
        ..Default::default()
    };
    emit_pipeline_span(&ctx.state, &ctx.agent_name, span, ctx.arrived).await;
    close_turn(
        &ctx,
        chat_span_id,
        Vec::new(),
        Some(format!("proxy:{fault}")),
        ended,
    )
    .await;
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
        let text = intervention_message(&cmd.payload);
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

/// The user-role message an intervention injects. Steering, not
/// correction: the earlier "disregard the current approach" wording read
/// as "you made a mistake", and a live agent answered a redirect with
/// "I did this in error" and started treating good work as wrong. The
/// message must carry that priorities changed and no fault exists, or
/// the model invents one and self-attributes it.
fn intervention_message(payload: &ProxyPayload) -> String {
    match payload {
        ProxyPayload::Redirect { instruction } => format!(
            "[Operator redirect via Reeve] A human operator watching this session \
             has changed the priorities. Your work so far is not in question. \
             From this point, do the following instead: {instruction}"
        ),
        ProxyPayload::InjectContext { context } => format!(
            "[Operator context via Reeve] A human operator watching this session \
             shares the following context: {context}"
        ),
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
    let mut attributes = vec![
        kv_str("gen_ai.provider.name", "anthropic"),
        kv_str("gen_ai.operation.name", "execute_tool"),
        // The clean tool name, so the judge scores [bash, read]
        // rather than raw operation names.
        kv_str("gen_ai.tool.name", &tool.name),
    ];
    // The input's fingerprint, never the input: loop detection compares
    // hashes to tell repeated work from repeated calls.
    if let Some(ref hash) = tool.input_hash {
        attributes.push(kv_str("reeve.tool.input_hash", hash));
    }
    let span = OtlpSpan {
        trace_id: placement.trace_id.clone(),
        span_id: random_bytes(8),
        parent_span_id: tool.parent_span_id.clone(),
        name: format!("gen_ai.tool:{}", tool.name),
        start_time_unix_nano: to_nanos(tool.started_at),
        end_time_unix_nano: to_nanos(tool.ended_at),
        attributes,
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
async fn synthesize_span(mut ctx: SpanContext, resp_body: &[u8], http_status: u16) {
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
        kv_str("gen_ai.provider.name", "anthropic"),
        kv_str("gen_ai.operation.name", "chat"),
        kv_str("gen_ai.request.model", &model),
        kv_int("gen_ai.usage.input_tokens", input_tokens as i64),
        kv_int("gen_ai.usage.output_tokens", output_tokens as i64),
        kv_int(
            "gen_ai.usage.total_tokens",
            (input_tokens + output_tokens) as i64,
        ),
        kv_int("http.response.status_code", http_status as i64),
        kv_double("reeve.proxy.overhead_ms", ctx.overhead_ms),
    ];
    if let Some(ref p) = ctx.placement {
        attributes.extend(threading_attributes(p));
    }
    attributes.append(&mut ctx.request_attrs);
    attributes.append(&mut ctx.ratelimit);
    if let Some(ref reason) = stop_reason {
        attributes.push(kv_str("gen_ai.response.finish_reason", reason));
    }
    if thinking_tokens > 0 {
        attributes.push(kv_int(
            "gen_ai.usage.reasoning.output_tokens",
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
    surface_compaction(&ctx.state, &ctx.agent_name, &applied_edits, &mut attributes);
    stamp_secret_findings(&ctx.secret_kinds, &mut attributes);
    if cache_read > 0 {
        attributes.push(kv_int(
            "gen_ai.usage.cache_read.input_tokens",
            cache_read as i64,
        ));
    }
    if cache_creation > 0 {
        attributes.push(kv_int(
            "gen_ai.usage.cache_creation.input_tokens",
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
    let (trace_id, parent_span_id) = match &ctx.placement {
        Some(p) => (p.trace_id.clone(), p.root_span_id.clone()),
        None => (random_bytes(16), Vec::new()),
    };

    // Tier 2 only, and here the response really is one body, so it is
    // stored as it arrived rather than reassembled.
    if let (Some(capture), Some(request)) = (ctx.state.capture.clone(), ctx.request.take()) {
        capture.record(crate::capture::Round {
            span_id: hex(&chat_span_id),
            trace_id: hex(&trace_id),
            agent: ctx.agent_name.clone(),
            started_at_ms: to_millis(ctx.arrived),
            ended_at_ms: to_millis(ended),
            request,
            message_hashes: ctx
                .placement
                .as_ref()
                .map(|p| p.message_hashes.clone())
                .unwrap_or_default(),
            response: parsed,
        });
    }

    let span = OtlpSpan {
        trace_id,
        span_id: chat_span_id.clone(),
        parent_span_id,
        name: "gen_ai.chat".to_string(),
        start_time_unix_nano: to_nanos(ctx.arrived),
        end_time_unix_nano: to_nanos(ended),
        attributes,
        status: Some(OtlpStatus {
            code: status_code,
            message: String::new(),
        }),
        ..Default::default()
    };
    emit_pipeline_span(&ctx.state, &ctx.agent_name, span, ctx.arrived).await;
    close_turn(&ctx, chat_span_id, tool_uses, stop_reason, ended).await;
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

/// Stamps newly detected secret kinds on the span whose request first
/// carried them: count and kinds only, never the secrets. The ALERTS
/// notice already fired in the request path; this is the durable mark
/// that makes the moment findable in the trace afterward.
fn stamp_secret_findings(kinds: &[&'static str], attributes: &mut Vec<KeyValue>) {
    if kinds.is_empty() {
        return;
    }
    attributes.push(kv_int("reeve.secret.findings", kinds.len() as i64));
    attributes.push(kv_str("reeve.secret.kinds", &kinds.join(", ")));
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

fn kv_bool(key: &str, value: bool) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: Some(AnyValue {
            value: Some(any_value::Value::BoolValue(value)),
        }),
        ..Default::default()
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// What the request asked for, as span attributes.
///
/// Every one of these is a property of the ask, not of the answer, and
/// none of them is text the developer wrote, which is why they are
/// stamped at every privacy tier. `gen_ai.request.model` is deliberately
/// not among them: despite the name that key carries the model that
/// *responded*, it is what pricing and the renderer read, and the two
/// differ exactly when the question of routing is interesting. The model
/// asked for goes under `reeve.request.model` and the difference between
/// them is the measurement.
fn request_attributes(json: &serde_json::Value) -> Vec<KeyValue> {
    let mut attrs = Vec::new();
    if let Some(model) = json.get("model").and_then(|v| v.as_str()) {
        attrs.push(kv_str("reeve.request.model", model));
    }
    if let Some(max) = json.get("max_tokens").and_then(|v| v.as_i64()) {
        attrs.push(kv_int("reeve.request.max_tokens", max));
    }
    attrs.push(kv_bool(
        "reeve.request.stream",
        json.get("stream")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    ));
    // How large the offered toolset is, and how much of the prompt is
    // preamble: the two inputs a client controls that decide how much of
    // every request is fixed cost.
    if let Some(tools) = json.get("tools").and_then(|v| v.as_array()) {
        attrs.push(kv_int("reeve.request.tools", tools.len() as i64));
    }
    match json.get("system") {
        Some(serde_json::Value::Array(blocks)) => {
            attrs.push(kv_int("reeve.request.system_blocks", blocks.len() as i64));
        }
        Some(serde_json::Value::String(_)) => {
            attrs.push(kv_int("reeve.request.system_blocks", 1));
        }
        _ => {}
    }
    // Cache breakpoints are the client's cache strategy, stated. Reading
    // them beside the cache hit and miss counts on the response is how a
    // strategy gets judged rather than assumed.
    attrs.push(kv_int(
        "reeve.request.cache_breakpoints",
        count_cache_control(json),
    ));
    if let Some(budget) = json
        .get("thinking")
        .and_then(|t| t.get("budget_tokens"))
        .and_then(|v| v.as_i64())
    {
        attrs.push(kv_int("reeve.request.thinking_budget", budget));
    }
    attrs.extend(turn_shape_attributes(json));
    attrs
}

/// Whether a human started this round trip or the agent's own tool loop
/// did.
///
/// Both arrive as a `user` message, which is why a naive count of user
/// messages says nothing: an agentic client sends one per tool result, so
/// a single typed sentence and forty tool returns look identical. The
/// difference is in the content blocks, where a tool return is a
/// `tool_result` block and a person's message is not, and it is the
/// difference everything downstream wants. Cost per human turn is worth
/// knowing; cost per request is an artifact of how the client loops. It
/// is also what makes an operator correction detectable at all, since a
/// correction is by definition a human message that follows one.
///
/// The message carrying that difference is not always the last one. The
/// client can append its own after the tool results, and a trailing
/// `system` message is the shape that arrives in practice, so reading
/// only the last message sends a third of real turns to `unknown`. The
/// search runs backwards to the nearest `user` message instead, bounded
/// so the cost stays flat rather than growing with the conversation.
/// `reeve.request.last_role` still reports the true final role, which is
/// what makes an appended message visible at all.
fn turn_shape_attributes(json: &serde_json::Value) -> Vec<KeyValue> {
    /// Well clear of the single step every appended message has needed
    /// so far, and short enough that the walk is a constant.
    const LOOKBACK: usize = 4;

    let Some(messages) = json.get("messages").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let Some(last) = messages.last() else {
        return Vec::new();
    };
    let last_role = last.get("role").and_then(|v| v.as_str()).unwrap_or("");
    let subject = messages
        .iter()
        .rev()
        .take(LOOKBACK)
        .find(|m| m.get("role").and_then(|v| v.as_str()) == Some("user"));
    let tool_results = subject
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
                .count()
        })
        .unwrap_or(0);
    let kind = match (subject, tool_results) {
        (None, _) => "unknown",
        (Some(_), 0) => "human",
        (Some(_), _) => "tool_loop",
    };
    vec![
        kv_str("reeve.request.turn_kind", kind),
        kv_str("reeve.request.last_role", last_role),
        kv_int("reeve.request.tool_results", tool_results as i64),
    ]
}

/// Counts `cache_control` markers in the places the API accepts them:
/// system blocks, tool definitions, and each message's top-level content
/// blocks.
///
/// Deliberately shallow. A real request is 1.3 MB of parsed JSON at the
/// top of a long session, and a recursive walk of it would cost several
/// milliseconds, which is more than the proxy's entire measured overhead
/// and would therefore corrupt the number it is being measured by.
fn count_cache_control(json: &serde_json::Value) -> i64 {
    fn marked(v: &serde_json::Value) -> i64 {
        i64::from(v.get("cache_control").is_some())
    }
    fn in_array(json: &serde_json::Value, key: &str) -> i64 {
        json.get(key)
            .and_then(|v| v.as_array())
            .map(|items| items.iter().map(marked).sum())
            .unwrap_or(0)
    }

    let messages: i64 = json
        .get("messages")
        .and_then(|v| v.as_array())
        .map(|msgs| {
            msgs.iter()
                .map(|m| marked(m) + in_array(m, "content"))
                .sum()
        })
        .unwrap_or(0);
    in_array(json, "system") + in_array(json, "tools") + messages
}

/// Rate-limit state and the upstream request id, read off the response
/// headers.
///
/// The rate-limit keys are copied generically rather than by name because
/// they differ by account type: an API key is metered per model class,
/// while a subscription reports one unified window. Naming the ones seen
/// on one machine would have quietly recorded nothing on the other.
fn response_header_attributes(headers: &HeaderMap) -> Vec<KeyValue> {
    const PREFIX: &str = "anthropic-ratelimit-";
    let mut attrs = Vec::new();
    for (name, value) in headers.iter() {
        let Ok(value) = value.to_str() else { continue };
        let name = name.as_str();
        if let Some(rest) = name.strip_prefix(PREFIX) {
            attrs.push(kv_str(
                &format!("reeve.ratelimit.{}", rest.replace('-', "_")),
                value,
            ));
        } else if name == "retry-after" {
            attrs.push(kv_str("reeve.ratelimit.retry_after", value));
        } else if name == "request-id" || name == "anthropic-request-id" {
            // The upstream's own handle for this round trip: the only
            // identifier a support conversation about a specific request
            // can be conducted in.
            attrs.push(kv_str("reeve.proxy.request_id", value));
        }
    }
    attrs
}

/// What threading decided, and on what evidence.
///
/// The last three exist because prefix matching against real Claude Code
/// traffic misses far more often than it should, and a miss on its own
/// says nothing about why. Recorded per request, they turn the question
/// into arithmetic over stored spans instead of a live debugging session.
fn threading_attributes(p: &TurnPlacement) -> Vec<KeyValue> {
    vec![
        kv_int("reeve.proxy.context_messages", p.message_count as i64),
        kv_bool("reeve.threading.new_conversation", p.new_conversation),
        kv_int("reeve.threading.matched_prefix", p.matched_prefix as i64),
        kv_int("reeve.threading.candidates", p.candidates as i64),
    ]
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
            ProxyConfig {
                upstream: format!("http://{}", upstream_addr),
                agent_name_override: None,
                stream_chunk_timeout: std::time::Duration::from_millis(500),
                pipeline_tx: tx,
                signal_tx,
                interventions: Some(interventions.clone()),
                active_streams: None,
                open_turns: None,
                secrets_block: false,
                capture: None,
            },
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
            ProxyConfig {
                upstream: format!("http://{}", upstream_addr),
                agent_name_override: None,
                stream_chunk_timeout: std::time::Duration::from_millis(500),
                pipeline_tx: tx,
                signal_tx,
                interventions: None,
                active_streams: None,
                open_turns: None,
                secrets_block: false,
                capture: None,
            },
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

    /// The proxy with secret handling under test: returns the base URL,
    /// the pipeline receiver, and a signal subscription.
    async fn spawn_proxy_for_secrets(
        block: bool,
    ) -> (
        String,
        mpsc::Receiver<PipelineSpan>,
        broadcast::Receiver<IngestionEvent>,
    ) {
        let upstream_app = axum::Router::new().route(
            "/v1/messages",
            post(|| async {
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

        let (tx, rx) = mpsc::channel(8);
        let (signal_tx, signal_rx) = broadcast::channel(64);
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();
        drop(proxy_listener);
        tokio::spawn(run_with(
            proxy_addr,
            ProxyConfig {
                upstream: format!("http://{}", upstream_addr),
                agent_name_override: None,
                stream_chunk_timeout: std::time::Duration::from_millis(500),
                pipeline_tx: tx,
                signal_tx,
                interventions: None,
                active_streams: None,
                open_turns: None,
                secrets_block: block,
                capture: None,
            },
        ));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        (format!("http://{}", proxy_addr), rx, signal_rx)
    }

    /// Assembled at runtime so no token-shaped literal sits in source,
    /// where it trips secret scanners; see the secrets module tests.
    fn leaky_body() -> String {
        format!(
            r#"{{"model":"claude-opus-4-8","messages":[{{"role":"user","content":"my .env says ANTHROPIC_KEY=sk-ant-{}-{}"}}]}}"#,
            "api03", "abcdefghijklmnopqrstuvwx"
        )
    }

    #[tokio::test]
    async fn a_leaked_secret_warns_once_and_stamps_the_span() {
        let (base, mut rx, mut signal_rx) = spawn_proxy_for_secrets(false).await;
        let client = reqwest::Client::new();
        let send = || {
            client
                .post(format!("{base}/v1/messages"))
                .header("user-agent", "claude-cli/2.0.0")
                .body(leaky_body())
                .send()
        };

        let resp = send().await.unwrap();
        assert_eq!(resp.status(), 200, "warn mode forwards the request");

        // The alert names the kind, redacted, and never the secret.
        let warning = loop {
            match signal_rx.recv().await.unwrap() {
                IngestionEvent::PipelineWarning { message } => break message,
                _ => continue,
            }
        };
        assert!(warning.contains("outbound secret"), "{warning}");
        assert!(warning.contains("anthropic api key"), "{warning}");
        assert!(
            !warning.contains("abcdefghijklmnopqrstuvwx"),
            "the alert must never carry the secret: {warning}"
        );

        // The chat span carries the durable mark.
        let span = rx.recv().await.expect("chat span synthesized");
        match attr(&span.span, "reeve.secret.kinds") {
            Some(any_value::Value::StringValue(kinds)) => {
                assert!(kinds.contains("anthropic api key"), "{kinds}")
            }
            other => panic!("secret kinds missing on span: {other:?}"),
        }

        // The replayed history re-sends the secret; it speaks once.
        let resp = send().await.unwrap();
        assert_eq!(resp.status(), 200);
        let span2 = rx.recv().await.expect("second chat span");
        assert!(
            attr(&span2.span, "reeve.secret.kinds").is_none(),
            "a seen secret does not re-stamp"
        );
        assert!(
            matches!(
                signal_rx.try_recv(),
                Err(broadcast::error::TryRecvError::Empty)
            ),
            "a seen secret does not re-alert"
        );
    }

    #[tokio::test]
    async fn block_mode_refuses_a_request_carrying_a_secret() {
        let (base, _rx, _signal_rx) = spawn_proxy_for_secrets(true).await;
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{base}/v1/messages"))
            .header("user-agent", "claude-cli/2.0.0")
            .body(leaky_body())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 403);
        let body = resp.text().await.unwrap();
        assert!(body.contains("Reeve"), "the refusal names itself: {body}");

        // A retry replays the same secret: still refused. The history
        // re-leaks on every request, so the wall holds.
        let retry = client
            .post(format!("{base}/v1/messages"))
            .header("user-agent", "claude-cli/2.0.0")
            .body(leaky_body())
            .send()
            .await
            .unwrap();
        assert_eq!(retry.status(), 403, "a seen secret still blocks");

        // Clean traffic flows: the block is per request, not per agent.
        let clean = client
            .post(format!("{base}/v1/messages"))
            .header("user-agent", "claude-cli/2.0.0")
            .body(r#"{"model":"claude-opus-4-8","messages":[{"role":"user","content":"hi"}]}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(clean.status(), 200, "clean requests are not punished");
    }

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
            ProxyConfig {
                upstream: format!("http://{}", upstream_addr),
                agent_name_override: None,
                stream_chunk_timeout: std::time::Duration::from_millis(500),
                pipeline_tx: tx,
                signal_tx,
                interventions: None,
                active_streams: None,
                open_turns: None,
                secrets_block: false,
                capture: None,
            },
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
            ProxyConfig {
                upstream: format!("http://{}", upstream_addr),
                agent_name_override: None,
                stream_chunk_timeout: std::time::Duration::from_millis(500),
                pipeline_tx: tx,
                signal_tx,
                interventions: None,
                active_streams: None,
                open_turns: Some(open_turns.clone()),
                secrets_block: false,
                capture: None,
            },
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
        match attr(&ps.span, "gen_ai.usage.cache_read.input_tokens") {
            Some(any_value::Value::IntValue(n)) => assert_eq!(*n, 2000),
            other => panic!("cache_read_tokens missing: {other:?}"),
        }
        match attr(&ps.span, "gen_ai.usage.cache_creation.input_tokens") {
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

    #[test]
    fn intervention_messages_steer_without_fault_framing() {
        let redirect = intervention_message(&ProxyPayload::Redirect {
            instruction: "focus on the tests".to_string(),
        });
        // The live failure mode: fault-framing words make the model
        // apologize for the operator's decision and undo good work.
        for banned in ["Disregard", "disregard", "wrong", "mistake", "error"] {
            assert!(!redirect.contains(banned), "fault framing: {banned}");
        }
        assert!(redirect.contains("not in question"));
        assert!(redirect.contains("focus on the tests"));
        assert!(redirect.starts_with("[Operator redirect via Reeve]"));

        let inject = intervention_message(&ProxyPayload::InjectContext {
            context: "the deploy window closes at 5".to_string(),
        });
        assert!(inject.contains("the deploy window closes at 5"));
        assert!(inject.starts_with("[Operator context via Reeve]"));
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

    fn kv_of<'a>(attrs: &'a [KeyValue], key: &str) -> Option<&'a any_value::Value> {
        attrs
            .iter()
            .find(|kv| kv.key == key)?
            .value
            .as_ref()?
            .value
            .as_ref()
    }

    fn as_int(attrs: &[KeyValue], key: &str) -> Option<i64> {
        match kv_of(attrs, key)? {
            any_value::Value::IntValue(i) => Some(*i),
            _ => None,
        }
    }

    fn as_str<'a>(attrs: &'a [KeyValue], key: &str) -> Option<&'a str> {
        match kv_of(attrs, key)? {
            any_value::Value::StringValue(s) => Some(s),
            _ => None,
        }
    }

    #[test]
    fn the_requested_model_is_recorded_apart_from_the_responding_one() {
        let json = serde_json::json!({
            "model": "claude-opus-4-8",
            "max_tokens": 4096,
            "stream": true,
            "messages": [],
        });
        let attrs = request_attributes(&json);
        assert_eq!(
            as_str(&attrs, "reeve.request.model"),
            Some("claude-opus-4-8")
        );
        assert_eq!(as_int(&attrs, "reeve.request.max_tokens"), Some(4096));
        assert!(
            kv_of(&attrs, "gen_ai.request.model").is_none(),
            "gen_ai.request.model belongs to the response and is what pricing reads; \
             the request must never claim it"
        );
    }

    #[test]
    fn cache_breakpoints_are_counted_where_the_api_accepts_them() {
        let json = serde_json::json!({
            "system": [
                {"type": "text", "text": "a"},
                {"type": "text", "text": "b", "cache_control": {"type": "ephemeral"}},
            ],
            "tools": [
                {"name": "read"},
                {"name": "write", "cache_control": {"type": "ephemeral"}},
            ],
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}},
                ]},
                {"role": "assistant", "content": [{"type": "text", "text": "hello"}]},
            ],
        });
        assert_eq!(count_cache_control(&json), 3);
        let attrs = request_attributes(&json);
        assert_eq!(as_int(&attrs, "reeve.request.cache_breakpoints"), Some(3));
        assert_eq!(as_int(&attrs, "reeve.request.system_blocks"), Some(2));
        assert_eq!(as_int(&attrs, "reeve.request.tools"), Some(2));
    }

    #[test]
    fn a_typed_message_and_a_tool_return_are_told_apart() {
        let human = serde_json::json!({
            "messages": [{"role": "user", "content": [{"type": "text", "text": "no, undo that"}]}],
        });
        let attrs = request_attributes(&human);
        assert_eq!(as_str(&attrs, "reeve.request.turn_kind"), Some("human"));
        assert_eq!(as_int(&attrs, "reeve.request.tool_results"), Some(0));

        let loop_step = serde_json::json!({
            "messages": [{"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "a", "content": "ok"},
                {"type": "tool_result", "tool_use_id": "b", "content": "ok"},
            ]}],
        });
        let attrs = request_attributes(&loop_step);
        assert_eq!(as_str(&attrs, "reeve.request.turn_kind"), Some("tool_loop"));
        assert_eq!(as_int(&attrs, "reeve.request.tool_results"), Some(2));
    }

    #[test]
    fn a_message_appended_after_the_tool_returns_does_not_hide_them() {
        let json = serde_json::json!({
            "messages": [
                {"role": "assistant", "content": [{"type": "text", "text": "checking"}]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "a", "content": "ok"},
                ]},
                {"role": "system", "content": "keep going"},
            ],
        });
        let attrs = request_attributes(&json);
        assert_eq!(as_str(&attrs, "reeve.request.turn_kind"), Some("tool_loop"));
        assert_eq!(as_int(&attrs, "reeve.request.tool_results"), Some(1));
        // The appended message is still reported, since it is the only
        // way to notice the client has started doing this.
        assert_eq!(as_str(&attrs, "reeve.request.last_role"), Some("system"));
    }

    #[test]
    fn a_turn_with_no_user_message_within_reach_stays_unknown() {
        let json = serde_json::json!({
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "go"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "a"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "b"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "c"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "d"}]},
            ],
        });
        let attrs = request_attributes(&json);
        assert_eq!(as_str(&attrs, "reeve.request.turn_kind"), Some("unknown"));
        assert_eq!(as_int(&attrs, "reeve.request.tool_results"), Some(0));
    }

    #[test]
    fn rate_limit_headers_are_copied_without_being_named() {
        let mut headers = HeaderMap::new();
        // Deliberately a header this code has never seen: an API key and a
        // subscription report different windows, and naming the ones on
        // one machine would silently record nothing on the other.
        headers.insert(
            "anthropic-ratelimit-unified-reset",
            "2026-08-15T12".parse().unwrap(),
        );
        headers.insert("retry-after", "42".parse().unwrap());
        headers.insert("request-id", "req_abc".parse().unwrap());
        headers.insert("content-type", "application/json".parse().unwrap());

        let attrs = response_header_attributes(&headers);
        assert_eq!(
            as_str(&attrs, "reeve.ratelimit.unified_reset"),
            Some("2026-08-15T12")
        );
        assert_eq!(as_str(&attrs, "reeve.ratelimit.retry_after"), Some("42"));
        assert_eq!(as_str(&attrs, "reeve.proxy.request_id"), Some("req_abc"));
        assert_eq!(attrs.len(), 3, "unrelated headers must not be copied");
    }
}
