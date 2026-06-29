#![deny(clippy::all)]

use reeve_storage::warm::WarmStore;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::broadcast;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "reeve=info,reeve_ingestion=info,reeve_renderer=info".into()),
        )
        .init();

    let ascii_mode = std::env::args().any(|a| a == "--ascii");

    let db_path = std::env::var("REEVE_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(|h| PathBuf::from(h).join(".local/share/reeve/reeve.db"))
                .unwrap_or_else(|_| PathBuf::from("reeve.db"))
        });

    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let warm = Arc::new(WarmStore::open(&db_path)?);
    let (ingestion_tx, ingestion_rx) = broadcast::channel(256);
    let (engine_event_tx, engine_event_rx) =
        broadcast::channel::<reeve_model::signal::EngineEvent>(64);

    let addr: SocketAddr = std::env::var("REEVE_ADDR")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| "127.0.0.1:4317".parse().unwrap());

    tokio::spawn(reeve_ingestion::serve(addr, warm.clone(), ingestion_tx));

    // engine_event_tx is held here until reeve-engine exists and subscribes.
    let _ = &engine_event_tx;
    reeve_renderer::run(ingestion_rx, engine_event_rx, warm, ascii_mode).await?;

    Ok(())
}
