//! The fixture corpus: real wire shapes replayed through the real proxy
//! against a scripted upstream, asserting what the pipeline produced.
//! Every scenario reproduces a shape that broke something once; mocks
//! are too polite to catch these, which is why each one earned a place
//! here after it was found in live traffic.
//!
//! Fixtures are authored from documented shapes, not scrubbed from
//! captures: no real session content exists in them, and nothing
//! credential-shaped sits in the repo at rest (placeholders are
//! substituted at load time).

use opentelemetry_proto::tonic::common::v1::any_value;
use reeve_ingestion::normalize::PipelineSpan;
use reeve_ingestion::proxy::run_with;
use serde::Deserialize;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc};

#[derive(Deserialize)]
struct Manifest {
    #[allow(dead_code)]
    description: String,
    steps: Vec<Step>,
}

#[derive(Deserialize)]
struct Step {
    request: String,
    response: PreparedResponse,
}

#[derive(Deserialize, Clone)]
struct PreparedResponse {
    kind: String,
    file: String,
    #[serde(default)]
    chunking: Option<String>,
}

/// The fake key for the credential fixture, assembled at load time so
/// no token-shaped literal sits in the repo for scanners to find.
fn fake_key() -> String {
    format!("sk-ant-{}-{}", "api03", "abcdefghijklmnopqrstuvwx")
}

fn fixture_dir(scenario: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(scenario)
}

/// Runs one scenario end to end: scripted upstream, real proxy, the
/// fixture's requests in order. Returns every span the pipeline saw.
async fn replay(scenario: &str) -> Vec<PipelineSpan> {
    let dir = fixture_dir(scenario);
    let manifest: Manifest =
        serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap();

    // The scripted upstream: each Messages request pops the next
    // prepared response, served exactly as the fixture specifies.
    let responses: Arc<Mutex<VecDeque<(PreparedResponse, String)>>> = Arc::new(Mutex::new(
        manifest
            .steps
            .iter()
            .map(|s| {
                let text = std::fs::read_to_string(dir.join(&s.response.file)).unwrap();
                (s.response.clone(), text)
            })
            .collect(),
    ));
    let upstream_app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move || {
            let responses = responses.clone();
            async move {
                let (prep, text) = responses
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("a request beyond the script");
                match prep.kind.as_str() {
                    "json" => axum::response::Response::builder()
                        .status(200)
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(text))
                        .unwrap(),
                    "sse" => {
                        let (tx, rx) =
                            mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(2048);
                        let by_bytes = prep.chunking.as_deref() == Some("bytes");
                        tokio::spawn(async move {
                            if by_bytes {
                                // The cruelest chunking: every byte its
                                // own chunk, every boundary exercised.
                                for b in text.into_bytes() {
                                    if tx.send(Ok(vec![b].into())).await.is_err() {
                                        return;
                                    }
                                }
                            } else {
                                let _ = tx.send(Ok(text.into_bytes().into())).await;
                            }
                        });
                        axum::response::Response::builder()
                            .status(200)
                            .header("content-type", "text/event-stream")
                            .body(axum::body::Body::from_stream(
                                tokio_stream::wrappers::ReceiverStream::new(rx),
                            ))
                            .unwrap()
                    }
                    other => panic!("unknown response kind {other}"),
                }
            }
        }),
    );
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream_listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(upstream_listener, upstream_app).await.unwrap();
    });

    let (pipeline_tx, mut pipeline_rx) = mpsc::channel(64);
    let (signal_tx, _keep) = broadcast::channel(64);
    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    drop(proxy_listener);
    tokio::spawn(run_with(
        proxy_addr,
        format!("http://{upstream_addr}"),
        None,
        std::time::Duration::from_millis(2_000),
        pipeline_tx,
        signal_tx,
        None,
        None,
        None,
        false,
    ));
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let client = reqwest::Client::new();
    for step in &manifest.steps {
        let body = std::fs::read_to_string(dir.join(&step.request))
            .unwrap()
            .replace("{{FAKE_ANTHROPIC_KEY}}", &fake_key());
        let resp = client
            .post(format!("http://{proxy_addr}/v1/messages"))
            .header("content-type", "application/json")
            .header("user-agent", "claude-cli/2.0.0")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "fixture request must forward");
        // Streamed bodies finalize their span after the client drains
        // the stream, so consume it before moving on.
        let _ = resp.bytes().await.unwrap();
    }

    // Everything the replay produced; spans stop arriving quickly since
    // synthesis happens inside or right after each round trip.
    let mut spans = Vec::new();
    while let Ok(Some(ps)) =
        tokio::time::timeout(std::time::Duration::from_millis(700), pipeline_rx.recv()).await
    {
        spans.push(ps);
    }
    spans
}

fn attr<'a>(ps: &'a PipelineSpan, key: &str) -> Option<&'a any_value::Value> {
    ps.span
        .attributes
        .iter()
        .find(|kv| kv.key == key)
        .and_then(|kv| kv.value.as_ref())
        .and_then(|v| v.value.as_ref())
}

