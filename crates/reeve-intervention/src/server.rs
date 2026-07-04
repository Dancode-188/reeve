use crate::proto::{
    self, agent_message, control_message, reeve_control_server::ReeveControl,
    reeve_control_server::ReeveControlServer,
};
use crate::types::AckNotification;
use reeve_model::entity::intervention::AckStatus;
use reeve_model::ids::AgentId;
use reeve_model::signal::EngineEvent;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

struct AgentStreamEntry {
    tx: mpsc::Sender<Result<proto::ControlMessage, Status>>,
    capabilities: Vec<String>,
    last_seen: Instant,
}

/// Handle to the running control server. Used by the dispatcher to send
/// commands to connected agents.
#[derive(Clone)]
pub struct ControlServer {
    connected: Arc<Mutex<HashMap<AgentId, AgentStreamEntry>>>,
    engine_tx: broadcast::Sender<EngineEvent>,
    ack_sink: Arc<Mutex<Option<mpsc::Sender<AckNotification>>>>,
}

impl ControlServer {
    fn new(engine_tx: broadcast::Sender<EngineEvent>) -> Self {
        Self {
            connected: Arc::new(Mutex::new(HashMap::new())),
            engine_tx,
            ack_sink: Arc::new(Mutex::new(None)),
        }
    }

    /// Register a channel through which the server forwards `CommandAck`
    /// messages to the dispatcher. Call this once after the dispatcher is
    /// constructed.
    pub fn register_ack_sink(&self, tx: mpsc::Sender<AckNotification>) {
        *self.ack_sink.lock().unwrap() = Some(tx);
    }

    /// Send a control message to a connected agent. Returns `true` if the
    /// agent was connected and the send succeeded.
    pub async fn send_to_agent(&self, agent_id: &AgentId, command: proto::ControlMessage) -> bool {
        let tx = {
            let map = self.connected.lock().unwrap();
            map.get(agent_id).map(|e| e.tx.clone())
        };
        match tx {
            Some(tx) => tx.send(Ok(command)).await.is_ok(),
            None => false,
        }
    }

    /// Return the declared capabilities for a connected agent, or `None` if
    /// the agent is not connected.
    pub fn agent_capabilities(&self, agent_id: &AgentId) -> Option<Vec<String>> {
        self.connected
            .lock()
            .unwrap()
            .get(agent_id)
            .map(|e| e.capabilities.clone())
    }

    /// Return the IDs of all currently connected agents.
    pub fn connected_agent_ids(&self) -> Vec<AgentId> {
        self.connected.lock().unwrap().keys().cloned().collect()
    }
}

#[tonic::async_trait]
impl ReeveControl for ControlServer {
    type ControlStreamStream = ReceiverStream<Result<proto::ControlMessage, Status>>;

    async fn control_stream(
        &self,
        request: Request<Streaming<proto::AgentMessage>>,
    ) -> Result<Response<Self::ControlStreamStream>, Status> {
        let mut inbound = request.into_inner();

        // First message must be AgentHandshake.
        let first = inbound
            .message()
            .await
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::invalid_argument("stream closed before handshake"))?;

        let handshake = match first.payload {
            Some(agent_message::Payload::Handshake(h)) => h,
            _ => {
                return Err(Status::invalid_argument(
                    "first message must be AgentHandshake",
                ));
            }
        };

        let agent_id = AgentId::from(handshake.agent_id.as_str());
        let capabilities = handshake.capabilities.clone();

        let (tx, rx) = mpsc::channel::<Result<proto::ControlMessage, Status>>(32);

        // NTP: record T2 and T3. The formula is applied in reeve-intervention
        // once issue #7 lands; timestamps are captured here so no handshake
        // round-trip is needed later.
        let t2_ms = current_ms();
        let t3_ms = current_ms();
        let _ = tx
            .send(Ok(proto::ControlMessage {
                payload: Some(control_message::Payload::HandshakeAck(
                    proto::HandshakeAck { t2_ms, t3_ms },
                )),
            }))
            .await;

        {
            let mut map = self.connected.lock().unwrap();
            map.insert(
                agent_id.clone(),
                AgentStreamEntry {
                    tx: tx.clone(),
                    capabilities: capabilities.clone(),
                    last_seen: Instant::now(),
                },
            );
        }

