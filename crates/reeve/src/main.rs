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

    // Paused-agent state shared between the intervention layer (writer) and
    // the ingestion assembler (reader), so a paused agent's silence is not
    // finalized as an interrupted trace.
    let paused_agents: Arc<Mutex<std::collections::HashSet<reeve_model::ids::AgentId>>> =
        Arc::new(Mutex::new(std::collections::HashSet::new()));

    // Applied-command feed from the dispatcher (writer) to the engine
    // (reader), which measures whether each intervention improved quality.
    let applied_commands: Arc<Mutex<Vec<reeve_model::entity::intervention::AppliedCommand>>> =
        Arc::new(Mutex::new(Vec::new()));

    let (dispatch_tx, mut dispatch_rx) = tokio::sync::mpsc::channel::<(
        reeve_model::ids::AgentId,
        reeve_model::entity::intervention::InterventionCommand,
    )>(64);

    // Privacy tier gates SpanEvent content capture. Tier 1 (the default)
    // stores metadata only; tier 2+ stores content. Enabling capture is a
    // deliberate act (editing the config), and the consent log makes that
    // act auditable after the fact.
    let config_path = std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".config/reeve/config.toml"))
        .unwrap_or_else(|_| PathBuf::from(".config/reeve/config.toml"));
    let privacy_tier = reeve_engine::policy::config::load_privacy_tier(&config_path);
    if privacy_tier >= 2 {
        let consent_path = db_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("consent.log");
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let line = format!(
            "{now_ms} CONTENT_CAPTURE_ENABLED tier={privacy_tier} source={}\n",
            config_path.display()
        );
        if let Err(e) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&consent_path)
            .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()))
        {
            tracing::warn!(error = %e, "failed to write consent log");
        }
    }

    let engine_ingestion_rx = ingestion_tx.subscribe();
    tokio::spawn(reeve_ingestion::serve(
        addr,
        warm.clone(),
        ingestion_tx,
        ntp_offsets.clone(),
        paused_agents.clone(),
        privacy_tier >= 2,
    ));
    tokio::spawn(reeve_engine::run(
        engine_ingestion_rx,
        engine_event_tx.clone(),
        warm.clone(),
        Some(dispatch_tx),
        Some(applied_commands.clone()),
    ));
    let control_server = reeve_intervention::server::run(
        engine_event_tx.clone(),
        ntp_offsets,
        paused_agents.clone(),
    )
    .await;
    let audit_path = db_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("audit.log");
    // The audit trail is the permanent record of every intervention, so a
    // dispatcher that cannot write it is a fatal condition, not a degraded
    // mode. The retry path exists because the common cause is a permissions
    // problem the developer can fix in another terminal without losing the
    // already-running ingestion and engine tasks.
    let dispatcher = loop {
        match reeve_intervention::dispatcher::Dispatcher::new(
            control_server.clone(),
            warm.clone(),
            audit_path.clone(),
            paused_agents.clone(),
            applied_commands.clone(),
        ) {
            Ok(d) => break d,
            Err(e) => {
                let err = reeve_renderer::app::FatalError {
                    message: format!("cannot open audit log: {e}"),
                    hint: Some(format!("check permissions on {}", audit_path.display())),
                };
                match reeve_renderer::show_fatal(&err)? {
                    reeve_renderer::FatalOutcome::Retry => continue,
                    reeve_renderer::FatalOutcome::Quit => return Ok(()),
                }
            }
        }
    };

    let engine_dispatcher = dispatcher.clone();
    tokio::spawn(async move {
        while let Some((agent_id, command)) = dispatch_rx.recv().await {
            engine_dispatcher.dispatch(&agent_id, command).await;
        }
    });

    let notifications_enabled =
        reeve_engine::policy::config::load_notifications_enabled(&config_path);
    reeve_renderer::run(
        ingestion_rx,
        engine_event_rx,
        warm,
        ascii_mode,
        dispatcher,
        notifications_enabled,
    )
    .await?;

    Ok(())
}
