use reeve_model::entity::{
    Agent, AgentStatus, CommandStatus, EvaluationResult, EvaluatorType, EventType, InternalSpan,
    IntegrationPath, InterventionCommand, SpanEvent, SpanStatus, TargetType, Trace, TraceStatus,
};
use reeve_model::ids::{AgentId, CommandId, EvalId, SpanId, TraceId};
use rusqlite::{Connection, Row, params};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("blocking task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}

/// Embedded at compile time so the binary doesn't depend on a migrations
/// directory existing next to wherever it's installed.
const MIGRATIONS: &[(i64, &str)] = &[(1, include_str!("../../../migrations/0001_initial.sql"))];

fn run_migrations(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS _schema_migrations (
            version    INTEGER PRIMARY KEY,
            applied_at INTEGER NOT NULL
        );",
    )?;

    let applied: HashSet<i64> = conn
        .prepare("SELECT version FROM _schema_migrations")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;

    for (version, sql) in MIGRATIONS {
        if applied.contains(version) {
            continue;
        }
        conn.execute_batch(sql)?;
        let applied_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_millis() as i64;
        conn.execute(
            "INSERT INTO _schema_migrations (version, applied_at) VALUES (?1, ?2)",
            params![version, applied_at],
        )?;
    }
    Ok(())
}

/// Round-trips a unit-only enum through a bare SQL TEXT value (no quotes),
/// reusing the Serialize/Deserialize already derived on the reeve-model
/// entities rather than hand-writing a second string mapping per enum.
/// Not used for CommandType, which carries data inline and needs the full
/// JSON shape, quotes included.
fn enum_to_text<T: Serialize>(value: &T) -> Result<String, StorageError> {
    Ok(serde_json::to_string(value)?.trim_matches('"').to_string())
}

fn text_to_enum<T: DeserializeOwned>(text: &str) -> Result<T, StorageError> {
    Ok(serde_json::from_str(&format!("\"{text}\""))?)
}

fn row_to_trace(row: &Row) -> rusqlite::Result<Trace> {
    let status: String = row.get("status")?;
    Ok(Trace {
        id: row.get::<_, String>("id")?.into(),
        agent_id: row.get::<_, String>("agent_id")?.into(),
        status: text_to_enum::<TraceStatus>(&status).map_err(rusqlite_serde_err)?,
        start_time: row.get("started_at")?,
        end_time: row.get("completed_at")?,
        root_span_id: row
            .get::<_, Option<String>>("root_span_id")?
            .map(Into::into),
        final_health_score: row.get("final_health_score")?,
    })
}

fn row_to_span(row: &Row) -> rusqlite::Result<InternalSpan> {
    let status: String = row.get("status")?;
    let attributes: String = row.get("attributes")?;
    let raw_attributes: String = row.get("raw_attributes")?;
    Ok(InternalSpan {
        id: row.get::<_, String>("id")?.into(),
        trace_id: row.get::<_, String>("trace_id")?.into(),
        parent_id: row.get::<_, Option<String>>("parent_id")?.map(Into::into),
        operation: row.get("operation")?,
        status: text_to_enum::<SpanStatus>(&status).map_err(rusqlite_serde_err)?,
        start_time: row.get("start_time")?,
        end_time: row.get("end_time")?,
        arrived_at: row.get("arrived_at")?,
        attributes: serde_json::from_str(&attributes).map_err(rusqlite_serde_err)?,
        raw_attributes: serde_json::from_str(&raw_attributes).map_err(rusqlite_serde_err)?,
    })
}

fn row_to_span_event(row: &Row) -> rusqlite::Result<SpanEvent> {
    let event_type: String = row.get("event_type")?;
    Ok(SpanEvent {
        id: row.get::<_, String>("id")?.into(),
        span_id: row.get::<_, String>("span_id")?.into(),
        event_type: text_to_enum::<EventType>(&event_type).map_err(rusqlite_serde_err)?,
        occurred_at: row.get("occurred_at")?,
        content: row.get("content")?,
    })
}

fn row_to_agent(row: &Row) -> rusqlite::Result<Agent> {
    let integration: String = row.get("integration")?;
    let status: String = row.get("status")?;
    Ok(Agent {
        id: row.get::<_, String>("id")?.into(),
        name: row.get("name")?,
        framework: row.get("framework")?,
        integration: text_to_enum::<IntegrationPath>(&integration).map_err(rusqlite_serde_err)?,
        status: text_to_enum::<AgentStatus>(&status).map_err(rusqlite_serde_err)?,
        first_seen_at: row.get("first_seen_at")?,
        last_seen_at: row.get("last_seen_at")?,
    })
}

