#![deny(clippy::all)]

use reeve_storage::warm::WarmStore;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
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

    // Log to a file so the TUI is not corrupted by tracing output on stderr.
    let log_path = db_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("reeve.log");
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "reeve=info,reeve_ingestion=info,reeve_renderer=info,reeve_engine=info".into()
            }),
        )
        .with_writer(log_file)
        .with_ansi(false)
        .init();

    tracing::info!(path = %log_path.display(), "logging to file");

    let warm = Arc::new(WarmStore::open(&db_path)?);
    let (ingestion_tx, ingestion_rx) = broadcast::channel(256);
    let (engine_event_tx, engine_event_rx) =
        broadcast::channel::<reeve_model::signal::EngineEvent>(64);

    let addr: SocketAddr = std::env::var("REEVE_ADDR")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| "127.0.0.1:4317".parse().unwrap());

    let ntp_offsets: Arc<Mutex<HashMap<String, i64>>> = Arc::new(Mutex::new(HashMap::new()));

    let engine_ingestion_rx = ingestion_tx.subscribe();
    tokio::spawn(reeve_ingestion::serve(
        addr,
        warm.clone(),
        ingestion_tx,
        ntp_offsets.clone(),
    ));
    tokio::spawn(reeve_engine::run(
        engine_ingestion_rx,
        engine_event_tx.clone(),
        warm.clone(),
    ));
    let control_server =
        reeve_intervention::server::run(engine_event_tx.clone(), ntp_offsets).await;
    let audit_path = db_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("audit.log");
    let dispatcher =
        reeve_intervention::dispatcher::Dispatcher::new(control_server, warm.clone(), audit_path);
    reeve_renderer::run(ingestion_rx, engine_event_rx, warm, ascii_mode, dispatcher).await?;

    Ok(())
}
