//! The seam between the engine and everything downstream of it.
//!
//! The engine's unit tests can only ask what `PolicyEngine::evaluate`
//! returns. What the cockpit actually receives is an `EngineEvent` on a
//! broadcast channel, assembled by `run` from a rule, a store lookup and
//! a shared capability map, and every defect recorded here lived in that
//! assembly rather than in any one of its parts.
//!
//! Every scenario reproduces something that shipped. Each drives the real
//! engine over real channels against a real store, because a unit test on
//! either side of a seam agrees with itself by construction.

use reeve_model::entity::agent::{Agent, AgentStatus, IntegrationPath};
use reeve_model::entity::intervention::{CommandType, InterventionCommand, LiveCapabilities};
use reeve_model::entity::policy::{PolicyRule, RuleScope};
use reeve_model::ids::{AgentId, RuleId, TraceId};
use reeve_model::signal::{EngineEvent, IngestionEvent};
use reeve_storage::warm::WarmStore;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc};

const RULE: &str = "seam_fires_on_any_trace";

/// A rule that matches any completed trace, so a scenario needs only to
/// produce one. Waiting on a builtin means also reproducing the health
/// score that trips it, which is a different test's job.
///
/// `requires_confirmation` decides where a runnable command goes: false
/// sends it straight down the dispatch channel, true parks it in the store
/// for the operator. Only the first is observable from out here.
fn any_trace_rule(requires_confirmation: bool) -> PolicyRule {
    PolicyRule {
        id: RuleId::from(RULE),
        name: "Fires on any trace".to_string(),
        description: "A trace completed.".to_string(),
        trigger_condition: "span_count >= 1".to_string(),
        command_type: CommandType::Pause,
        requires_confirmation,
        cooldown_secs: 1,
        scope: RuleScope::Global,
        enabled: true,
        auto_confirm_after_secs: None,
    }
}

fn agent(id: &str, integration: IntegrationPath) -> Agent {
    Agent {
        id: AgentId::from(id),
        name: id.to_string(),
        framework: "test".to_string(),
        integration,
        status: AgentStatus::Running,
        first_seen_at: 0,
        last_seen_at: 0,
    }
}

struct Harness {
    ingestion_tx: broadcast::Sender<IngestionEvent>,
    engine_rx: broadcast::Receiver<EngineEvent>,
    dispatched: mpsc::Receiver<(AgentId, InterventionCommand)>,
}

/// Stands up the real engine over real channels. `agents` are written to
/// the store first, because the engine reads each agent's integration path
/// back out when it decides what can be commanded.
async fn harness(agents: Vec<Agent>, live: HashMap<AgentId, Vec<String>>) -> Harness {
    harness_with(agents, live, true).await
}

async fn harness_with(
    agents: Vec<Agent>,
    live: HashMap<AgentId, Vec<String>>,
    requires_confirmation: bool,
) -> Harness {
    let warm = Arc::new(WarmStore::open_in_memory().unwrap());
    warm.save_policy_rule(any_trace_rule(requires_confirmation))
        .await
        .unwrap();
    for a in agents {
        warm.upsert_agent(a).await.unwrap();
    }

    let (ingestion_tx, ingestion_rx) = broadcast::channel(64);
    let (engine_tx, engine_rx) = broadcast::channel(64);
    let (dispatch_tx, dispatched) = mpsc::channel(16);
    let live_capabilities: LiveCapabilities = Arc::new(Mutex::new(live));

    tokio::spawn(reeve_engine::run(reeve_engine::EngineConfig {
        ingestion_rx,
        engine_tx,
        warm,
        dispatch_tx: Some(dispatch_tx),
        applied_commands: None,
        reprobe_requested: None,
        live_capabilities: Some(live_capabilities),
        capture_root: None,
    }));
    // The engine probes its evaluation backend and loads rules before it
    // reads the first event; publishing into that gap loses the event.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    Harness {
        ingestion_tx,
        engine_rx,
        dispatched,
    }
}

impl Harness {
    fn complete_trace(&self, trace: &str, agent_id: &str) {
        self.ingestion_tx
            .send(IngestionEvent::TraceCompleted {
                trace_id: TraceId::from(trace),
                agent_id: AgentId::from(agent_id),
                span_count: 3,
                cost: 0.01,
            })
            .unwrap();
    }

    /// The next alert raised by the rule under test, ignoring the health
    /// and evaluation traffic that shares the channel. Returns the agent it
    /// names and the command it offers.
    async fn next_alert(&mut self) -> (AgentId, Option<String>) {
        let deadline = std::time::Duration::from_secs(10);
        tokio::time::timeout(deadline, async {
            loop {
                match self.engine_rx.recv().await.unwrap() {
                    EngineEvent::PolicyAlert {
                        agent_id,
                        rule_id,
                        command_type,
                        ..
                    } if rule_id == RULE => return (agent_id, command_type),
                    _ => continue,
                }
            }
        })
        .await
        .expect("the rule matches every completed trace, so an alert must arrive")
    }
}