        let _ = self.engine_tx.send(EngineEvent::AgentControlConnected {
            agent_id: agent_id.clone(),
            capabilities: capabilities.clone(),
        });

        tracing::info!(agent_id = %agent_id, ?capabilities, "agent connected to control channel");

        let connected = self.connected.clone();
        let engine_tx = self.engine_tx.clone();
        let ack_sink = self.ack_sink.clone();

        tokio::spawn(async move {
            while let Ok(Some(msg)) = inbound.message().await {
                match msg.payload {
                    Some(agent_message::Payload::Heartbeat(_)) => {
                        if let Some(entry) = connected.lock().unwrap().get_mut(&agent_id) {
                            entry.last_seen = Instant::now();
                        }
                    }
                    Some(agent_message::Payload::Ack(ack)) => {
                        tracing::debug!(
                            agent_id = %agent_id,
                            command_id = %ack.command_id,
                            status = ack.status,
                            "command ack received",
                        );
                        if let Some(status) = proto_ack_to_domain(ack.status) {
                            let sink = ack_sink.lock().unwrap().clone();
                            if let Some(tx) = sink {
                                let _ = tx.try_send(AckNotification {
                                    command_id: ack.command_id.as_str().into(),
                                    agent_id: agent_id.clone(),
                                    status,
                                });
                            }
                        }
                    }
                    Some(agent_message::Payload::NtpFollowup(f)) => {
                        tracing::debug!(
                            agent_id = %agent_id,
                            t4_ms = f.t4_ms,
                            "NTP followup received",
                        );
                    }
                    Some(agent_message::Payload::Handshake(_)) | None => {}
                }
            }

            connected.lock().unwrap().remove(&agent_id);
            let _ = engine_tx.send(EngineEvent::AgentControlDisconnected {
                agent_id: agent_id.clone(),
            });
            tracing::info!(agent_id = %agent_id, "agent disconnected from control channel");
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

/// Start the gRPC control server on `127.0.0.1:4316` and return a handle
/// to it. The handle can be used by the dispatcher to send commands to
/// connected agents.
pub async fn run(engine_tx: broadcast::Sender<EngineEvent>) -> Arc<ControlServer> {
    let server = ControlServer::new(engine_tx);
    let handle = Arc::new(server.clone());

    let addr = "127.0.0.1:4316"
        .parse()
        .expect("control server address is hardcoded and valid");

    tokio::spawn(async move {
        tracing::info!(%addr, "control server listening");
        if let Err(e) = tonic::transport::Server::builder()
            .add_service(ReeveControlServer::new(server))
            .serve(addr)
            .await
        {
            tracing::error!(error = %e, "control server failed");
        }
    });

    handle
}

fn proto_ack_to_domain(status: i32) -> Option<AckStatus> {
    match status {
        1 => Some(AckStatus::Received),
        2 => Some(AckStatus::Applying),
        3 => Some(AckStatus::Applied),
        4 => Some(AckStatus::Failed),
        5 => Some(AckStatus::Expired),
        6 => Some(AckStatus::Cancelled),
        _ => None,
    }
}

fn current_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// Create an `Arc<ControlServer>` without binding any port. For use in
/// unit tests that need a real server handle but not a live gRPC socket.
#[cfg(test)]
pub fn new_for_test(engine_tx: broadcast::Sender<EngineEvent>) -> Arc<ControlServer> {
    Arc::new(ControlServer::new(engine_tx))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_server() -> ControlServer {
        let (tx, _rx) = broadcast::channel(8);
        ControlServer::new(tx)
    }

    #[test]
    fn no_agents_connected_initially() {
        let server = make_server();
        assert!(server.connected_agent_ids().is_empty());
    }

    #[test]
    fn capabilities_returns_none_for_unknown_agent() {
        let server = make_server();
        assert!(
            server
                .agent_capabilities(&AgentId::from("unknown"))
                .is_none()
        );
    }

    #[tokio::test]
    async fn send_to_agent_returns_false_when_not_connected() {
        let server = make_server();
        let msg = proto::ControlMessage { payload: None };
        assert!(!server.send_to_agent(&AgentId::from("nobody"), msg).await);
    }
}
