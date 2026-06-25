pub mod assemble;
pub mod normalize;
pub mod receive;
pub mod route;

use opentelemetry_proto::tonic::collector::trace::v1::trace_service_server::TraceServiceServer;
use receive::OtlpReceiver;
use std::net::SocketAddr;
use tonic::transport::Server;
use tonic_health::{ServingStatus, server::health_reporter};

pub async fn serve(addr: SocketAddr) -> Result<(), Box<dyn std::error::Error>> {
    let (health_reporter, health_service) = health_reporter();
    health_reporter
        .set_service_status("", ServingStatus::Serving)
        .await;

    tracing::info!(addr = %addr, "OTLP gRPC receiver listening");

    Server::builder()
        .add_service(health_service)
        .add_service(TraceServiceServer::new(OtlpReceiver::new()))
        .serve(addr)
        .await?;

    Ok(())
}
