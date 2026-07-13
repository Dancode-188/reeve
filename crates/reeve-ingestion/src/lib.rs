pub mod assemble;
pub mod normalize;
pub mod pricing;
pub mod proxy;
pub mod receive;
pub mod route;
pub mod secrets;
pub mod sse;
pub mod threading;

use opentelemetry_proto::tonic::collector::trace::v1::trace_service_server::TraceServiceServer;
use receive::OtlpReceiver;
use reeve_model::signal::IngestionEvent;
use reeve_storage::hot::HotStore;
use reeve_storage::warm::WarmStore;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use tonic::transport::Server;
use tonic_health::{ServingStatus, server::health_reporter};

#[allow(clippy::too_many_arguments)]
pub async fn serve(
    addr: SocketAddr,
    proxy_addr: SocketAddr,
    warm: Arc<WarmStore>,
    signal_tx: broadcast::Sender<IngestionEvent>,
    ntp_offsets: receive::NtpOffsets,
    paused: assemble::PausedAgents,
    disconnected: assemble::DisconnectedAgents,
    proxy_interventions: reeve_model::entity::ProxyInterventions,
    capture_content: bool,
    secrets_block: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let hot = Arc::new(Mutex::new(HotStore::new(10_000)));

    let (pipeline_tx, pipeline_rx) = tokio::sync::mpsc::channel(1024);
    let (assemble_tx, assemble_rx) = tokio::sync::mpsc::channel(1024);
    let (route_tx, route_rx) = tokio::sync::mpsc::channel(1024);

    let resume_tx = assemble_tx.clone();
    tokio::spawn(normalize::run(
        pipeline_rx,
        capture_content,
        assemble_tx,
        signal_tx.clone(),
    ));

    // Traces mid-generation are exempt from the idle timeout: the proxy
    // marks them here while a Messages round trip is in flight, and the
    // assembler treats them as alive however long the model streams.
    let active_streams: assemble::ActiveStreams =
        Arc::new(Mutex::new(std::collections::HashMap::new()));
    // Open turns hold the idle timeout across the client-side tool gaps
    // between round trips; the proxy writes, the assembler reads.
    let open_turns: assemble::OpenTurns = Arc::new(Mutex::new(std::collections::HashMap::new()));

    // The HTTP proxy is a second producer into the same pipeline: spans it
    // synthesizes are normalized, assembled, and routed like SDK spans.
    let proxy_pipeline_tx = pipeline_tx.clone();
    let proxy_signal_tx = signal_tx.clone();
    let proxy_active_streams = active_streams.clone();
    let proxy_open_turns = open_turns.clone();
    tokio::spawn(async move {
        if let Err(e) = proxy::run(
            proxy_addr,
            proxy_pipeline_tx,
            proxy_signal_tx,
            proxy_interventions,
            proxy_active_streams,
            proxy_open_turns,
            secrets_block,
        )
        .await
        {
            tracing::error!(error = %e, "HTTP proxy exited");
        }
    });
    // Resumable traces interrupted by the last shutdown or disconnect
    // reload into the assembler before live spans start arriving, so a
    // returning agent continues its trace instead of orphaning it.
    const RESUME_WINDOW_MS: i64 = 5 * 60 * 1000;
    match warm.claim_resumable_traces(RESUME_WINDOW_MS).await {
        Ok(traces) => {
            for trace in traces {
                let spans = warm
                    .list_spans_for_trace(&trace.id)
                    .await
                    .unwrap_or_default();
                let agent = match warm.get_agent(&trace.agent_id).await {
                    Ok(Some(a)) => a,
                    _ => continue,
                };
                tracing::info!(trace_id = %trace.id, agent = %agent.name, "resuming interrupted trace");
                for span in spans {
                    let ns = normalize::NormalizedSpan {
                        span,
                        events: Vec::new(),
                        agent: agent.clone(),
                    };
                    if resume_tx.send(ns).await.is_err() {
                        break;
                    }
                }
            }
        }
        Err(e) => tracing::warn!(error = %e, "resumable trace rescan failed"),
    }

    tokio::spawn(assemble::run(
        assemble_rx,
        500,
        route_tx,
        paused,
        disconnected,
        active_streams,
        open_turns,
    ));
    tokio::spawn(route::run(route_rx, hot, warm, signal_tx));

    let (health_reporter, health_service) = health_reporter();
    health_reporter
        .set_service_status("", ServingStatus::Serving)
        .await;

    tracing::info!(addr = %addr, "OTLP gRPC receiver listening");

    Server::builder()
        .add_service(health_service)
        .add_service(TraceServiceServer::new(OtlpReceiver::new(
            pipeline_tx,
            ntp_offsets,
        )))
        .serve(addr)
        .await?;

    Ok(())
}
