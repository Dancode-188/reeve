use crate::proto::{self, CommandType as ProtoCommandType, control_message};
use crate::server::ControlServer;
use crate::types::AckNotification;
use reeve_model::entity::intervention::{
    AckStatus, AppliedCommand, CommandStatus, CommandType, InterventionCommand,
};
use reeve_model::ids::{AgentId, CommandId};
use reeve_storage::warm::WarmStore;
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio::time;

/// An ack has not arrived within this window: retry once, then expire.
const ACK_TIMEOUT: Duration = Duration::from_secs(30);

/// Agents currently paused by an applied Pause command. Shared with the
/// ingestion assembler, which suspends its idle timeout for these agents so
/// a paused agent's silence is not finalized as an interrupted trace.
pub type PausedAgents = Arc<Mutex<HashSet<AgentId>>>;

/// Commands confirmed applied, awaiting pickup by the engine's outcome
/// measurement. The dispatcher appends on every applied ack; the engine
/// drains on every completed trace.
pub type AppliedFeed = Arc<Mutex<Vec<AppliedCommand>>>;

struct PendingEntry {
    command: InterventionCommand,
    agent_id: AgentId,
    queued_at: Instant,
    retry_count: u8,
    /// Delivered through the proxy queue, not the control channel. The
    /// retry loop skips these: a proxy agent has no stream to resend on,
    /// and the command applies on the agent's next request, which can be
    /// well past the ack timeout. They stay in pending only so the ack
    /// can resolve them and feed the outcome tracker.
    via_proxy: bool,
}

/// Routes `InterventionCommand` records from the policy engine and the UI to
/// connected agent streams. Every dispatch and every ack is written to an
/// append-only audit log.
pub struct Dispatcher {
    server: Arc<ControlServer>,
    pending: Arc<Mutex<HashMap<CommandId, PendingEntry>>>,
    applied: Arc<Mutex<HashSet<CommandId>>>,
    warm: Arc<WarmStore>,
    audit_log: Arc<Mutex<File>>,
    paused: PausedAgents,
    applied_feed: AppliedFeed,
    /// Commands for proxy-path agents, applied by the proxy on the
    /// agent's next request. None only in tests.
    proxy_interventions: Option<reeve_model::entity::ProxyInterventions>,
}

impl Dispatcher {
    /// Fails when the audit log cannot be created or opened. The audit trail
    /// is the permanent record of every intervention; running without one is
    /// not an acceptable degraded mode, so the caller must treat this as a
    /// fatal startup condition rather than pressing on.
    pub fn new(
        server: Arc<ControlServer>,
        warm: Arc<WarmStore>,
        audit_path: PathBuf,
        paused: PausedAgents,
        applied_feed: AppliedFeed,
        proxy_interventions: Option<reeve_model::entity::ProxyInterventions>,
    ) -> Result<Arc<Self>, std::io::Error> {
        let audit_log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&audit_path)?;

        let (ack_tx, ack_rx) = mpsc::channel::<AckNotification>(64);
        server.register_ack_sink(ack_tx);

        let dispatcher = Arc::new(Dispatcher {
            server,
            pending: Arc::new(Mutex::new(HashMap::new())),
            applied: Arc::new(Mutex::new(HashSet::new())),
            warm,
            audit_log: Arc::new(Mutex::new(audit_log)),
            paused,
            applied_feed,
            proxy_interventions,
        });

        let d = dispatcher.clone();
        tokio::spawn(async move { d.ack_loop(ack_rx).await });

        let d = dispatcher.clone();
        tokio::spawn(async move { d.expiry_loop().await });

        // Proxy applications arrive through the shared queue rather than
        // the control channel; folding them into the same ack handling
        // keeps audit, pending bookkeeping, and outcome measurement blind
        // to which channel delivered the command.
        if dispatcher.proxy_interventions.is_some() {
            let d = dispatcher.clone();
            tokio::spawn(async move { d.proxy_applied_loop().await });
        }