fn row_to_evaluation_result(row: &Row) -> rusqlite::Result<EvaluationResult> {
    let target_type: String = row.get("target_type")?;
    let evaluator: String = row.get("evaluator")?;
    Ok(EvaluationResult {
        id: row.get::<_, String>("id")?.into(),
        target_id: row.get("target_id")?,
        target_type: text_to_enum::<TargetType>(&target_type).map_err(rusqlite_serde_err)?,
        metric: row.get("metric")?,
        score: row.get("score")?,
        evaluator: text_to_enum::<EvaluatorType>(&evaluator).map_err(rusqlite_serde_err)?,
        evaluated_at: row.get("evaluated_at")?,
        judge_model_version: row.get("judge_model_version")?,
    })
}

fn row_to_intervention_command(row: &Row) -> rusqlite::Result<InterventionCommand> {
    let command_type: String = row.get("command_type")?;
    let status: String = row.get("status")?;
    Ok(InterventionCommand {
        id: row.get::<_, String>("id")?.into(),
        trace_id: row.get::<_, String>("trace_id")?.into(),
        span_id: row.get::<_, Option<String>>("span_id")?.map(Into::into),
        policy_id: row.get::<_, Option<String>>("policy_id")?.map(Into::into),
        command_type: serde_json::from_str(&command_type).map_err(rusqlite_serde_err)?,
        status: text_to_enum::<CommandStatus>(&status).map_err(rusqlite_serde_err)?,
        requires_confirmation: row.get("requires_confirmation")?,
        issued_at: row.get("issued_at")?,
        acknowledged_at: row.get("acknowledged_at")?,
        issued_by: row.get("issued_by")?,
        valid_until_ms: row.get("valid_until_ms")?,
    })
}

/// rusqlite's row-mapping closures must return `rusqlite::Result`, so JSON
/// errors encountered while decoding a row (whether from `StorageError` via
/// `text_to_enum` or a bare `serde_json::Error` from a direct call) get
/// wrapped to fit that shape.
fn rusqlite_serde_err(e: impl std::fmt::Display) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(e.to_string().into())
}

pub struct WarmStore {
    conn: Arc<Mutex<Connection>>,
}