/// Found and fixed in #297 without an issue of its own: `PolicyAlert`
/// carried no agent id, so the renderer attributed every alert to whichever
/// agent was selected and would have dispatched there. Invisible with one
/// agent, because then the wrong answer and the right one are the same
/// value.
#[tokio::test]
async fn an_alert_names_the_agent_whose_rule_fired_not_another_one() {
    let mut h = harness(
        vec![
            agent("first-agent", IntegrationPath::Sdk),
            agent("second-agent", IntegrationPath::Sdk),
        ],
        HashMap::from([
            (AgentId::from("first-agent"), vec!["pause".to_string()]),
            (AgentId::from("second-agent"), vec!["pause".to_string()]),
        ]),
    )
    .await;

    h.complete_trace("trace-for-second", "second-agent");

    let (named, _) = h.next_alert().await;
    assert_eq!(
        named,
        AgentId::from("second-agent"),
        "the alert must name the agent that produced the trace"
    );
}

/// #296 and ADR-0045. The suggested command is attached only
/// when the target can run it, and the alert is raised either way. These
/// three agents cover the three answers, and the engine has to reach a
/// different source for each: the shared handshake map for the first, the
/// stored integration path for the other two.
#[tokio::test]
async fn what_the_alert_offers_depends_on_how_the_agent_can_be_reached() {
    let mut h = harness(
        vec![
            agent("handshaken", IntegrationPath::Sdk),
            agent("proxied", IntegrationPath::Proxy),
            agent("otlp-only", IntegrationPath::Sdk),
        ],
        // Only the first holds a live control stream. `otlp-only` is on the
        // SDK path with no entry here, which is #296 exactly: spans arrive
        // over OTLP whether or not anything is listening for commands.
        HashMap::from([(AgentId::from("handshaken"), vec!["pause".to_string()])]),
    )
    .await;

    h.complete_trace("trace-a", "handshaken");
    let (named, command) = h.next_alert().await;
    assert_eq!(named, AgentId::from("handshaken"));
    assert_eq!(
        command.as_deref(),
        Some("pause"),
        "a live handshake declaring pause makes pause offerable"
    );

    h.complete_trace("trace-b", "proxied");
    let (named, command) = h.next_alert().await;
    assert_eq!(named, AgentId::from("proxied"));
    assert_eq!(
        command, None,
        "a proxy agent cannot be paused, but the condition still matched"
    );

    h.complete_trace("trace-c", "otlp-only");
    let (named, command) = h.next_alert().await;
    assert_eq!(named, AgentId::from("otlp-only"));
    assert_eq!(
        command, None,
        "no control stream means nothing can be sent, whatever the path says"
    );
}

/// The other half of ADR-0045: withholding the suggestion also has to
/// withhold the dispatch. An alert that offers nothing must not quietly
/// send the command anyway, which would be the dead letter #274 started
/// with, just invisible.
#[tokio::test]
async fn nothing_is_dispatched_to_an_agent_that_cannot_run_it() {
    let mut h = harness_with(
        vec![
            agent("proxied", IntegrationPath::Proxy),
            agent("handshaken", IntegrationPath::Sdk),
        ],
        HashMap::from([(AgentId::from("handshaken"), vec!["pause".to_string()])]),
        // Auto-confirming, so a runnable command goes straight out rather
        // than parking in the store where this test cannot see it.
        false,
    )
    .await;

    h.complete_trace("trace-proxy", "proxied");
    let (named, command) = h.next_alert().await;
    assert_eq!(named, AgentId::from("proxied"));
    assert_eq!(command, None, "a proxy agent cannot be paused");
    assert!(
        h.dispatched.try_recv().is_err(),
        "nothing may be sent to an agent that cannot apply it"
    );

    // The same rule against an agent that CAN pause still dispatches, so
    // the assertion above is about capability and not a broken channel.
    h.complete_trace("trace-sdk", "handshaken");
    let (named, command) = h.next_alert().await;
    assert_eq!(named, AgentId::from("handshaken"));
    assert_eq!(command.as_deref(), Some("pause"));
    let (target, sent) =
        tokio::time::timeout(std::time::Duration::from_secs(5), h.dispatched.recv())
            .await
            .expect("a runnable command must reach the dispatcher")
            .expect("dispatch channel closed");
    assert_eq!(target, AgentId::from("handshaken"));
    assert_eq!(sent.command_type, CommandType::Pause);
}
