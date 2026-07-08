//! The HTTP proxy input path: point `ANTHROPIC_BASE_URL` at this server
//! and an uninstrumented tool appears in the cockpit. Requests forward to
//! the real API; spans are synthesized from what passes through and fed
//! into the same pipeline the OTel receiver uses.
//!
//! The Authorization and x-api-key headers are forwarded in memory and
//! never logged, persisted, or attached to any synthesized span.

use crate::normalize::PipelineSpan;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::Response;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue, any_value};
use opentelemetry_proto::tonic::trace::v1::Span as OtlpSpan;
use opentelemetry_proto::tonic::trace::v1::Status as OtlpStatus;
use reeve_model::entity::IntegrationPath;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

const DEFAULT_UPSTREAM: &str = "https://api.anthropic.com";

struct ProxyState {
    client: reqwest::Client,
    upstream: String,
    pipeline_tx: mpsc::Sender<PipelineSpan>,
    /// Overrides User-Agent derivation when set (REEVE_PROXY_AGENT_NAME).
    agent_name_override: Option<String>,
}

pub async fn run(
    addr: SocketAddr,
    pipeline_tx: mpsc::Sender<PipelineSpan>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let upstream =
        std::env::var("REEVE_PROXY_UPSTREAM").unwrap_or_else(|_| DEFAULT_UPSTREAM.to_string());
    run_with(
        addr,
        upstream,
        std::env::var("REEVE_PROXY_AGENT_NAME").ok(),
        pipeline_tx,
    )
    .await
}

pub async fn run_with(
    addr: SocketAddr,
    upstream: String,
    agent_name_override: Option<String>,
    pipeline_tx: mpsc::Sender<PipelineSpan>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let state = Arc::new(ProxyState {
        client: reqwest::Client::new(),
        upstream,
        pipeline_tx,
        agent_name_override,
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

    let mut req = state.client.request(method.clone(), &url);
    for (name, value) in headers.iter() {
        // Host belongs to the upstream; hyper sets the rest correctly.
        if name == axum::http::header::HOST || name == axum::http::header::CONTENT_LENGTH {
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

    let upstream_resp = match req.body(body.clone()).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "proxy could not reach upstream");
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
        // SSE passes through chunk by chunk with nothing buffered; span
        // synthesis for streams is the next milestone item.
        let stream = upstream_resp.bytes_stream();
        return build_response(status, &resp_headers, Body::from_stream(stream));
    }

    let resp_body = match upstream_resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "proxy failed reading upstream response");
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from("upstream response read failed"))
                .expect("static response construction cannot fail");
        }
    };

    if method == Method::POST && uri.path().ends_with("/v1/messages") {
        synthesize_span(
            &state,
            &headers,
            &resp_body,
            status.as_u16(),
            arrived,
            overhead_ms,
        )
        .await;
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

/// One Messages API round trip becomes one gen_ai.chat span carrying the
/// model, token usage, and estimated cost, fed through the same pipeline
/// as SDK spans. Upstream failures (429s, 5xx) synthesize a failed span
/// so retry storms render visibly.
async fn synthesize_span(
    state: &ProxyState,
    req_headers: &HeaderMap,
    resp_body: &[u8],
    http_status: u16,
    arrived: SystemTime,
    overhead_ms: f64,
) {
    let ended = SystemTime::now();
    let arrived_ms = to_millis(arrived);

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
    let cache_read = get_u64("cache_read_input_tokens");
    let cache_creation = get_u64("cache_creation_input_tokens");

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
    if cache_read > 0 {
        attributes.push(kv_int("gen_ai.usage.cache_read_tokens", cache_read as i64));
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

    // STATUS_CODE_ERROR = 2: upstream refusals and failures render as
    // failed spans, which is what makes a retry storm visible.
    let status_code = if http_status >= 400 { 2 } else { 1 };

    let span = OtlpSpan {
        trace_id: random_bytes(16),
        span_id: random_bytes(8),
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

    let service_name = state
        .agent_name_override
        .clone()
        .unwrap_or_else(|| derive_agent_name(req_headers));

    let ps = PipelineSpan {
        span,
        service_name,
        service_instance_id: "proxy".to_string(),
        framework: "proxy".to_string(),
        arrived_at: arrived_ms,
        clock_offset_ms: 0,
        integration: IntegrationPath::Proxy,
    };
    if state.pipeline_tx.send(ps).await.is_err() {
        tracing::warn!("normalize stage unavailable, proxy span discarded");
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
        let upstream_app = axum::Router::new().route(
            "/v1/messages",
            post(move || async move {
                Response::builder()
                    .status(upstream_status)
                    .header("content-type", "application/json")
                    .body(Body::from(upstream_body))
                    .unwrap()
            }),
        );
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(upstream_listener, upstream_app).await.unwrap();
        });

        let (tx, rx) = mpsc::channel(8);
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();
        drop(proxy_listener);
        tokio::spawn(run_with(
            proxy_addr,
            format!("http://{}", upstream_addr),
            None,
            tx,
        ));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        (format!("http://{}", proxy_addr), rx)
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

    #[test]
    fn synthesized_ids_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..10_000 {
            assert!(seen.insert(random_bytes(8)), "span ids must never collide");
        }
    }
}