impl WarmStore {
    pub fn open(path: &Path) -> Result<Self, StorageError> {
        let conn = Connection::open(path)?;
        run_migrations(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn open_in_memory() -> Result<Self, StorageError> {
        let conn = Connection::open_in_memory()?;
        run_migrations(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Bridges rusqlite's sync API into the async callers in
    /// reeve-ingestion/reeve-renderer. Every warm tier operation goes
    /// through this rather than hiding the spawn_blocking behind a
    /// generic async trait, so the sync-in-async wrapping stays visible
    /// in one place.
    async fn with_conn<F, T>(&self, f: F) -> Result<T, StorageError>
    where
        F: FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        let result: rusqlite::Result<T> = tokio::task::spawn_blocking(move || {
            let conn = conn.lock().expect("warm store mutex poisoned");
            f(&conn)
        })
        .await?;
        Ok(result?)
    }

    pub async fn upsert_agent(&self, agent: Agent) -> Result<(), StorageError> {
        let integration = enum_to_text(&agent.integration)?;
        let status = enum_to_text(&agent.status)?;
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO agents (id, name, framework, integration, status, first_seen_at, last_seen_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(id) DO UPDATE SET
                    status = excluded.status,
                    last_seen_at = excluded.last_seen_at",
                params![
                    agent.id.as_str(),
                    agent.name,
                    agent.framework,
                    integration,
                    status,
                    agent.first_seen_at,
                    agent.last_seen_at,
                ],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn save_trace(&self, trace: Trace) -> Result<(), StorageError> {
        let status = enum_to_text(&trace.status)?;
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO traces
                    (id, agent_id, status, started_at, completed_at, root_span_id, final_health_score)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(id) DO UPDATE SET
                    status = excluded.status,
                    completed_at = excluded.completed_at,
                    root_span_id = excluded.root_span_id,
                    final_health_score = excluded.final_health_score",
                params![
                    trace.id.as_str(),
                    trace.agent_id.as_str(),
                    status,
                    trace.start_time,
                    trace.end_time,
                    trace.root_span_id.as_deref(),
                    trace.final_health_score,
                ],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn get_trace(&self, id: &TraceId) -> Result<Option<Trace>, StorageError> {
        let id = id.clone();
        self.with_conn(move |conn| {
            conn.query_row(
                "SELECT * FROM traces WHERE id = ?1",
                params![id.as_str()],
                row_to_trace,
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                e => Err(e),
            })
        })
        .await
    }

    pub async fn list_traces_for_agent(
        &self,
        agent_id: &AgentId,
    ) -> Result<Vec<Trace>, StorageError> {
        let agent_id = agent_id.clone();
        self.with_conn(move |conn| {
            conn.prepare("SELECT * FROM traces WHERE agent_id = ?1 ORDER BY started_at")?
                .query_map(params![agent_id.as_str()], row_to_trace)?
                .collect()
        })
        .await
    }

    pub async fn save_span(&self, span: InternalSpan) -> Result<(), StorageError> {
        let status = enum_to_text(&span.status)?;
        let attributes = serde_json::to_string(&span.attributes)?;
        let raw_attributes = serde_json::to_string(&span.raw_attributes)?;
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO spans
                    (id, trace_id, parent_id, operation, status, start_time, end_time, arrived_at, attributes, raw_attributes)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(id) DO UPDATE SET
                    status = excluded.status,
                    end_time = excluded.end_time,
                    attributes = excluded.attributes,
                    raw_attributes = excluded.raw_attributes",
                params![
                    span.id.as_str(),
                    span.trace_id.as_str(),
                    span.parent_id.as_deref(),
                    span.operation,
                    status,
                    span.start_time,
                    span.end_time,
                    span.arrived_at,
                    attributes,
                    raw_attributes,
                ],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn get_span(&self, id: &SpanId) -> Result<Option<InternalSpan>, StorageError> {
        let id = id.clone();
        self.with_conn(move |conn| {
            conn.query_row(
                "SELECT * FROM spans WHERE id = ?1",
                params![id.as_str()],
                row_to_span,
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                e => Err(e),
            })
        })
        .await
    }

    /// Saves every event in one transaction, since these always arrive as
    /// a batch tied to a single span.
    pub async fn save_span_events(&self, events: Vec<SpanEvent>) -> Result<(), StorageError> {
        self.with_conn(move |conn| {
            let tx = conn.unchecked_transaction()?;
            for event in &events {
                let event_type = enum_to_text(&event.event_type).map_err(rusqlite_serde_err)?;
                tx.execute(
                    "INSERT INTO span_events (id, span_id, event_type, occurred_at, content)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        event.id.as_str(),
                        event.span_id.as_str(),
                        event_type,
                        event.occurred_at,
                        event.content
                    ],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn get_span_events_for_span(
        &self,
        span_id: &SpanId,
    ) -> Result<Vec<SpanEvent>, StorageError> {
        let span_id = span_id.clone();
        self.with_conn(move |conn| {
            conn.prepare("SELECT * FROM span_events WHERE span_id = ?1 ORDER BY occurred_at")?
                .query_map(params![span_id.as_str()], row_to_span_event)?
                .collect()
        })
        .await
    }

    pub async fn save_evaluation_result(
        &self,
        result: EvaluationResult,
    ) -> Result<(), StorageError> {
        let target_type = enum_to_text(&result.target_type)?;
        let evaluator = enum_to_text(&result.evaluator)?;
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO evaluation_results
                    (id, target_id, target_type, metric, score, evaluator, judge_model_version, evaluated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    result.id.as_str(),
                    result.target_id,
                    target_type,
                    result.metric,
                    result.score,
                    evaluator,
                    result.judge_model_version,
                    result.evaluated_at,
                ],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn get_evaluation_result(
        &self,
        id: &EvalId,
    ) -> Result<Option<EvaluationResult>, StorageError> {
        let id = id.clone();
        self.with_conn(move |conn| {
            conn.query_row(
                "SELECT * FROM evaluation_results WHERE id = ?1",
                params![id.as_str()],
                row_to_evaluation_result,
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                e => Err(e),
            })
        })
        .await
    }

    pub async fn save_intervention_command(
        &self,
        command: InterventionCommand,
    ) -> Result<(), StorageError> {
        // CommandType carries data inline (Redirect/InjectContext), so it
        // gets stored as the full JSON shape, not quote-trimmed like the
        // unit-only enums.
        let command_type = serde_json::to_string(&command.command_type)?;
        let status = enum_to_text(&command.status)?;
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO intervention_commands
                    (id, trace_id, span_id, policy_id, command_type, status,
                     requires_confirmation, issued_by, valid_until_ms, issued_at, acknowledged_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(id) DO UPDATE SET
                    status = excluded.status,
                    acknowledged_at = excluded.acknowledged_at",
                params![
                    command.id.as_str(),
                    command.trace_id.as_str(),
                    command.span_id.as_deref(),
                    command.policy_id.as_deref(),
                    command_type,
                    status,
                    command.requires_confirmation,
                    command.issued_by,
                    command.valid_until_ms,
                    command.issued_at,
                    command.acknowledged_at,
                ],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn get_intervention_command(
        &self,
        id: &CommandId,
    ) -> Result<Option<InterventionCommand>, StorageError> {
        let id = id.clone();
        self.with_conn(move |conn| {
            conn.query_row(
                "SELECT * FROM intervention_commands WHERE id = ?1",
                params![id.as_str()],
                row_to_intervention_command,
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                e => Err(e),
            })
        })
        .await
    }

    pub async fn list_agents(&self) -> Result<Vec<Agent>, StorageError> {
        self.with_conn(|conn| {
            conn.prepare("SELECT * FROM agents ORDER BY last_seen_at DESC")?
                .query_map([], row_to_agent)?
                .collect()
        })
        .await
    }

    pub async fn list_spans_for_trace(
        &self,
        trace_id: &TraceId,
    ) -> Result<Vec<InternalSpan>, StorageError> {
        let trace_id = trace_id.clone();
        self.with_conn(move |conn| {
            conn.prepare("SELECT * FROM spans WHERE trace_id = ?1 ORDER BY start_time")?
                .query_map(params![trace_id.as_str()], row_to_span)?
                .collect()
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reeve_model::entity::{CommandType as CT, EvaluatorType as ET, TargetType as TT};
    use reeve_model::ids::{AgentId, CommandId, EvalId, SpanId, TraceId};
    use std::collections::HashMap as Map;

    /// `save_agent` is deliberately out of scope for this crate's first
    /// pass (see the issue), but `traces.agent_id` has a real foreign key
    /// constraint, so tests insert the minimum row directly rather than
    /// adding a public method that wasn't promised.
    async fn insert_test_agent(store: &WarmStore, id: &str) {
        let id = id.to_string();
        store
            .with_conn(move |conn| {
                conn.execute(
                    "INSERT INTO agents (id, name, framework, integration, status, first_seen_at, last_seen_at)
                     VALUES (?1, 'test-agent', 'custom', 'sdk', 'idle', 0, 0)",
                    params![id],
                )?;
                Ok(())
            })
            .await
            .unwrap();
    }

    fn trace(id: &str) -> Trace {
        Trace {
            id: id.into(),
            agent_id: "agent-1".into(),
            status: TraceStatus::Running,
            start_time: 0,
            end_time: None,
            root_span_id: None,
            final_health_score: None,
        }
    }

    fn span(id: &str, trace_id: &str) -> InternalSpan {
        InternalSpan {
            id: id.into(),
            trace_id: trace_id.into(),
            parent_id: None,
            operation: "test.op".to_string(),
            status: SpanStatus::Completed,
            start_time: 0,
            end_time: Some(5),
            arrived_at: 5,
            attributes: serde_json::json!({"k": "v"}),
            raw_attributes: Map::new(),
        }
    }

    #[tokio::test]
    async fn trace_round_trips() {
        let store = WarmStore::open_in_memory().unwrap();
        insert_test_agent(&store, "agent-1").await;
        store.save_trace(trace("t1")).await.unwrap();

        let loaded = store
            .get_trace(&TraceId::from("t1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.id.as_str(), "t1");
        assert_eq!(loaded.status, TraceStatus::Running);
    }

    #[tokio::test]
    async fn get_trace_missing_returns_none() {
        let store = WarmStore::open_in_memory().unwrap();
        let loaded = store.get_trace(&TraceId::from("missing")).await.unwrap();
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn span_round_trips_including_json_attributes() {
        let store = WarmStore::open_in_memory().unwrap();
        insert_test_agent(&store, "agent-1").await;
        store.save_trace(trace("t1")).await.unwrap();
        store.save_span(span("s1", "t1")).await.unwrap();

        let loaded = store.get_span(&SpanId::from("s1")).await.unwrap().unwrap();
        assert_eq!(loaded.status, SpanStatus::Completed);
        assert_eq!(loaded.attributes, serde_json::json!({"k": "v"}));
    }

    #[tokio::test]
    async fn span_events_round_trip_as_a_batch() {
        let store = WarmStore::open_in_memory().unwrap();
        insert_test_agent(&store, "agent-1").await;
        store.save_trace(trace("t1")).await.unwrap();
        store.save_span(span("s1", "t1")).await.unwrap();

        let events = vec![
            SpanEvent {
                id: "e1".into(),
                span_id: "s1".into(),
                event_type: EventType::UserMessage,
                occurred_at: 1,
                content: Some("hello".to_string()),
            },
            SpanEvent {
                id: "e2".into(),
                span_id: "s1".into(),
                event_type: EventType::AssistantMessage,
                occurred_at: 2,
                content: None,
            },
        ];
        store.save_span_events(events).await.unwrap();

        let loaded = store
            .get_span_events_for_span(&SpanId::from("s1"))
            .await
            .unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].content, Some("hello".to_string()));
        assert_eq!(loaded[1].content, None);
    }

    #[tokio::test]
    async fn evaluation_result_round_trips() {
        let store = WarmStore::open_in_memory().unwrap();
        insert_test_agent(&store, "agent-1").await;
        store.save_trace(trace("t1")).await.unwrap();

        let result = EvaluationResult {
            id: "ev1".into(),
            target_id: "t1".to_string(),
            target_type: TT::Trace,
            metric: "loop_detection".to_string(),
            score: 0.9,
            evaluator: ET::Heuristic,
            evaluated_at: 10,
            judge_model_version: None,
        };
        store.save_evaluation_result(result).await.unwrap();

        let loaded = store
            .get_evaluation_result(&EvalId::from("ev1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.metric, "loop_detection");
        assert_eq!(loaded.evaluator, ET::Heuristic);
        assert_eq!(loaded.score, 0.9);
    }

    #[tokio::test]
    async fn intervention_command_round_trips_inline_data() {
        let store = WarmStore::open_in_memory().unwrap();
        insert_test_agent(&store, "agent-1").await;
        store.save_trace(trace("t1")).await.unwrap();

        let command = InterventionCommand {
            id: "c1".into(),
            trace_id: "t1".into(),
            span_id: None,
            policy_id: None,
            command_type: CT::Redirect {
                instruction: "slow down".to_string(),
            },
            status: CommandStatus::PendingConfirmation,
            requires_confirmation: true,
            issued_at: 0,
            acknowledged_at: None,
            issued_by: "human".to_string(),
            valid_until_ms: 1000,
        };
        store.save_intervention_command(command).await.unwrap();

        let loaded = store
            .get_intervention_command(&CommandId::from("c1"))
            .await
            .unwrap()
            .unwrap();
        match loaded.command_type {
            CT::Redirect { instruction } => assert_eq!(instruction, "slow down"),
            other => panic!("expected Redirect, got {other:?}"),
        }
        assert_eq!(loaded.status, CommandStatus::PendingConfirmation);
    }

    #[tokio::test]
    async fn list_traces_for_agent_filters_correctly() {
        let store = WarmStore::open_in_memory().unwrap();
        insert_test_agent(&store, "agent-a").await;
        insert_test_agent(&store, "agent-b").await;
        let mut t1 = trace("t1");
        t1.agent_id = "agent-a".into();
        let mut t2 = trace("t2");
        t2.agent_id = "agent-a".into();
        let mut t3 = trace("t3");
        t3.agent_id = "agent-b".into();
        store.save_trace(t1).await.unwrap();
        store.save_trace(t2).await.unwrap();
        store.save_trace(t3).await.unwrap();

        let loaded = store
            .list_traces_for_agent(&AgentId::from("agent-a"))
            .await
            .unwrap();
        assert_eq!(loaded.len(), 2);
    }
}
