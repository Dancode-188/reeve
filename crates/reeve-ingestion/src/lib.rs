pub mod assemble;
pub mod normalize;
pub mod pricing;
pub mod proxy;
pub mod receive;
pub mod route;
pub mod sse;

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

pub async fn serve(
    addr: SocketAddr,
    proxy_addr: SocketAddr,
    warm: Arc<WarmStore>,
    signal_tx: broadcast::Sender<IngestionEvent>,
    ntp_offsets: receive::NtpOffsets,
    paused: assemble::PausedAgents,
    capture_content: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let hot = Arc::new(Mutex::new(HotStore::new(10_000)));

    let (pipeline_tx, pipeline_rx) = tokio::sync::mpsc::channel(1024);
    let (assemble_tx, assemble_rx) = tokio::sync::mpsc::channel(1024);
    let (route_tx, route_rx) = tokio::sync::mpsc::channel(1024);

    tokio::spawn(normalize::run(pipeline_rx, capture_content, assemble_tx));

    // The HTTP proxy is a second producer into the same pipeline: spans it
    // synthesizes are normalized, assembled, and routed like SDK spans.
    let proxy_pipeline_tx = pipeline_tx.clone();
    let proxy_signal_tx = signal_tx.clone();
    tokio::spawn(async move {
        if let Err(e) = proxy::run(proxy_addr, proxy_pipeline_tx, proxy_signal_tx).await {
            tracing::error!(error = %e, "HTTP proxy exited");
        }
    });
    tokio::spawn(assemble::run(assemble_rx, 500, route_tx, paused));
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
