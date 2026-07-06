use crate::proto::{self, CommandType as ProtoCommandType, control_message};
use crate::server::ControlServer;
use crate::types::AckNotification;
use reeve_model::entity::intervention::{
    AckStatus, CommandStatus, CommandType, InterventionCommand,
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

struct PendingEntry {
    command: InterventionCommand,
    agent_id: AgentId,
    queued_at: Instant,
    retry_count: u8,
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
}

impl Dispatcher {
    pub fn new(
        server: Arc<ControlServer>,
        warm: Arc<WarmStore>,
        audit_path: PathBuf,
        paused: PausedAgents,
    ) -> Arc<Self> {
        let audit_log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&audit_path)
            .unwrap_or_else(|e| {
                panic!("failed to open audit log at {}: {e}", audit_path.display())
            });

        let (ack_tx, ack_rx) = mpsc::channel::<AckNotification>(64);
        server.register_ack_sink(ack_tx);

        let dispatcher = Arc::new(Dispatcher {
            server,
            pending: Arc::new(Mutex::new(HashMap::new())),
            applied: Arc::new(Mutex::new(HashSet::new())),
            warm,
            audit_log: Arc::new(Mutex::new(audit_log)),
            paused,
        });

        let d = dispatcher.clone();
        tokio::spawn(async move { d.ack_loop(ack_rx).await });

        let d = dispatcher.clone();
        tokio::spawn(async move { d.expiry_loop().await });

        dispatcher
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

        let to_process: Vec<(CommandId, AgentId, u8, bool)> = {
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
                    )
                })
                .collect()
        };

        for (command_id, agent_id, retry_count, past_expiry) in to_process {
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
        let (engine_tx, _rx) = broadcast::channel(8);
        let server = crate::server::new_for_test(engine_tx);
        let warm = Arc::new(WarmStore::open_in_memory().unwrap());
        let audit_path = std::env::temp_dir().join("reeve_test_audit.log");
        let paused = Arc::new(Mutex::new(HashSet::new()));
        Dispatcher::new(server, warm, audit_path, paused)
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
}