        Ok(dispatcher)
    }

    /// Whether the agent is currently paused, as confirmed by an applied
    /// Pause ack that no applied Resume or Kill has since cleared. The UI
    /// uses this to make the pause key a toggle.
    pub fn is_paused(&self, agent_id: &AgentId) -> bool {
        self.paused.lock().unwrap().contains(agent_id)
    }

    /// Whether the circuit breaker is engaged for this agent. The
    /// renderer polls this to mark killed agents, the same way the
    /// paused set drives the paused status.
    pub fn is_killed(&self, agent_id: &AgentId) -> bool {
        self.proxy_interventions
            .as_ref()
            .is_some_and(|q| q.lock().unwrap().killed.contains(agent_id))
    }

    /// Route a command to the agent that owns the trace. Returns `true` if the
    /// command was sent on the wire. Returns `false` if the agent is not
    /// connected, the command is already past its `valid_until_ms`, or the
    /// command ID has been seen before (dedup guard).
    pub async fn dispatch(&self, agent_id: &AgentId, mut command: InterventionCommand) -> bool {
        let command_id = command.id.clone();
        let now_ms = current_ms();

        if self.applied.lock().unwrap().contains(&command_id) {
            tracing::debug!(command_id = %command_id, "dispatch skipped: already applied");
            return false;
        }

        if command.valid_until_ms < now_ms {
            command.status = CommandStatus::Expired;
            self.write_audit(format_args!(
                "{now_ms} EXPIRED cmd={command_id} agent={agent_id} \
                 type={} by={} reason=pre_dispatch\n",
                command_type_tag(&command.command_type),
                command.issued_by,
            ));
            return false;
        }

        let proto_cmd = domain_to_proto_command(&command);
        let sent = self
            .server
            .send_to_agent(
                agent_id,
                proto::ControlMessage {
                    payload: Some(control_message::Payload::Command(proto_cmd)),
                },
            )
            .await;

        if !sent {
            if self.queue_for_proxy(agent_id, &command).await {
                command.status = CommandStatus::Delivered;
                self.write_audit(format_args!(
                    "{now_ms} DISPATCH cmd={command_id} agent={agent_id} \
                     type={} by={} status=queued channel=proxy\n",
                    command_type_tag(&command.command_type),
                    command.issued_by,
                ));
                // Track it in pending like a control-channel command, so
                // the proxy's later ack can resolve it and feed the
                // outcome tracker. via_proxy keeps the retry loop off it.
                self.pending.lock().unwrap().insert(
                    command_id.clone(),
                    PendingEntry {
                        command: command.clone(),
                        agent_id: agent_id.clone(),
                        queued_at: Instant::now(),
                        retry_count: 0,
                        via_proxy: true,
                    },
                );
                let warm = self.warm.clone();
                let persisted = command.clone();
                tokio::spawn(async move {
                    if let Err(e) = warm.save_intervention_command(persisted).await {
                        tracing::warn!(error = %e, "failed to persist proxy command");
                    }
                });
                return true;
            }
            command.status = CommandStatus::Failed;
            self.write_audit(format_args!(
                "{now_ms} DISPATCH cmd={command_id} agent={agent_id} \
                 type={} by={} status=failed reason=not_connected\n",
                command_type_tag(&command.command_type),
                command.issued_by,
            ));
            return false;
        }

        command.status = CommandStatus::Delivered;
        self.write_audit(format_args!(
            "{now_ms} DISPATCH cmd={command_id} agent={agent_id} \
             type={} by={} status=delivered\n",
            command_type_tag(&command.command_type),
            command.issued_by,
        ));

        self.pending.lock().unwrap().insert(
            command_id.clone(),
            PendingEntry {
                command: command.clone(),
                agent_id: agent_id.clone(),
                queued_at: Instant::now(),
                retry_count: 0,
                via_proxy: false,
            },
        );

        let warm = self.warm.clone();
        tokio::spawn(async move {
            if let Err(e) = warm.save_intervention_command(command).await {
                tracing::warn!(error = %e, "failed to persist dispatched command");
            }
        });

        true
    }

    /// Queues a command for proxy application when the target is a
    /// proxy-path agent and the command type survives without a control
    /// channel. Pause and kill do not: pause has no safe hold on this
    /// path, and kill has different semantics owned elsewhere.
    async fn queue_for_proxy(&self, agent_id: &AgentId, command: &InterventionCommand) -> bool {
        let Some(ref queue) = self.proxy_interventions else {
            return false;
        };
        let payload = match &command.command_type {
            CommandType::Redirect { instruction } => {
                Some(reeve_model::entity::ProxyPayload::Redirect {
                    instruction: instruction.clone(),
                })
            }
            CommandType::InjectContext { context } => {
                Some(reeve_model::entity::ProxyPayload::InjectContext {
                    context: context.clone(),
                })
            }
            // Kill is the circuit breaker: engaged below, not queued.
            CommandType::Kill => None,
            // Resume against an engaged breaker is the revive: the one
            // recovery short of restarting Reeve. Anything else has no
            // proxy meaning.
            CommandType::Resume => {
                let mut q = queue.lock().unwrap();
                if q.killed.remove(agent_id) {
                    q.applied
                        .push((command.id.clone(), agent_id.clone(), current_ms()));
                    return true;
                }
                return false;
            }
            _ => return false,
        };
        let is_proxy_agent = matches!(
            self.warm.get_agent(agent_id).await,
            Ok(Some(agent)) if agent.integration == reeve_model::entity::IntegrationPath::Proxy
        );
        if !is_proxy_agent {
            return false;
        }
        match payload {
            Some(payload) => {
                queue
                    .lock()
                    .unwrap()
                    .pending
                    .entry(agent_id.clone())
                    .or_default()
                    .push_back(reeve_model::entity::ProxyCommand {
                        id: command.id.clone(),
                        payload,
                        valid_until_ms: command.valid_until_ms,
                    });
            }
            None => {
                // The breaker is effective the moment it is set, so the
                // application is reported immediately: enforcement is
                // local, not on the agent.
                let mut q = queue.lock().unwrap();
                q.killed.insert(agent_id.clone());
                q.applied
                    .push((command.id.clone(), agent_id.clone(), current_ms()));
            }
        }
        true
    }

    /// Folds proxy applications into the same ack handling the control
    /// channel uses, so downstream bookkeeping cannot tell the channels
    /// apart.
    async fn proxy_applied_loop(&self) {
        let queue = self
            .proxy_interventions
            .clone()
            .expect("loop only spawned when the queue exists");
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(500));
        loop {
            tick.tick().await;
            let drained: Vec<reeve_model::entity::intervention::ProxyApplied> =
                std::mem::take(&mut queue.lock().unwrap().applied);
            for (command_id, agent_id, _applied_at_ms) in drained {
                self.handle_ack(AckNotification {
                    command_id,
                    agent_id,
                    status: AckStatus::Applied,
                })
                .await;
            }
        }
    }

    async fn ack_loop(&self, mut rx: mpsc::Receiver<AckNotification>) {
        while let Some(notif) = rx.recv().await {
            self.handle_ack(notif).await;
        }
    }

    async fn handle_ack(&self, notif: AckNotification) {
        let now_ms = current_ms();
        let AckNotification {
            command_id,
            agent_id,
            status,
        } = notif;

        self.write_audit(format_args!(
            "{now_ms} ACK cmd={command_id} agent={agent_id} status={}\n",
            ack_status_tag(status),
        ));

        let mut command = {
            let mut pending = self.pending.lock().unwrap();
            match pending.get_mut(&command_id) {
                Some(entry) => {
                    let cmd = entry.command.clone();
                    // Terminal states remove from pending.
                    if matches!(
                        status,
                        AckStatus::Applied
                            | AckStatus::Failed
                            | AckStatus::Expired
                            | AckStatus::Cancelled
                    ) {
                        pending.remove(&command_id);
                        self.applied.lock().unwrap().insert(command_id.clone());
                    }
                    cmd
                }
                None => {
                    tracing::debug!(command_id = %command_id, "ack for unknown or already-settled command");
                    return;
                }
            }
        };

        // Pause state flips only on Applied: the agent has confirmed it is
        // actually holding at a checkpoint, not merely that the command was
        // received. Kill clears it too, since a killed agent is no longer
        // paused and its trace should be allowed to finalize.
        if status == AckStatus::Applied {
            match command.command_type {
                CommandType::Pause => {
                    self.paused.lock().unwrap().insert(agent_id.clone());
                }
                CommandType::Resume | CommandType::Kill => {
                    self.paused.lock().unwrap().remove(&agent_id);
                }
                _ => {}
            }
            self.applied_feed.lock().unwrap().push(AppliedCommand {
                command_id: command_id.clone(),
                trace_id: command.trace_id.clone(),
                agent_id: agent_id.clone(),
                command_type: command.command_type.clone(),
                applied_at_ms: now_ms,
            });
        }

        command.status = ack_to_command_status(status);
        command.acknowledged_at = Some(now_ms);

        let warm = self.warm.clone();
        tokio::spawn(async move {
            if let Err(e) = warm.save_intervention_command(command).await {
                tracing::warn!(error = %e, "failed to persist command ack");
            }
        });
    }

    async fn expiry_loop(&self) {
        let mut interval = time::interval(Duration::from_secs(5));
        loop {
            interval.tick().await;
            self.expire_and_retry().await;
        }
    }

    async fn expire_and_retry(&self) {
        let now_ms = current_ms();
        let now = Instant::now();

        let to_process: Vec<(CommandId, AgentId, u8, bool, bool)> = {
            let pending = self.pending.lock().unwrap();
            pending
                .iter()
                .filter(|(_, e)| now.duration_since(e.queued_at) >= ACK_TIMEOUT)
                .map(|(id, e)| {
                    (
                        id.clone(),
                        e.agent_id.clone(),
                        e.retry_count,
                        e.command.valid_until_ms < now_ms,
                        e.via_proxy,
                    )
                })
                .collect()
        };

        for (command_id, agent_id, retry_count, past_expiry, via_proxy) in to_process {
            if via_proxy {
                // A proxy command has no control channel to resend on. It
                // waits in pending for the proxy to apply it on the next
                // request, and only leaves on its own validity window.
                if past_expiry {
                    self.settle_expired(&command_id, &agent_id, now_ms).await;
                }
                continue;
            }
            if past_expiry || retry_count >= 1 {
                self.settle_expired(&command_id, &agent_id, now_ms).await;
            } else {
                self.retry(&command_id, &agent_id, now_ms).await;
            }
        }
    }

    async fn retry(&self, command_id: &CommandId, agent_id: &AgentId, now_ms: i64) {
        let (proto_cmd, issued_by) = {
            let mut pending = self.pending.lock().unwrap();
            match pending.get_mut(command_id) {
                Some(entry) => {
                    entry.retry_count += 1;
                    entry.queued_at = Instant::now();
                    (
                        domain_to_proto_command(&entry.command),
                        entry.command.issued_by.clone(),
                    )
                }
                None => return,
            }
        };

        self.write_audit(format_args!(
            "{now_ms} RETRY cmd={command_id} agent={agent_id} by={issued_by} attempt=2\n",
        ));

        let sent = self
            .server
            .send_to_agent(
                agent_id,
                proto::ControlMessage {
                    payload: Some(control_message::Payload::Command(proto_cmd)),
                },
            )
            .await;

        if !sent {
            self.settle_expired(command_id, agent_id, now_ms).await;
        }
    }

    async fn settle_expired(&self, command_id: &CommandId, agent_id: &AgentId, now_ms: i64) {
        let command = {
            let mut pending = self.pending.lock().unwrap();
            match pending.remove(command_id) {
                Some(entry) => {
                    self.applied.lock().unwrap().insert(command_id.clone());
                    entry.command
                }
                None => return,
            }
        };

        self.write_audit(format_args!(
            "{now_ms} EXPIRED cmd={command_id} agent={agent_id} \
             type={} by={} reason=ack_timeout\n",
            command_type_tag(&command.command_type),
            command.issued_by,
        ));

        let mut expired = command;
        expired.status = CommandStatus::Expired;
        expired.acknowledged_at = Some(now_ms);

        let warm = self.warm.clone();
        tokio::spawn(async move {
            if let Err(e) = warm.save_intervention_command(expired).await {
                tracing::warn!(error = %e, "failed to persist expired command");
            }
        });
    }

    fn write_audit(&self, args: std::fmt::Arguments<'_>) {
        let line = std::fmt::format(args);
        let mut log = self.audit_log.lock().unwrap();
        let _ = log.write_all(line.as_bytes());
    }
}

