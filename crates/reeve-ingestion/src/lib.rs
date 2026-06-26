pub mod assemble;
pub mod normalize;
pub mod receive;
pub mod route;

use opentelemetry_proto::tonic::collector::trace::v1::trace_service_server::TraceServiceServer;
use receive::OtlpReceiver;
use reeve_storage::hot::HotStore;
use reeve_storage::warm::WarmStore;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tonic::transport::Server;
use tonic_health::{ServingStatus, server::health_reporter};

pub async fn serve(addr: SocketAddr, db_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let hot = Arc::new(Mutex::new(HotStore::new(10_000)));
    let warm = Arc::new(WarmStore::open(db_path)?);

    let (pipeline_tx, pipeline_rx) = tokio::sync::mpsc::channel(1024);
    let (assemble_tx, assemble_rx) = tokio::sync::mpsc::channel(1024);
    let (route_tx, route_rx) = tokio::sync::mpsc::channel(1024);

    tokio::spawn(normalize::run(pipeline_rx, false, assemble_tx));
    tokio::spawn(assemble::run(assemble_rx, 500, route_tx));
    tokio::spawn(route::run(route_rx, hot, warm));

    let (health_reporter, health_service) = health_reporter();
    health_reporter
        .set_service_status("", ServingStatus::Serving)
        .await;

    tracing::info!(addr = %addr, "OTLP gRPC receiver listening");

    Server::builder()
        .add_service(health_service)
        .add_service(TraceServiceServer::new(OtlpReceiver::new(pipeline_tx)))
        .serve(addr)
        .await?;

    Ok(())
}
