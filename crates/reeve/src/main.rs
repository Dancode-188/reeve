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

    // Ports are checked before anything binds them for real: a taken port
    // must be a loud fatal card, never a cockpit that renders and starves.
    loop {
        match check_ports_available(&[
            (4316, "control channel"),
            (addr.port(), "OTel ingestion"),
            (4318, "HTTP proxy"),
        ]) {
            Ok(()) => break,
            Err(err) => match reeve_renderer::show_fatal(&err)? {
                reeve_renderer::FatalOutcome::Retry => continue,
                reeve_renderer::FatalOutcome::Quit => return Ok(()),
            },
        }
    }

    let engine_ingestion_rx = ingestion_tx.subscribe();
    let proxy_addr: std::net::SocketAddr = "127.0.0.1:4318"
        .parse()
        .expect("static proxy address is valid");
    let disconnected_agents: reeve_ingestion::assemble::DisconnectedAgents =
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let proxy_interventions: reeve_model::entity::ProxyInterventions =
        std::sync::Arc::new(std::sync::Mutex::new(Default::default()));
    tokio::spawn(reeve_ingestion::serve(
        addr,
        proxy_addr,
        warm.clone(),
        ingestion_tx,
        ntp_offsets.clone(),
        paused_agents.clone(),
        disconnected_agents.clone(),
        proxy_interventions.clone(),
        privacy_tier >= 2,
    ));
    let reprobe_requested: reeve_engine::ReprobeRequested =
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    tokio::spawn(reeve_engine::run(
        engine_ingestion_rx,
        engine_event_tx.clone(),
        warm.clone(),
        Some(dispatch_tx),
        Some(applied_commands.clone()),
        Some(reprobe_requested.clone()),
    ));
    let control_server = reeve_intervention::server::run(
        engine_event_tx.clone(),
        ntp_offsets,
        paused_agents.clone(),
        disconnected_agents,
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
            Some(proxy_interventions.clone()),
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
        reprobe_requested,
    )
    .await?;

    Ok(())
}

/// Best-effort lookup of which process holds a local TCP port: the
/// socket inode from /proc/net/tcp, then a scan of same-user /proc fds.
/// Same-user visibility only, which covers the common case of a second
/// Reeve instance; anything else reports as unknown.
fn port_holder(port: u16) -> Option<String> {
    let tcp = std::fs::read_to_string("/proc/net/tcp").ok()?;
    let inode = tcp.lines().skip(1).find_map(|line| {
        let cols: Vec<&str> = line.split_whitespace().collect();
        let local = cols.get(1)?;
        let port_hex = local.split(':').nth(1)?;
        if u16::from_str_radix(port_hex, 16).ok()? == port {
            cols.get(9).map(|s| s.to_string())
        } else {
            None
        }
    })?;
    let target = format!("socket:[{inode}]");
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let pid = entry.file_name();
        let Some(pid_str) = pid.to_str() else {
            continue;
        };
        if !pid_str.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            if let Ok(link) = std::fs::read_link(fd.path()) {
                if link.to_string_lossy() == target {
                    let comm =
                        std::fs::read_to_string(entry.path().join("comm")).unwrap_or_default();
                    return Some(format!("{} (pid {})", comm.trim(), pid_str));
                }
            }
        }
    }
    None
}

/// Binding every port Reeve needs is a startup precondition. A port
/// already held used to produce a normal-looking cockpit that silently
/// received nothing, because the bind failure happened inside a spawned
/// task; checking up front turns that dead cockpit into a fatal card
/// that names the port and, when visible, the holder.
fn check_ports_available(ports: &[(u16, &str)]) -> Result<(), reeve_renderer::app::FatalError> {
    for (port, purpose) in ports {
        let addr = format!("127.0.0.1:{port}");
        if std::net::TcpListener::bind(&addr).is_err() {
            let holder = port_holder(*port)
                .map(|h| format!("held by {h}"))
                .unwrap_or_else(|| "held by another process".to_string());
            return Err(reeve_renderer::app::FatalError {
                message: format!("port {port} ({purpose}) is already in use, {holder}"),
                hint: Some(
                    "another Reeve may already be running; close it or wait for the port to free"
                        .to_string(),
                ),
            });
        }
    }
    Ok(())
}