fn domain_to_proto_command(command: &InterventionCommand) -> proto::InterventionCommand {
    let (cmd_type, payload) = match &command.command_type {
        CommandType::Pause => (ProtoCommandType::Pause as i32, String::new()),
        CommandType::Resume => (ProtoCommandType::Resume as i32, String::new()),
        CommandType::Kill => (ProtoCommandType::Kill as i32, String::new()),
        CommandType::Redirect { instruction } => {
            (ProtoCommandType::Redirect as i32, instruction.clone())
        }
        CommandType::InjectContext { context } => {
            (ProtoCommandType::InjectContext as i32, context.clone())
        }
    };
    proto::InterventionCommand {
        command_id: command.id.to_string(),
        trace_id: command.trace_id.to_string(),
        span_id: command.span_id.as_deref().unwrap_or("").to_string(),
        r#type: cmd_type,
        payload,
        issued_by: command.issued_by.clone(),
        valid_until_ms: command.valid_until_ms,
        requires_confirmation: command.requires_confirmation,
    }
}

fn ack_to_command_status(ack: AckStatus) -> CommandStatus {
    match ack {
        AckStatus::Received | AckStatus::Applying => CommandStatus::Delivered,
        AckStatus::Applied => CommandStatus::Applied,
        AckStatus::Failed => CommandStatus::Failed,
        AckStatus::Expired => CommandStatus::Expired,
        AckStatus::Cancelled => CommandStatus::Cancelled,
    }
}

