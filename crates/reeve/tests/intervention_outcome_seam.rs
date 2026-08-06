//! The seam ADR-0029 built on purpose, and the one nothing crossed.
//!
//! `reeve-engine` may not depend on `reeve-intervention`, so a command the
//! dispatcher applies reaches the engine only as an `AppliedCommand` on a
//! shared feed. Each side is well covered alone: the dispatcher has nine
//! tests on what an ack does to its own state, and the engine's outcome
//! tracker has its own. Nothing had ever run the two together, which means
//! nothing checked that a command applied on one side becomes a measured
//! outcome on the other.
//!
//! This lives in the binary crate because that is the only place allowed to
//! see both. Moving it into either one would need the dependency that ADR
//! exists to forbid.

use reeve_model::entity::agent::{Agent, AgentStatus, IntegrationPath};
use reeve_model::entity::intervention::{
    CommandStatus, CommandType, InterventionCommand, ProxyInterventions,
};
use reeve_model::entity::trace::{Trace, TraceStatus};
use reeve_model::ids::{AgentId, CommandId, TraceId, current_ms};
use reeve_model::signal::{EngineEvent, IngestionEvent};
use reeve_storage::warm::WarmStore;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

const AGENT: &str = "proxied-agent";
const COMMAND: &str = "cmd-under-measurement";

fn redirect_command() -> InterventionCommand {
    InterventionCommand {
        id: CommandId::from(COMMAND),
        trace_id: TraceId::from("trace-that-prompted-it"),
        span_id: None,
        policy_id: None,
        command_type: CommandType::Redirect {
            instruction: "narrow the search".to_string(),
        },
        status: CommandStatus::Pending,
        requires_confirmation: false,
        issued_at: current_ms(),
        acknowledged_at: None,
        issued_by: "human".to_string(),
        valid_until_ms: current_ms() + 60_000,
    }
}

/// A free port for the control server. It never accepts a connection here:
/// the dispatcher needs a server handle to exist, and this test drives the
/// proxy path, which acks without any control channel at all.
async fn free_addr() -> std::net::SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    drop(l);
    addr
}

#[tokio::test]
async fn a_command_the_dispatcher_applies_becomes_an_outcome_the_engine_stores() {
    let warm = Arc::new(WarmStore::open_in_memory().unwrap());
    warm.upsert_agent(Agent {
        id: AgentId::from(AGENT),
        name: AGENT.to_string(),
        framework: "test".to_string(),
        // Proxy, so the command is applied on the request path and acked
        // without a control channel.
        integration: IntegrationPath::Proxy,
        status: AgentStatus::Running,
        first_seen_at: 0,
        last_seen_at: 0,
    })
    .await
    .unwrap();

    // An outcome row references both the command and the trace that
    // prompted it, so those have to exist before the measurement lands.
    // Without them the engine measures the outcome, fails to store it, and
    // says so only in a warning: the seam looks broken when the fixture is.
    warm.save_trace(Trace {
        id: TraceId::from("trace-that-prompted-it"),
        agent_id: AgentId::from(AGENT),
        status: TraceStatus::Completed,
        start_time: current_ms(),
        end_time: Some(current_ms()),
        root_span_id: None,
        final_health_score: None,
    })
    .await
    .unwrap();

    // The seam itself: one allocation, held by both sides, which is the
    // entire interface between them.
    let applied_feed: reeve_engine::AppliedCommands = Arc::new(Mutex::new(Vec::new()));
    let proxy_queue: ProxyInterventions = Arc::new(Mutex::new(Default::default()));

    let (engine_tx, mut engine_rx) = broadcast::channel(256);
    let (ingestion_tx, ingestion_rx) = broadcast::channel(64);

    let control = reeve_intervention::server::run_on(
        free_addr().await,
        engine_tx.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashSet::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
    )
    .await;

    let audit = tempfile::NamedTempFile::new().unwrap();
    let dispatcher = reeve_intervention::dispatcher::Dispatcher::new(
        control,
        warm.clone(),
        audit.path().to_path_buf(),
        Arc::new(Mutex::new(HashSet::new())),
        applied_feed.clone(),
        Some(proxy_queue.clone()),
    )
    .unwrap();

    tokio::spawn(reeve_engine::run(
        ingestion_rx,
        engine_tx,
        warm.clone(),
        None,
        Some(applied_feed.clone()),
        None,
        None,
    ));
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    let complete_trace = |n: usize| {
        ingestion_tx
            .send(IngestionEvent::TraceCompleted {
                trace_id: TraceId::from(format!("trace-{n}").as_str()),
                agent_id: AgentId::from(AGENT),
                span_count: 3,
                cost: 0.01,
            })
            .unwrap();
    };

    // One scored trace before the command, so the measurement has a
    // before-picture to compare against.
    complete_trace(0);
    wait_for_health(&mut engine_rx).await;

    assert!(
        dispatcher
            .dispatch(&AgentId::from(AGENT), redirect_command())
            .await,
        "a redirect is something a proxy agent can take"
    );

    // What the proxy does when the next request carries the command. The
    // dispatcher folds this into the same ack handling the control channel
    // uses, which is the behaviour being relied on here.
    proxy_queue.lock().unwrap().applied.push((
        CommandId::from(COMMAND),
        AgentId::from(AGENT),
        current_ms(),
    ));

    // The applied queue is drained on a timer rather than a signal.
    tokio::time::sleep(std::time::Duration::from_millis(900)).await;
    assert_eq!(
        applied_feed.lock().unwrap().len(),
        1,
        "the application should have crossed onto the shared feed"
    );

    // Three scored traces after it: the engine picks the command up on the
    // first of them and completes the measurement on the third.
    for n in 1..=3 {
        complete_trace(n);
        wait_for_health(&mut engine_rx).await;
    }

    let outcome = warm
        .get_intervention_outcome(&CommandId::from(COMMAND))
        .await
        .unwrap()
        .expect("the applied command should have produced a stored outcome");
    assert_eq!(outcome.trace_id.as_str(), "trace-that-prompted-it");
    assert!(
        outcome.pre_intervention_score.is_some(),
        "the score from before the command is what the measurement compares against"
    );
    assert!(
        outcome.post_intervention_score.is_some(),
        "and the three traces after it are what it compares to"
    );
}

/// Waits until the engine has finished scoring a trace, which is the point
/// at which it has also drained the feed and fed the outcome tracker.
async fn wait_for_health(rx: &mut broadcast::Receiver<EngineEvent>) {
    let deadline = std::time::Duration::from_secs(10);
    tokio::time::timeout(deadline, async {
        loop {
            if let Ok(EngineEvent::HealthScoreUpdated { .. }) = rx.recv().await {
                return;
            }
        }
    })
    .await
    .expect("the engine scores every completed trace");
    // Scoring emits before the outcome bookkeeping that follows it.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
}
