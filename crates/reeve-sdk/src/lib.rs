pub mod checkpoint;
pub mod span;

pub use checkpoint::{AgentError, CheckpointResult};
pub use span::{LlmSpan, ToolSpan};

mod proto {
    tonic::include_proto!("reeve");
}

use opentelemetry_otlp::WithExportConfig;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{Notify, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tracing;

/// Configuration for [`ReeveSdk::connect`].
pub struct SdkConfig {
    pub agent_id: String,
    pub framework: String,
    pub capabilities: Vec<String>,
    /// Host running Reeve. Defaults to `"127.0.0.1"`.
    pub host: String,
}

impl SdkConfig {
    pub fn new(agent_id: impl Into<String>) -> Self {
        Self {
            agent_id: agent_id.into(),
            framework: "custom".to_string(),
            capabilities: vec![
                "pause".to_string(),
                "redirect".to_string(),
                "inject_context".to_string(),
                "kill".to_string(),
            ],
            host: "127.0.0.1".to_string(),
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum SdkError {
    #[error("connection failed: {0}")]
    Connection(#[from] tonic::transport::Error),
    #[error("stream error: {0}")]
    Status(#[from] tonic::Status),
    #[error("telemetry setup failed: {0}")]
    Telemetry(String),
}

struct PendingCommand {
    command_id: String,
    variant: CommandVariant,
}

enum CommandVariant {
    Kill,
    Pause,
    Redirect(String),
    InjectContext(String),
}

pub struct ReeveSdk {
    outbound_tx: mpsc::Sender<proto::AgentMessage>,
    pending: Arc<Mutex<Option<PendingCommand>>>,
    pause_notify: Arc<Notify>,
}

impl ReeveSdk {
    /// Connect to the Reeve control channel on port 4316, send the handshake,
    /// and install an OTel OTLP exporter pointing at port 4317. Returns an
    /// `Arc<ReeveSdk>` so the handle can be shared across async tasks.
    pub async fn connect(config: SdkConfig) -> Result<Arc<Self>, SdkError> {
        setup_otel(&config.host).map_err(|e| SdkError::Telemetry(e.to_string()))?;

        let control_endpoint = format!("http://{}:4316", config.host);
        let mut client =
            proto::reeve_control_client::ReeveControlClient::connect(control_endpoint).await?;

        let (outbound_tx, outbound_rx) = mpsc::channel::<proto::AgentMessage>(64);
        let outbound_stream = ReceiverStream::new(outbound_rx);

        // Queue the handshake before the stream is consumed by the RPC call.
        // The channel buffer guarantees it arrives as the first message.
        let t1 = now_ms();
        let _ = outbound_tx
            .send(proto::AgentMessage {
                payload: Some(proto::agent_message::Payload::Handshake(
                    proto::AgentHandshake {
                        agent_id: config.agent_id.clone(),
                        framework: config.framework.clone(),
                        sdk_version: env!("CARGO_PKG_VERSION").to_string(),
                        capabilities: config.capabilities.clone(),
                        t1_ms: t1,
                    },
                )),
            })
            .await;

        let response = client.control_stream(outbound_stream).await?;
        let mut inbound = response.into_inner();

        let pending: Arc<Mutex<Option<PendingCommand>>> = Arc::new(Mutex::new(None));
        let pause_notify = Arc::new(Notify::new());

        let pending_bg = pending.clone();
        let pause_notify_bg = pause_notify.clone();
        let outbound_tx_bg = outbound_tx.clone();

        tokio::spawn(async move {
            while let Ok(Some(msg)) = inbound.message().await {
                use proto::control_message::Payload;
                match msg.payload {
                    Some(Payload::HandshakeAck(ack)) => {
                        let t4 = now_ms();
                        tracing::debug!(
                            t2 = ack.t2_ms,
                            t3 = ack.t3_ms,
                            t4,
                            "NTP timestamps exchanged",
                        );
                        let _ = outbound_tx_bg
                            .send(proto::AgentMessage {
                                payload: Some(proto::agent_message::Payload::NtpFollowup(
                                    proto::NtpFollowup { t4_ms: t4 },
                                )),
                            })
                            .await;
                    }
                    Some(Payload::Command(cmd)) => {
                        if cmd.valid_until_ms > 0 && now_ms() > cmd.valid_until_ms {
                            let _ = outbound_tx_bg
                                .send(make_ack(&cmd.command_id, 5 /* EXPIRED */))
                                .await;
                            continue;
                        }

                        let _ = outbound_tx_bg
                            .send(make_ack(&cmd.command_id, 1 /* RECEIVED */))
                            .await;

                        let cmd_type = cmd.r#type;

                        if cmd_type == proto::CommandType::Resume as i32 {
                            *pending_bg.lock().unwrap() = None;
                            pause_notify_bg.notify_one();
                            continue;
                        }

                        let variant = if cmd_type == proto::CommandType::Kill as i32 {
                            CommandVariant::Kill
                        } else if cmd_type == proto::CommandType::Pause as i32 {
                            CommandVariant::Pause
                        } else if cmd_type == proto::CommandType::Redirect as i32 {
                            CommandVariant::Redirect(cmd.payload)
                        } else if cmd_type == proto::CommandType::InjectContext as i32 {
                            CommandVariant::InjectContext(cmd.payload)
                        } else {
                            continue;
                        };

                        tracing::debug!(
                            command_id = %cmd.command_id,
                            cmd_type,
                            "intervention command received",
                        );
                        *pending_bg.lock().unwrap() = Some(PendingCommand {
                            command_id: cmd.command_id,
                            variant,
                        });
                    }
                    Some(Payload::Heartbeat(_)) => {
                        let _ = outbound_tx_bg
                            .send(proto::AgentMessage {
                                payload: Some(proto::agent_message::Payload::Heartbeat(
                                    proto::Heartbeat {
                                        timestamp_ms: now_ms(),
                                    },
                                )),
                            })
                            .await;
                    }
                    None => {}
                }
            }
            tracing::info!("reeve control stream closed");
        });

        let outbound_tx_hb = outbound_tx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                interval.tick().await;
                if outbound_tx_hb
                    .send(proto::AgentMessage {
                        payload: Some(proto::agent_message::Payload::Heartbeat(proto::Heartbeat {
                            timestamp_ms: now_ms(),
                        })),
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        Ok(Arc::new(Self {
            outbound_tx,
            pending,
            pause_notify,
        }))
    }

    /// Call this at every safe yield point in the agent loop. Blocks on Pause
    /// until a Resume arrives. Returns immediately for any other pending
    /// command or when nothing is pending.
    pub async fn checkpoint(&self) -> Result<CheckpointResult, AgentError> {
        loop {
            let pending = self.pending.lock().unwrap().take();
            match pending {
                None => return Ok(CheckpointResult::Continue),
                Some(PendingCommand {
                    command_id,
                    variant: CommandVariant::Kill,
                }) => {
                    self.send_ack(&command_id, 3 /* APPLIED */).await;
                    return Err(AgentError::Killed);
                }
                Some(PendingCommand {
                    command_id,
                    variant: CommandVariant::Pause,
                }) => {
                    self.send_ack(&command_id, 2 /* APPLYING */).await;
                    self.pause_notify.notified().await;
                    self.send_ack(&command_id, 3 /* APPLIED */).await;
                    // loop back: another command may have arrived while paused
                }
                Some(PendingCommand {
                    command_id,
                    variant: CommandVariant::Redirect(s),
                }) => {
                    self.send_ack(&command_id, 2 /* APPLYING */).await;
                    return Ok(CheckpointResult::Redirect(s));
                }
                Some(PendingCommand {
                    command_id,
                    variant: CommandVariant::InjectContext(s),
                }) => {
                    self.send_ack(&command_id, 2 /* APPLYING */).await;
                    return Ok(CheckpointResult::Context(s));
                }
            }
        }
    }

    /// Start an LLM call span. Call [`LlmSpan::set_token_usage`] before the
    /// guard is dropped to attach token counts to the span.
    pub fn llm_span(&self) -> LlmSpan {
        use opentelemetry::trace::Tracer;
        LlmSpan::new(opentelemetry::global::tracer("reeve-sdk").start("llm.call"))
    }

    /// Start a tool-call span named `name`.
    pub fn tool_span(&self, name: &str) -> ToolSpan {
        use opentelemetry::trace::Tracer;
        ToolSpan::new(opentelemetry::global::tracer("reeve-sdk").start(name.to_string()))
    }

    async fn send_ack(&self, command_id: &str, status: i32) {
        let _ = self.outbound_tx.send(make_ack(command_id, status)).await;
    }
}

fn make_ack(command_id: &str, status: i32) -> proto::AgentMessage {
    proto::AgentMessage {
        payload: Some(proto::agent_message::Payload::Ack(proto::CommandAck {
            command_id: command_id.to_string(),
            status,
            message: String::new(),
        })),
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn setup_otel(host: &str) -> Result<(), opentelemetry::trace::TraceError> {
    let endpoint = format!("http://{}:4317", host);
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()?;
    let provider = opentelemetry_sdk::trace::TracerProvider::builder()
        .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
        .build();
    opentelemetry::global::set_tracer_provider(provider);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sdk_with_pending(
        variant: CommandVariant,
    ) -> (ReeveSdk, mpsc::Receiver<proto::AgentMessage>) {
        let (tx, rx) = mpsc::channel(8);
        let pending = Arc::new(Mutex::new(Some(PendingCommand {
            command_id: "cmd-1".to_string(),
            variant,
        })));
        let sdk = ReeveSdk {
            outbound_tx: tx,
            pending,
            pause_notify: Arc::new(Notify::new()),
        };
        (sdk, rx)
    }

    #[test]
    fn sdk_config_defaults() {
        let cfg = SdkConfig::new("my-agent");
        assert_eq!(cfg.agent_id, "my-agent");
        assert_eq!(cfg.host, "127.0.0.1");
        assert!(cfg.capabilities.contains(&"pause".to_string()));
        assert!(cfg.capabilities.contains(&"kill".to_string()));
    }

    #[tokio::test]
    async fn checkpoint_returns_continue_when_empty() {
        let (tx, _rx) = mpsc::channel(8);
        let sdk = ReeveSdk {
            outbound_tx: tx,
            pending: Arc::new(Mutex::new(None)),
            pause_notify: Arc::new(Notify::new()),
        };
        let result = sdk.checkpoint().await.unwrap();
        assert!(matches!(result, CheckpointResult::Continue));
    }

    #[tokio::test]
    async fn checkpoint_returns_killed_on_kill_command() {
        let (sdk, _rx) = sdk_with_pending(CommandVariant::Kill);
        let err = sdk.checkpoint().await.unwrap_err();
        assert!(matches!(err, AgentError::Killed));
    }

    #[tokio::test]
    async fn checkpoint_returns_redirect_instruction() {
        let (sdk, _rx) = sdk_with_pending(CommandVariant::Redirect("slow down".to_string()));
        let result = sdk.checkpoint().await.unwrap();
        assert!(matches!(result, CheckpointResult::Redirect(s) if s == "slow down"));
    }

    #[tokio::test]
    async fn checkpoint_returns_context() {
        let (sdk, _rx) = sdk_with_pending(CommandVariant::InjectContext(
            r#"{"hint":"be concise"}"#.to_string(),
        ));
        let result = sdk.checkpoint().await.unwrap();
        assert!(matches!(result, CheckpointResult::Context(s) if s.contains("be concise")));
    }

    #[tokio::test]
    async fn kill_sends_applied_ack() {
        let (sdk, mut rx) = sdk_with_pending(CommandVariant::Kill);
        let _ = sdk.checkpoint().await;

        let msg = rx.recv().await.unwrap();
        match msg.payload.unwrap() {
            proto::agent_message::Payload::Ack(ack) => {
                assert_eq!(ack.command_id, "cmd-1");
                assert_eq!(ack.status, 3); // APPLIED
            }
            _ => panic!("expected ack"),
        }
    }

    #[tokio::test]
    async fn redirect_sends_applying_ack() {
        let (sdk, mut rx) = sdk_with_pending(CommandVariant::Redirect("try again".to_string()));
        let _ = sdk.checkpoint().await;

        let msg = rx.recv().await.unwrap();
        match msg.payload.unwrap() {
            proto::agent_message::Payload::Ack(ack) => {
                assert_eq!(ack.status, 2); // APPLYING
            }
            _ => panic!("expected ack"),
        }
    }

    #[test]
    fn agent_error_display() {
        let e = AgentError::Killed;
        assert!(e.to_string().contains("terminated"));
    }
}