fn command_type_tag(ct: &CommandType) -> &'static str {
    match ct {
        CommandType::Pause => "Pause",
        CommandType::Resume => "Resume",
        CommandType::Kill => "Kill",
        CommandType::Redirect { .. } => "Redirect",
        CommandType::InjectContext { .. } => "InjectContext",
    }
}

fn ack_status_tag(s: AckStatus) -> &'static str {
    match s {
        AckStatus::Received => "received",
        AckStatus::Applying => "applying",
        AckStatus::Applied => "applied",
        AckStatus::Failed => "failed",
        AckStatus::Expired => "expired",
        AckStatus::Cancelled => "cancelled",
    }
}

fn current_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use reeve_model::entity::intervention::CommandType;
    use reeve_model::ids::{CommandId, TraceId};
    use tokio::sync::broadcast;

    fn make_dispatcher() -> Arc<Dispatcher> {
        make_dispatcher_with_queue(None).0
    }

    fn make_dispatcher_with_queue(
        queue: Option<reeve_model::entity::ProxyInterventions>,
    ) -> (Arc<Dispatcher>, Arc<WarmStore>) {
        let (engine_tx, _rx) = broadcast::channel(8);
        let server = crate::server::new_for_test(engine_tx);
        let warm = Arc::new(WarmStore::open_in_memory().unwrap());
        let audit_path = std::env::temp_dir().join("reeve_test_audit.log");
        let paused = Arc::new(Mutex::new(HashSet::new()));
        let applied_feed = Arc::new(Mutex::new(Vec::new()));
        let d = Dispatcher::new(
            server,
            warm.clone(),
            audit_path,
            paused,
            applied_feed,
            queue,
        )
        .unwrap();
        (d, warm)
    }

    fn pending_command(command_type: CommandType) -> InterventionCommand {
        InterventionCommand {
            id: CommandId::from("cmd-pending"),
            trace_id: TraceId::from("trace-1"),
            span_id: None,
            policy_id: None,
            command_type,
            status: CommandStatus::Delivered,
            requires_confirmation: false,
            issued_at: 0,
            acknowledged_at: None,
            issued_by: "human".to_string(),
            valid_until_ms: i64::MAX,
        }
    }

    async fn ack_applied(d: &Dispatcher, agent_id: &AgentId, command_type: CommandType) {
        let command = pending_command(command_type);
        d.pending.lock().unwrap().insert(
            command.id.clone(),
            PendingEntry {
                command: command.clone(),
                agent_id: agent_id.clone(),
                queued_at: Instant::now(),
                retry_count: 0,
                via_proxy: false,
            },
        );
        d.handle_ack(AckNotification {
            command_id: command.id,
            agent_id: agent_id.clone(),
            status: AckStatus::Applied,
        })
        .await;
        // The applied set carries over between calls in a test; clear it so
        // a subsequent command with the same ID is not treated as a dup.
        d.applied.lock().unwrap().clear();
    }

    fn expired_command() -> (AgentId, InterventionCommand) {
        let agent_id = AgentId::from("agent-test");
        let command = InterventionCommand {
            id: CommandId::from("cmd-expired"),
            trace_id: TraceId::from("trace-1"),
            span_id: None,
            policy_id: None,
            command_type: CommandType::Pause,
            status: CommandStatus::Pending,
            requires_confirmation: false,
            issued_at: 0,
            acknowledged_at: None,
            issued_by: "policy:test".to_string(),
            valid_until_ms: 1, // epoch 1ms — always in the past
        };
        (agent_id, command)
    }

    #[tokio::test]
    async fn dispatcher_initializes_empty() {
        let d = make_dispatcher();
        assert!(d.pending.lock().unwrap().is_empty());
        assert!(d.applied.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn dispatch_rejects_expired_command() {
        let d = make_dispatcher();
        let (agent_id, command) = expired_command();
        assert!(!d.dispatch(&agent_id, command).await);
        assert!(d.pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn dispatch_rejects_duplicate_applied_command() {
        let d = make_dispatcher();
        let command_id = CommandId::from("cmd-dup");
        d.applied.lock().unwrap().insert(command_id.clone());

        let agent_id = AgentId::from("agent-dup");
        let command = InterventionCommand {
            id: command_id,
            trace_id: TraceId::from("trace-2"),
            span_id: None,
            policy_id: None,
            command_type: CommandType::Pause,
            status: CommandStatus::Pending,
            requires_confirmation: false,
            issued_at: 0,
            acknowledged_at: None,
            issued_by: "human".to_string(),
            valid_until_ms: i64::MAX,
        };
        assert!(!d.dispatch(&agent_id, command).await);
    }

    #[tokio::test]
    async fn dispatch_returns_false_when_agent_not_connected() {
        let d = make_dispatcher();
        let agent_id = AgentId::from("agent-absent");
        let command = InterventionCommand {
            id: CommandId::from("cmd-no-agent"),
            trace_id: TraceId::from("trace-3"),
            span_id: None,
            policy_id: None,
            command_type: CommandType::Redirect {
                instruction: "stop looping".to_string(),
            },
            status: CommandStatus::Pending,
            requires_confirmation: false,
            issued_at: 0,
            acknowledged_at: None,
            issued_by: "human".to_string(),
            valid_until_ms: i64::MAX,
        };
        assert!(!d.dispatch(&agent_id, command).await);
    }

    #[tokio::test]
    async fn new_fails_when_audit_path_is_unwritable() {
        let (engine_tx, _rx) = broadcast::channel(8);
        let server = crate::server::new_for_test(engine_tx);
        let warm = Arc::new(WarmStore::open_in_memory().unwrap());
        // Parent directory does not exist, so the open must fail.
        let audit_path = std::env::temp_dir().join("reeve_no_such_dir/audit.log");
        let paused = Arc::new(Mutex::new(HashSet::new()));
        let applied_feed = Arc::new(Mutex::new(Vec::new()));

        let result = Dispatcher::new(server, warm, audit_path, paused, applied_feed, None);
        assert!(
            result.is_err(),
            "an unopenable audit log must be an error, not a panic"
        );
    }

    #[tokio::test]
    async fn applied_pause_marks_agent_paused() {
        let d = make_dispatcher();
        let agent_id = AgentId::from("agent-pause");

        ack_applied(&d, &agent_id, CommandType::Pause).await;

        assert!(
            d.paused.lock().unwrap().contains(&agent_id),
            "agent must be marked paused after an applied Pause ack"
        );
    }

    #[tokio::test]
    async fn applied_resume_clears_pause_state() {
        let d = make_dispatcher();
        let agent_id = AgentId::from("agent-resume");

        ack_applied(&d, &agent_id, CommandType::Pause).await;
        ack_applied(&d, &agent_id, CommandType::Resume).await;

        assert!(
            !d.paused.lock().unwrap().contains(&agent_id),
            "applied Resume must clear the pause state"
        );
    }

    #[tokio::test]
    async fn applied_kill_clears_pause_state() {
        let d = make_dispatcher();
        let agent_id = AgentId::from("agent-kill");

        ack_applied(&d, &agent_id, CommandType::Pause).await;
        ack_applied(&d, &agent_id, CommandType::Kill).await;

        assert!(
            !d.paused.lock().unwrap().contains(&agent_id),
            "a killed agent is not paused; its traces must be allowed to finalize"
        );
    }

    #[tokio::test]
    async fn applied_ack_lands_in_applied_feed() {
        let d = make_dispatcher();
        let agent_id = AgentId::from("agent-feed");

        ack_applied(
            &d,
            &agent_id,
            CommandType::Redirect {
                instruction: "steer".to_string(),
            },
        )
        .await;

        let feed = d.applied_feed.lock().unwrap();
        assert_eq!(feed.len(), 1, "applied command must reach the feed");
        assert_eq!(feed[0].agent_id, agent_id);
        assert_eq!(feed[0].trace_id.as_str(), "trace-1");
    }

    #[tokio::test]
    async fn proxy_command_reaches_pending_and_the_applied_feed() {
        // Regression for #272. A proxy command used to return from dispatch
        // before the pending insert, so its later ack found nothing and no
        // outcome was ever measured on the proxy path. It must land in
        // pending and, on ack, reach the applied feed like an SDK command.
        let queue: reeve_model::entity::ProxyInterventions =
            Arc::new(Mutex::new(Default::default()));
        let (d, warm) = make_dispatcher_with_queue(Some(queue.clone()));

        let agent_id = reeve_model::ids::agent_id_from_service("claude-cli", "proxy");
        warm.upsert_agent(reeve_model::entity::Agent {
            id: agent_id.clone(),
            name: "claude-cli".to_string(),
            framework: "proxy".to_string(),
            integration: reeve_model::entity::IntegrationPath::Proxy,
            status: reeve_model::entity::AgentStatus::Idle,
            first_seen_at: 0,
            last_seen_at: 0,
        })
        .await
        .unwrap();

        let command = pending_command(CommandType::Redirect {
            instruction: "wrap up".to_string(),
        });
        let command_id = command.id.clone();
        assert!(d.dispatch(&agent_id, command).await, "queued to the proxy");
        assert!(
            d.pending.lock().unwrap().contains_key(&command_id),
            "the proxy command is tracked in pending"
        );

        // The proxy applies it on the agent's next request and acks back.
        d.handle_ack(AckNotification {
            command_id,
            agent_id: agent_id.clone(),
            status: AckStatus::Applied,
        })
        .await;

        let feed = d.applied_feed.lock().unwrap();
        assert_eq!(feed.len(), 1, "the applied proxy command reaches the feed");
        assert_eq!(feed[0].agent_id, agent_id);
    }

    #[tokio::test]
    async fn received_ack_does_not_reach_applied_feed() {
        let d = make_dispatcher();
        let agent_id = AgentId::from("agent-feed-recv");
        let command = pending_command(CommandType::Pause);

        d.pending.lock().unwrap().insert(
            command.id.clone(),
            PendingEntry {
                command: command.clone(),
                agent_id: agent_id.clone(),
                queued_at: Instant::now(),
                retry_count: 0,
                via_proxy: false,
            },
        );
        d.handle_ack(AckNotification {
            command_id: command.id,
            agent_id,
            status: AckStatus::Received,
        })
        .await;

        assert!(
            d.applied_feed.lock().unwrap().is_empty(),
            "only applied acks feed outcome measurement"
        );
    }

    #[tokio::test]
    async fn is_paused_tracks_pause_resume_cycle() {
        let d = make_dispatcher();
        let agent_id = AgentId::from("agent-toggle");

        assert!(!d.is_paused(&agent_id), "fresh agent is not paused");
        ack_applied(&d, &agent_id, CommandType::Pause).await;
        assert!(d.is_paused(&agent_id), "applied pause must be visible");
        ack_applied(&d, &agent_id, CommandType::Resume).await;
        assert!(!d.is_paused(&agent_id), "applied resume must clear it");
    }

    #[tokio::test]
    async fn received_ack_does_not_mark_paused() {
        let d = make_dispatcher();
        let agent_id = AgentId::from("agent-received");
        let command = pending_command(CommandType::Pause);

        d.pending.lock().unwrap().insert(
            command.id.clone(),
            PendingEntry {
                command: command.clone(),
                agent_id: agent_id.clone(),
                queued_at: Instant::now(),
                retry_count: 0,
                via_proxy: false,
            },
        );
        d.handle_ack(AckNotification {
            command_id: command.id,
            agent_id: agent_id.clone(),
            status: AckStatus::Received,
        })
        .await;

        assert!(
            !d.paused.lock().unwrap().contains(&agent_id),
            "pause state flips on Applied, not on Received"
        );
    }

    #[tokio::test]
    async fn kill_against_a_proxy_agent_engages_the_breaker() {
        let queue: reeve_model::entity::ProxyInterventions =
            Arc::new(Mutex::new(Default::default()));
        let (d, warm) = make_dispatcher_with_queue(Some(queue.clone()));

        let agent_id = reeve_model::ids::agent_id_from_service("claude-cli", "proxy");
        warm.upsert_agent(reeve_model::entity::Agent {
            id: agent_id.clone(),
            name: "claude-cli".to_string(),
            framework: "proxy".to_string(),
            integration: reeve_model::entity::IntegrationPath::Proxy,
            status: reeve_model::entity::AgentStatus::Idle,
            first_seen_at: 0,
            last_seen_at: 0,
        })
        .await
        .unwrap();

        let command = InterventionCommand {
            id: CommandId::from("cmd-kill"),
            trace_id: TraceId::from("trace-1"),
            span_id: None,
            policy_id: None,
            command_type: CommandType::Kill,
            status: CommandStatus::Pending,
            requires_confirmation: false,
            issued_at: current_ms(),
            acknowledged_at: None,
            issued_by: "human".to_string(),
            valid_until_ms: i64::MAX,
        };
        assert!(d.dispatch(&agent_id, command).await);

        let q = queue.lock().unwrap();
        assert!(q.killed.contains(&agent_id), "the breaker is engaged");
        assert_eq!(
            q.applied.len(),
            1,
            "kill reports applied immediately: enforcement is local"
        );
        assert!(
            q.pending.get(&agent_id).is_none_or(|p| p.is_empty()),
            "kill is a breaker, never a queued payload"
        );
    }

    #[tokio::test]
    async fn resume_revives_a_killed_proxy_agent() {
        // The one recovery short of restarting Reeve (#214): Resume
        // against an engaged breaker clears it and reports applied.
        let queue: reeve_model::entity::ProxyInterventions =
            Arc::new(Mutex::new(Default::default()));
        let (d, warm) = make_dispatcher_with_queue(Some(queue.clone()));

        let agent_id = reeve_model::ids::agent_id_from_service("claude-cli", "proxy");
        warm.upsert_agent(reeve_model::entity::Agent {
            id: agent_id.clone(),
            name: "claude-cli".to_string(),
            framework: "proxy".to_string(),
            integration: reeve_model::entity::IntegrationPath::Proxy,
            status: reeve_model::entity::AgentStatus::Idle,
            first_seen_at: 0,
            last_seen_at: 0,
        })
        .await
        .unwrap();
        queue.lock().unwrap().killed.insert(agent_id.clone());
        assert!(d.is_killed(&agent_id), "breaker visible through the query");

        let revive = pending_command(CommandType::Resume);
        assert!(d.dispatch(&agent_id, revive).await);
        assert!(!d.is_killed(&agent_id), "the breaker is cleared");
        assert_eq!(
            queue.lock().unwrap().applied.len(),
            1,
            "revive reports applied immediately, like the kill it undoes"
        );

        // Resume against a proxy agent that is NOT killed has nothing to
        // do and must not claim success.
        let idle_resume = pending_command(CommandType::Resume);
        assert!(!d.dispatch(&agent_id, idle_resume).await);
    }
}