fn int_attr(ps: &PipelineSpan, key: &str) -> Option<i64> {
    match attr(ps, key) {
        Some(any_value::Value::IntValue(n)) => Some(*n),
        _ => None,
    }
}

fn str_attr<'a>(ps: &'a PipelineSpan, key: &str) -> Option<&'a str> {
    match attr(ps, key) {
        Some(any_value::Value::StringValue(s)) => Some(s.as_str()),
        _ => None,
    }
}

fn chats(spans: &[PipelineSpan]) -> Vec<&PipelineSpan> {
    spans
        .iter()
        .filter(|ps| ps.span.name == "gen_ai.chat")
        .collect()
}

#[tokio::test]
async fn moving_cache_control_threads_into_one_turn() {
    let spans = replay("moving-cache-control").await;
    let chats = chats(&spans);
    assert_eq!(chats.len(), 2, "two round trips, two chat spans");
    assert_eq!(
        chats[0].span.trace_id, chats[1].span.trace_id,
        "the moving marker must not break the prefix: one turn"
    );
    let tool = spans
        .iter()
        .find(|ps| ps.span.name == "gen_ai.tool:bash")
        .expect("the tool call is reconstructed");
    assert_eq!(tool.span.trace_id, chats[0].span.trace_id);
    assert!(
        spans
            .iter()
            .any(|ps| ps.span.name.starts_with("agent.turn")),
        "end_turn emits the turn root"
    );
}

#[tokio::test]
async fn a_side_call_gets_its_own_trace() {
    let spans = replay("concurrent-side-call").await;
    let chats = chats(&spans);
    assert_eq!(chats.len(), 3);
    let mut traces: Vec<&[u8]> = chats.iter().map(|c| c.span.trace_id.as_slice()).collect();
    traces.dedup();
    // First and third requests are the main turn; the middle one is the
    // side call and must not have stolen or joined it.
    assert_eq!(
        chats[0].span.trace_id, chats[2].span.trace_id,
        "the main turn survives the side call"
    );
    assert_ne!(
        chats[1].span.trace_id, chats[0].span.trace_id,
        "the side call gets its own trace"
    );
}

#[tokio::test]
async fn byte_chunked_sse_reassembles_completely() {
    let spans = replay("cruel-chunking").await;
    let chats = chats(&spans);
    assert_eq!(chats.len(), 1);
    let chat = chats[0];
    assert_eq!(
        str_attr(chat, "gen_ai.request.model"),
        Some("claude-opus-4-8")
    );
    assert_eq!(int_attr(chat, "gen_ai.usage.input_tokens"), Some(900));
    assert_eq!(int_attr(chat, "gen_ai.usage.output_tokens"), Some(42));
    assert_eq!(
        int_attr(chat, "gen_ai.usage.cache_read.input_tokens"),
        Some(100)
    );
    assert_eq!(
        str_attr(chat, "reeve.proxy.stream_outcome"),
        Some("completed"),
        "the stream completed despite one-byte chunks"
    );
    assert!(
        attr(chat, "gen_ai.usage.cost").is_some(),
        "a known model prices"
    );
}

#[tokio::test]
async fn an_errored_tool_result_fails_its_span() {
    let spans = replay("errored-tool-result").await;
    let tool = spans
        .iter()
        .find(|ps| ps.span.name == "gen_ai.tool:bash")
        .expect("tool span synthesized");
    assert_eq!(
        tool.span.status.as_ref().map(|s| s.code),
        Some(2),
        "is_error on the result must mark the tool span failed"
    );
}

#[tokio::test]
async fn thinking_and_compaction_fields_land_as_attributes() {
    let spans = replay("thinking-and-compaction").await;
    let chats = chats(&spans);
    assert_eq!(chats.len(), 1);
    let chat = chats[0];
    assert_eq!(
        int_attr(chat, "gen_ai.usage.reasoning.output_tokens"),
        Some(47)
    );
    assert_eq!(int_attr(chat, "reeve.context.applied_edits"), Some(1));
    assert!(
        str_attr(chat, "reeve.context.edit_types").is_some_and(|t| t.contains("clear_thinking")),
        "the applied edit type is stamped"
    );
}

#[tokio::test]
async fn a_credential_bearing_body_stamps_the_span() {
    let spans = replay("credential-bearing-body").await;
    let chats = chats(&spans);
    assert_eq!(chats.len(), 1);
    let chat = chats[0];
    assert!(
        str_attr(chat, "reeve.secret.kinds").is_some_and(|k| k.contains("anthropic api key")),
        "the scanner marks the span that carried the leak"
    );
    // The mark is the redacted kind, never the key itself.
    for kv in &chat.span.attributes {
        if let Some(any_value::Value::StringValue(s)) =
            kv.value.as_ref().and_then(|v| v.value.as_ref())
        {
            assert!(
                !s.contains(&fake_key()),
                "no attribute may carry the secret"
            );
        }
    }
}
