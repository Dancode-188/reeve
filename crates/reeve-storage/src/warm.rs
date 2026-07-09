use reeve_model::entity::policy::{PolicyRule, RuleScope};
use reeve_model::entity::{
    Agent, AgentStatus, CommandStatus, CommandType, EvaluationResult, EvaluatorType, EventType,
    IntegrationPath, InternalSpan, InterventionCommand, InterventionOutcome, SpanEvent, SpanNote,
    SpanStatus, TargetType, Trace, TraceStatus,
};
use reeve_model::ids::{AgentId, CommandId, EvalId, RuleId, SpanId, TraceId};
use rusqlite::{Connection, Row, params};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Spending analytics for the Cost view.
#[derive(Debug, Clone, Default)]
pub struct CostSummary {
    pub total: f64,
    pub trace_count: u32,
    /// (agent name, total cost), highest spend first.
    pub by_agent: Vec<(String, f64)>,
    /// (model name, total cost), highest spend first.
    pub by_model: Vec<(String, f64)>,
}

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
const MIGRATIONS: &[(i64, &str)] = &[
    (1, include_str!("../../../migrations/0001_initial.sql")),
    (2, include_str!("../../../migrations/0002_cot_json.sql")),
    (
        3,
        include_str!("../../../migrations/0003_policy_rules_description.sql"),
    ),
    (
        4,
        include_str!("../../../migrations/0004_policy_cooldowns.sql"),
    ),
    (
        5,
        include_str!("../../../migrations/0005_resumable_traces.sql"),
    ),
];

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
        cot_json: row.get("cot_json")?,
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

fn effectiveness_rows(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> rusqlite::Result<Vec<(String, f64)>> {
    conn.prepare(sql)?
        .query_map(params, |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
        })?
        .collect()
}

/// Groups (serialized command_type, delta) rows by command tag and returns
/// the tag with the best average delta among tags with enough samples.
fn best_by_tag(rows: &[(String, f64)], min_samples: u32) -> Option<(String, f64, u32)> {
    let mut by_tag: HashMap<&'static str, (f64, u32)> = HashMap::new();
    for (raw, delta) in rows {
        let Ok(command_type) = serde_json::from_str::<CommandType>(raw) else {
            continue;
        };
        let tag = match command_type {
            CommandType::Pause => "pause",
            CommandType::Resume => "resume",
            CommandType::Kill => "kill",
            CommandType::Redirect { .. } => "redirect",
            CommandType::InjectContext { .. } => "inject_context",
        };
        let entry = by_tag.entry(tag).or_insert((0.0, 0));
        entry.0 += delta;
        entry.1 += 1;
    }
    by_tag
        .into_iter()
        .filter(|(_, (_, n))| *n >= min_samples)
        .map(|(tag, (sum, n))| (tag.to_string(), sum / n as f64, n))
        .max_by(|a, b| a.1.total_cmp(&b.1))
}

fn row_to_intervention_outcome(row: &Row) -> rusqlite::Result<InterventionOutcome> {
    Ok(InterventionOutcome {
        id: row.get("id")?,
        command_id: row.get::<_, String>("command_id")?.into(),
        trace_id: row.get::<_, String>("trace_id")?.into(),
        pre_intervention_score: row.get("pre_intervention_score")?,
        post_intervention_score: row.get("post_intervention_score")?,
        delta: row.get("delta")?,
        spans_measured: row.get("spans_measured")?,
        measured_at: row.get("measured_at")?,
    })
}

fn row_to_policy_rule(row: &Row) -> rusqlite::Result<PolicyRule> {
    let command_type: String = row.get("command_type")?;
    let scope_raw: String = row.get("scope")?;
    let scope = match scope_raw.split_once(':') {
        Some(("agent", id)) => RuleScope::Agent(id.to_string()),
        Some(("framework", name)) => RuleScope::Framework(name.to_string()),
        _ => RuleScope::Global,
    };
    Ok(PolicyRule {
        id: row.get::<_, String>("id")?.into(),
        name: row.get("name")?,
        description: row.get("description")?,
        trigger_condition: row.get("trigger_condition")?,
        command_type: serde_json::from_str(&command_type).map_err(rusqlite_serde_err)?,
        requires_confirmation: row.get("requires_confirmation")?,
        cooldown_secs: row.get::<_, i64>("cooldown_secs")? as u64,
        scope,
        enabled: row.get("enabled")?,
        auto_confirm_after_secs: row
            .get::<_, Option<i64>>("auto_confirm_after_secs")?
            .map(|v| v as u64),
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

    pub async fn get_agent(&self, id: &AgentId) -> Result<Option<Agent>, StorageError> {
        let id = id.clone();
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare("SELECT * FROM agents WHERE id = ?1")?;
            let mut rows = stmt.query_map(params![id.as_str()], row_to_agent)?;
            rows.next().transpose()
        })
        .await
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

    /// Flags a stored trace as resumable: interrupted by connection loss
    /// rather than silence, eligible for the startup rescan.
    pub async fn mark_resumable(&self, trace_id: &TraceId) -> Result<(), StorageError> {
        let trace_id = trace_id.clone();
        self.with_conn(move |conn| {
            conn.execute(
                "UPDATE traces SET resumable = 1 WHERE id = ?1",
                params![trace_id.as_str()],
            )?;
            Ok(())
        })
        .await
    }

    /// Interrupted-and-resumable traces newer than the window, for the
    /// startup rescan. Claiming clears the flag so a second restart does
    /// not resume the same trace twice.
    pub async fn claim_resumable_traces(&self, within_ms: i64) -> Result<Vec<Trace>, StorageError> {
        self.with_conn(move |conn| {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock before unix epoch")
                .as_millis() as i64;
            let traces: Vec<Trace> = conn
                .prepare(
                    "SELECT * FROM traces
                     WHERE status = 'interrupted' AND resumable = 1
                       AND completed_at IS NOT NULL AND completed_at > ?1",
                )?
                .query_map(params![now - within_ms], row_to_trace)?
                .collect::<rusqlite::Result<_>>()?;
            conn.execute(
                "UPDATE traces SET resumable = 0
                 WHERE status = 'interrupted' AND resumable = 1",
                [],
            )?;
            Ok(traces)
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

    pub async fn update_trace_health_score(
        &self,
        trace_id: &TraceId,
        score: f64,
    ) -> Result<(), StorageError> {
        let trace_id = trace_id.clone();
        self.with_conn(move |conn| {
            conn.execute(
                "UPDATE traces SET final_health_score = ?1 WHERE id = ?2",
                params![score, trace_id.as_str()],
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

    /// Most recent traces first, for stepping backwards through an agent's
    /// history. `list_traces_for_agent` keeps its ascending order for
    /// callers that replay chronologically.
    pub async fn list_recent_traces_for_agent(
        &self,
        agent_id: &AgentId,
        limit: u32,
    ) -> Result<Vec<Trace>, StorageError> {
        let agent_id = agent_id.clone();
        self.with_conn(move |conn| {
            conn.prepare(
                "SELECT * FROM traces WHERE agent_id = ?1
                 ORDER BY started_at DESC LIMIT ?2",
            )?
            .query_map(params![agent_id.as_str(), limit], row_to_trace)?
            .collect()
        })
        .await
    }

    /// Completed traces newest-first with their total cost, for the History
    /// view. Cost lives span by span in the `gen_ai.usage.cost` attribute,
    /// so it is aggregated here with SQLite's JSON functions rather than
    /// denormalized onto the trace row. Scoped to one agent when given.
    pub async fn list_history(
        &self,
        agent_id: Option<&AgentId>,
        limit: u32,
    ) -> Result<Vec<(Trace, f64)>, StorageError> {
        let agent_id = agent_id.cloned();
        self.with_conn(move |conn| {
            let sql = format!(
                "SELECT t.*, COALESCE(SUM(json_extract(s.attributes,
                     '$.\"gen_ai.usage.cost\"')), 0.0) AS total_cost
                 FROM traces t
                 LEFT JOIN spans s ON s.trace_id = t.id
                 WHERE t.completed_at IS NOT NULL {}
                 GROUP BY t.id
                 ORDER BY t.started_at DESC
                 LIMIT ?1",
                if agent_id.is_some() {
                    "AND t.agent_id = ?2"
                } else {
                    ""
                }
            );
            let mut stmt = conn.prepare(&sql)?;
            let map = |row: &Row| {
                let trace = row_to_trace(row)?;
                let cost: f64 = row.get("total_cost")?;
                Ok((trace, cost))
            };
            let rows = match &agent_id {
                Some(a) => stmt.query_map(params![limit, a.as_str()], map)?,
                None => stmt.query_map(params![limit], map)?,
            };
            rows.collect()
        })
        .await
    }

    /// Removes a trace and its spans and span events from the warm tier, in
    /// one transaction. Evaluation results and intervention records survive:
    /// they are the audit trail of what Reeve observed and did, and deleting
    /// a trace's data should not also rewrite history about decisions.
    pub async fn delete_trace(&self, trace_id: &TraceId) -> Result<(), StorageError> {
        let trace_id = trace_id.clone();
        self.with_conn(move |conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "DELETE FROM span_events WHERE span_id IN
                     (SELECT id FROM spans WHERE trace_id = ?1)",
                params![trace_id.as_str()],
            )?;
            tx.execute(
                "DELETE FROM spans WHERE trace_id = ?1",
                params![trace_id.as_str()],
            )?;
            tx.execute(
                "DELETE FROM traces WHERE id = ?1",
                params![trace_id.as_str()],
            )?;
            tx.commit()?;
            Ok(())
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
                    (id, target_id, target_type, metric, score, evaluator, judge_model_version, evaluated_at, cot_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    result.id.as_str(),
                    result.target_id,
                    target_type,
                    result.metric,
                    result.score,
                    evaluator,
                    result.judge_model_version,
                    result.evaluated_at,
                    result.cot_json,
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

    /// A trace's evaluation results in the order they were produced, for
    /// replaying the quality timeline. Trace-targeted results only; span
    /// results share the timeline but target ids are span ids, and replay
    /// keys its quality animation off the trace-level rows.
    pub async fn list_evaluations_for_trace(
        &self,
        trace_id: &TraceId,
    ) -> Result<Vec<EvaluationResult>, StorageError> {
        let trace_id = trace_id.clone();
        self.with_conn(move |conn| {
            conn.prepare(
                "SELECT * FROM evaluation_results
                 WHERE target_id = ?1 AND target_type = 'trace'
                 ORDER BY evaluated_at",
            )?
            .query_map(params![trace_id.as_str()], row_to_evaluation_result)?
            .collect()
        })
        .await
    }

    /// Spending totals for the Cost view: overall total with trace count,
    /// cost grouped by agent, and cost grouped by model, in one pass over
    /// span attributes. Spans without a model attribute group under
    /// "other". Agents appear even at zero spend so the fleet is complete.
    pub async fn cost_summary(&self) -> Result<CostSummary, StorageError> {
        self.with_conn(|conn| {
            let total: f64 = conn.query_row(
                "SELECT COALESCE(SUM(json_extract(attributes,
                     '$.\"gen_ai.usage.cost\"')), 0.0) FROM spans",
                [],
                |row| row.get(0),
            )?;
            let trace_count: u32 = conn.query_row(
                "SELECT COUNT(*) FROM traces WHERE completed_at IS NOT NULL",
                [],
                |row| row.get(0),
            )?;
            let by_agent: Vec<(String, f64)> = conn
                .prepare(
                    "SELECT a.name, COALESCE(SUM(json_extract(s.attributes,
                         '$.\"gen_ai.usage.cost\"')), 0.0) AS cost
                     FROM agents a
                     LEFT JOIN traces t ON t.agent_id = a.id
                     LEFT JOIN spans s ON s.trace_id = t.id
                     GROUP BY a.id ORDER BY cost DESC",
                )?
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let by_model: Vec<(String, f64)> = conn
                .prepare(
                    "SELECT COALESCE(json_extract(attributes,
                         '$.\"gen_ai.request.model\"'), 'other') AS model,
                         SUM(json_extract(attributes,
                         '$.\"gen_ai.usage.cost\"')) AS cost
                     FROM spans
                     WHERE json_extract(attributes,
                         '$.\"gen_ai.usage.cost\"') IS NOT NULL
                     GROUP BY model ORDER BY cost DESC",
                )?
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            Ok(CostSummary {
                total,
                trace_count,
                by_agent,
                by_model,
            })
        })
        .await
    }

    /// Saves a developer annotation. One note per span: annotating again
    /// replaces the note, keeping the interaction one keystroke instead of
    /// a note-management UI.
    pub async fn save_span_note(&self, note: SpanNote) -> Result<(), StorageError> {
        self.with_conn(move |conn| {
            conn.execute(
                "DELETE FROM span_notes WHERE span_id = ?1",
                params![note.span_id.as_str()],
            )?;
            conn.execute(
                "INSERT INTO span_notes (id, span_id, content, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    note.id,
                    note.span_id.as_str(),
                    note.content,
                    note.created_at
                ],
            )?;
            Ok(())
        })
        .await
    }

    /// A trace's annotations keyed by span, loaded with the trace so notes
    /// survive into history and replay views.
    pub async fn span_notes_for_trace(
        &self,
        trace_id: &TraceId,
    ) -> Result<HashMap<String, SpanNote>, StorageError> {
        let trace_id = trace_id.clone();
        self.with_conn(move |conn| {
            conn.prepare(
                "SELECT n.id, n.span_id, n.content, n.created_at
                 FROM span_notes n
                 JOIN spans s ON n.span_id = s.id
                 WHERE s.trace_id = ?1",
            )?
            .query_map(params![trace_id.as_str()], |row| {
                let span_id: String = row.get(1)?;
                Ok((
                    span_id.clone(),
                    SpanNote {
                        id: row.get(0)?,
                        span_id: span_id.into(),
                        content: row.get(2)?,
                        created_at: row.get(3)?,
                    },
                ))
            })?
            .collect()
        })
        .await
    }

    /// Content of a trace's span events keyed by span, for replay's
    /// streaming box. Only events that carried content (privacy tier 2
    /// recordings) come back; a tier 1 trace returns an empty map.
    pub async fn span_content_for_trace(
        &self,
        trace_id: &TraceId,
    ) -> Result<HashMap<String, String>, StorageError> {
        let trace_id = trace_id.clone();
        self.with_conn(move |conn| {
            conn.prepare(
                "SELECT se.span_id, se.content FROM span_events se
                 JOIN spans s ON se.span_id = s.id
                 WHERE s.trace_id = ?1 AND se.content IS NOT NULL
                 ORDER BY se.occurred_at",
            )?
            .query_map(params![trace_id.as_str()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect()
        })
        .await
    }

    /// A trace's intervention commands in the order they were issued, for
    /// replay's timeline markers.
    pub async fn list_commands_for_trace(
        &self,
        trace_id: &TraceId,
    ) -> Result<Vec<InterventionCommand>, StorageError> {
        let trace_id = trace_id.clone();
        self.with_conn(move |conn| {
            conn.prepare(
                "SELECT * FROM intervention_commands
                 WHERE trace_id = ?1 ORDER BY issued_at",
            )?
            .query_map(params![trace_id.as_str()], row_to_intervention_command)?
            .collect()
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

    /// Persist an agent record. Callers from the control channel use this to
    /// ensure agent identities survive server restarts independent of whether
    /// the agent also sends OTel spans. Delegates to `upsert_agent`.
    pub async fn save_agent(&self, agent: Agent) -> Result<(), StorageError> {
        self.upsert_agent(agent).await
    }

    pub async fn save_intervention_outcome(
        &self,
        outcome: InterventionOutcome,
    ) -> Result<(), StorageError> {
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO intervention_outcomes
                    (id, command_id, trace_id, pre_intervention_score,
                     post_intervention_score, delta, spans_measured, measured_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(id) DO UPDATE SET
                    post_intervention_score = excluded.post_intervention_score,
                    delta                   = excluded.delta,
                    spans_measured          = excluded.spans_measured,
                    measured_at             = excluded.measured_at",
                params![
                    outcome.id,
                    outcome.command_id.as_str(),
                    outcome.trace_id.as_str(),
                    outcome.pre_intervention_score,
                    outcome.post_intervention_score,
                    outcome.delta,
                    outcome.spans_measured,
                    outcome.measured_at,
                ],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn get_intervention_outcome(
        &self,
        command_id: &CommandId,
    ) -> Result<Option<InterventionOutcome>, StorageError> {
        let command_id = command_id.clone();
        self.with_conn(move |conn| {
            conn.query_row(
                "SELECT * FROM intervention_outcomes WHERE command_id = ?1",
                params![command_id.as_str()],
                row_to_intervention_outcome,
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                e => Err(e),
            })
        })
        .await
    }

    pub async fn get_intervention_outcomes_for_trace(
        &self,
        trace_id: &TraceId,
    ) -> Result<Vec<InterventionOutcome>, StorageError> {
        let trace_id = trace_id.clone();
        self.with_conn(move |conn| {
            let mut stmt =
                conn.prepare("SELECT * FROM intervention_outcomes WHERE trace_id = ?1")?;
            let rows = stmt.query_map(params![trace_id.as_str()], row_to_intervention_outcome)?;
            rows.collect()
        })
        .await
    }

    /// The best-performing intervention for a rule, from measured outcomes.
    ///
    /// Aggregates deltas of outcomes whose command was issued by `rule_id`,
    /// grouped by command type, requiring at least `min_samples` before an
    /// answer is offered. Scoped to `agent_id` first; when that agent has
    /// too few samples, falls back to all agents sharing its framework, so
    /// a fresh agent benefits from its siblings' history. Returns the
    /// command tag, average delta, and sample count of the best performer.
    ///
    /// The command_type column stores the serialized CommandType, which
    /// carries redirect instructions inline, so grouping happens in Rust on
    /// the extracted tag rather than in SQL on the raw column.
    pub async fn best_intervention_for_rule(
        &self,
        rule_id: &RuleId,
        agent_id: &AgentId,
        min_samples: u32,
    ) -> Result<Option<(String, f64, u32)>, StorageError> {
        let rule_id = rule_id.clone();
        let agent_id = agent_id.clone();
        self.with_conn(move |conn| {
            let agent_scoped = effectiveness_rows(
                conn,
                "SELECT ic.command_type, io.delta
                 FROM intervention_outcomes io
                 JOIN intervention_commands ic ON io.command_id = ic.id
                 JOIN traces t ON ic.trace_id = t.id
                 WHERE ic.policy_id = ?1 AND t.agent_id = ?2
                   AND io.delta IS NOT NULL",
                params![rule_id.as_str(), agent_id.as_str()],
            )?;
            if let Some(best) = best_by_tag(&agent_scoped, min_samples) {
                return Ok(Some(best));
            }
            let framework_scoped = effectiveness_rows(
                conn,
                "SELECT ic.command_type, io.delta
                 FROM intervention_outcomes io
                 JOIN intervention_commands ic ON io.command_id = ic.id
                 JOIN traces t ON ic.trace_id = t.id
                 JOIN agents a ON t.agent_id = a.id
                 WHERE ic.policy_id = ?1 AND io.delta IS NOT NULL
                   AND a.framework =
                       (SELECT framework FROM agents WHERE id = ?2)",
                params![rule_id.as_str(), agent_id.as_str()],
            )?;
            Ok(best_by_tag(&framework_scoped, min_samples))
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

    pub async fn load_policy_rules(&self) -> Result<Vec<PolicyRule>, StorageError> {
        self.with_conn(|conn| {
            conn.prepare("SELECT * FROM policy_rules WHERE enabled = 1")?
                .query_map([], row_to_policy_rule)?
                .collect()
        })
        .await
    }

    pub async fn save_policy_rule(&self, rule: PolicyRule) -> Result<(), StorageError> {
        let scope_text = match &rule.scope {
            RuleScope::Global => "global".to_string(),
            RuleScope::Agent(id) => format!("agent:{id}"),
            RuleScope::Framework(name) => format!("framework:{name}"),
        };
        let command_type = serde_json::to_string(&rule.command_type)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_millis() as i64;
        let id = rule.id.as_str().to_string();
        let cooldown = rule.cooldown_secs as i64;
        let auto_confirm = rule.auto_confirm_after_secs.map(|v| v as i64);
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO policy_rules \
                 (id, name, description, scope, trigger_condition, command_type, \
                  requires_confirmation, cooldown_secs, auto_confirm_after_secs, \
                  enabled, created_at) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                params![
                    id,
                    rule.name,
                    rule.description,
                    scope_text,
                    rule.trigger_condition,
                    command_type,
                    rule.requires_confirmation,
                    cooldown,
                    auto_confirm,
                    rule.enabled,
                    now,
                ],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn save_policy_cooldown(
        &self,
        agent_id: &AgentId,
        rule_id: &RuleId,
        last_fired_ms: i64,
        cooldown_secs: u64,
    ) -> Result<(), StorageError> {
        let expires_at = last_fired_ms + (cooldown_secs as i64) * 1000;
        let agent_id = agent_id.as_str().to_string();
        let rule_id = rule_id.as_str().to_string();
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO policy_cooldowns \
                 (agent_id, rule_id, last_fired_at, expires_at) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![agent_id, rule_id, last_fired_ms, expires_at],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn load_active_policy_cooldowns(
        &self,
        now_ms: i64,
    ) -> Result<Vec<(AgentId, RuleId, i64)>, StorageError> {
        self.with_conn(move |conn| {
            conn.prepare(
                "SELECT agent_id, rule_id, last_fired_at FROM policy_cooldowns \
                 WHERE expires_at > ?1",
            )?
            .query_map(params![now_ms], |row| {
                let agent_id: String = row.get(0)?;
                let rule_id: String = row.get(1)?;
                let last_fired_at: i64 = row.get(2)?;
                Ok((
                    AgentId::from(agent_id.as_str()),
                    RuleId::from(rule_id.as_str()),
                    last_fired_at,
                ))
            })?
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

    async fn insert_test_agent(store: &WarmStore, id: &str) {
        store
            .save_agent(Agent {
                id: id.into(),
                name: "test-agent".to_string(),
                framework: "custom".to_string(),
                integration: IntegrationPath::Sdk,
                status: AgentStatus::Idle,
                first_seen_at: 0,
                last_seen_at: 0,
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

    fn make_command(id: &str, trace_id: &str) -> InterventionCommand {
        InterventionCommand {
            id: id.into(),
            trace_id: trace_id.into(),
            span_id: None,
            policy_id: None,
            command_type: CT::Pause,
            status: CommandStatus::Pending,
            requires_confirmation: false,
            issued_at: 0,
            acknowledged_at: None,
            issued_by: "human".to_string(),
            valid_until_ms: i64::MAX,
        }
    }

    /// Seeds one measured outcome: an agent-owned trace, a command issued by
    /// `rule` (None = human), and an outcome with the given delta.
    async fn seed_outcome(
        store: &WarmStore,
        agent: &str,
        n: u32,
        rule: Option<&str>,
        command_type: CT,
        delta: f64,
    ) {
        let trace_id = format!("t-{agent}-{n}");
        let cmd_id = format!("c-{agent}-{n}");
        store
            .save_trace(Trace {
                id: trace_id.as_str().into(),
                agent_id: agent.into(),
                status: TraceStatus::Completed,
                start_time: 0,
                end_time: Some(1),
                root_span_id: None,
                final_health_score: None,
            })
            .await
            .unwrap();
        let mut cmd = make_command(&cmd_id, &trace_id);
        cmd.policy_id = rule.map(Into::into);
        cmd.command_type = command_type;
        store.save_intervention_command(cmd).await.unwrap();
        store
            .save_intervention_outcome(InterventionOutcome {
                id: format!("o-{agent}-{n}"),
                command_id: cmd_id.as_str().into(),
                trace_id: trace_id.as_str().into(),
                pre_intervention_score: Some(40.0),
                post_intervention_score: Some(40.0 + delta),
                delta: Some(delta),
                spans_measured: Some(3),
                measured_at: 0,
            })
            .await
            .unwrap();
    }

    fn redirect() -> CT {
        CT::Redirect {
            instruction: "steer".to_string(),
        }
    }

    #[tokio::test]
    async fn resumable_claim_returns_recent_and_clears_the_flag() {
        let store = WarmStore::open_in_memory().unwrap();
        insert_test_agent(&store, "agent-1").await;
        let mut t = trace("t1");
        t.status = TraceStatus::Interrupted;
        t.end_time = Some(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as i64,
        );
        store.save_trace(t).await.unwrap();
        store.mark_resumable(&TraceId::from("t1")).await.unwrap();

        let claimed = store.claim_resumable_traces(5 * 60 * 1000).await.unwrap();
        assert_eq!(claimed.len(), 1, "recent resumable trace is claimed");

        let again = store.claim_resumable_traces(5 * 60 * 1000).await.unwrap();
        assert!(
            again.is_empty(),
            "claiming clears the flag so a second restart does not double-resume"
        );
    }

    #[tokio::test]
    async fn span_note_saves_and_replaces() {
        let store = WarmStore::open_in_memory().unwrap();
        insert_test_agent(&store, "agent-1").await;
        store.save_trace(trace("t1")).await.unwrap();
        store.save_span(span("s1", "t1")).await.unwrap();

        store
            .save_span_note(SpanNote {
                id: "n1".to_string(),
                span_id: "s1".into(),
                content: "first thought".to_string(),
                created_at: 1,
            })
            .await
            .unwrap();
        store
            .save_span_note(SpanNote {
                id: "n2".to_string(),
                span_id: "s1".into(),
                content: "better thought".to_string(),
                created_at: 2,
            })
            .await
            .unwrap();

        let notes = store
            .span_notes_for_trace(&TraceId::from("t1"))
            .await
            .unwrap();
        assert_eq!(notes.len(), 1, "one note per span; the second replaces");
        assert_eq!(notes.get("s1").unwrap().content, "better thought");
    }

    #[tokio::test]
    async fn cost_summary_groups_by_agent_and_model() {
        let store = WarmStore::open_in_memory().unwrap();
        insert_test_agent(&store, "agent-1").await;
        store.save_trace(trace("t1")).await.unwrap();
        let mut s1 = span("s1", "t1");
        s1.attributes = serde_json::json!({
            "gen_ai.usage.cost": 0.20, "gen_ai.request.model": "phi4-mini"
        });
        store.save_span(s1).await.unwrap();
        let mut s2 = span("s2", "t1");
        s2.attributes = serde_json::json!({"gen_ai.usage.cost": 0.05});
        store.save_span(s2).await.unwrap();

        let summary = store.cost_summary().await.unwrap();
        assert!((summary.total - 0.25).abs() < 1e-9);
        assert_eq!(summary.by_agent[0].0, "test-agent");
        assert!((summary.by_agent[0].1 - 0.25).abs() < 1e-9);
        assert_eq!(summary.by_model[0], ("phi4-mini".to_string(), 0.20));
        assert_eq!(
            summary.by_model[1].0, "other",
            "spans without a model attribute group under other"
        );
    }

    #[tokio::test]
    async fn list_history_aggregates_cost_and_orders_newest_first() {
        let store = WarmStore::open_in_memory().unwrap();
        insert_test_agent(&store, "agent-1").await;
        for (tid, started, completed) in [("t1", 100, 150), ("t2", 200, 260)] {
            store
                .save_trace(Trace {
                    id: tid.into(),
                    agent_id: "agent-1".into(),
                    status: TraceStatus::Completed,
                    start_time: started,
                    end_time: Some(completed),
                    root_span_id: None,
                    final_health_score: Some(90.0),
                })
                .await
                .unwrap();
        }
        let mut costly = span("s1", "t2");
        costly.attributes = serde_json::json!({"gen_ai.usage.cost": 0.25});
        store.save_span(costly).await.unwrap();
        let mut costly2 = span("s2", "t2");
        costly2.attributes = serde_json::json!({"gen_ai.usage.cost": 0.05});
        store.save_span(costly2).await.unwrap();

        let history = store.list_history(None, 10).await.unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].0.id.as_str(), "t2", "newest first");
        assert!((history[0].1 - 0.30).abs() < 1e-9, "span costs summed");
        assert!((history[1].1 - 0.0).abs() < 1e-9, "no spans, zero cost");
    }

    #[tokio::test]
    async fn delete_trace_removes_spans_and_events_atomically() {
        let store = WarmStore::open_in_memory().unwrap();
        insert_test_agent(&store, "agent-1").await;
        store.save_trace(trace("t1")).await.unwrap();
        store.save_span(span("s1", "t1")).await.unwrap();
        store
            .save_span_events(vec![SpanEvent {
                id: "ev1".into(),
                span_id: "s1".into(),
                event_type: EventType::AssistantMessage,
                occurred_at: 0,
                content: None,
            }])
            .await
            .unwrap();

        store.delete_trace(&TraceId::from("t1")).await.unwrap();

        assert!(
            store
                .get_trace(&TraceId::from("t1"))
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .list_spans_for_trace(&TraceId::from("t1"))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn best_intervention_prefers_highest_average_delta() {
        let store = WarmStore::open_in_memory().unwrap();
        insert_test_agent(&store, "agent-1").await;
        for (n, delta) in [(1, 0.3), (2, 0.4), (3, 0.5)] {
            seed_outcome(&store, "agent-1", n, Some("rule-a"), redirect(), delta).await;
        }
        for (n, delta) in [(4, 0.1), (5, 0.1), (6, 0.1)] {
            seed_outcome(&store, "agent-1", n, Some("rule-a"), CT::Pause, delta).await;
        }

        let best = store
            .best_intervention_for_rule(&RuleId::from("rule-a"), &AgentId::from("agent-1"), 3)
            .await
            .unwrap()
            .expect("enough samples for an answer");
        assert_eq!(best.0, "redirect");
        assert!((best.1 - 0.4).abs() < 1e-9, "mean of 0.3/0.4/0.5");
        assert_eq!(best.2, 3);
    }

    #[tokio::test]
    async fn best_intervention_falls_back_to_framework() {
        let store = WarmStore::open_in_memory().unwrap();
        insert_test_agent(&store, "agent-1").await;
        insert_test_agent(&store, "agent-2").await;
        // agent-1 has one sample, below the minimum; agent-2 shares the
        // framework and has three.
        seed_outcome(&store, "agent-1", 1, Some("rule-a"), redirect(), 0.9).await;
        for n in 2..5 {
            seed_outcome(&store, "agent-2", n, Some("rule-a"), redirect(), 0.2).await;
        }

        let best = store
            .best_intervention_for_rule(&RuleId::from("rule-a"), &AgentId::from("agent-1"), 3)
            .await
            .unwrap()
            .expect("framework siblings supply the samples");
        assert_eq!(best.0, "redirect");
        assert_eq!(best.2, 4, "fallback aggregates the whole framework");
    }

    #[tokio::test]
    async fn best_intervention_ignores_other_rules_and_human_commands() {
        let store = WarmStore::open_in_memory().unwrap();
        insert_test_agent(&store, "agent-1").await;
        for n in 1..4 {
            seed_outcome(&store, "agent-1", n, Some("rule-b"), redirect(), 0.9).await;
        }
        for n in 4..7 {
            seed_outcome(&store, "agent-1", n, None, redirect(), 0.9).await;
        }

        let best = store
            .best_intervention_for_rule(&RuleId::from("rule-a"), &AgentId::from("agent-1"), 3)
            .await
            .unwrap();
        assert!(
            best.is_none(),
            "other rules' and human-issued outcomes must not answer for rule-a"
        );
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

        let cot = r#"{"claims":["sky is blue"],"supported":["sky is blue"],"unsupported":[]}"#;
        let result = EvaluationResult {
            id: "ev1".into(),
            target_id: "t1".to_string(),
            target_type: TT::Trace,
            metric: "faithfulness".to_string(),
            score: 0.9,
            evaluator: ET::LlmJudge,
            evaluated_at: 10,
            judge_model_version: Some("phi4-mini".to_string()),
            cot_json: Some(cot.to_string()),
        };
        store.save_evaluation_result(result).await.unwrap();

        let loaded = store
            .get_evaluation_result(&EvalId::from("ev1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.metric, "faithfulness");
        assert_eq!(loaded.evaluator, ET::LlmJudge);
        assert_eq!(loaded.score, 0.9);
        assert_eq!(loaded.cot_json.as_deref(), Some(cot));
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

    #[tokio::test]
    async fn save_agent_round_trips() {
        let store = WarmStore::open_in_memory().unwrap();
        insert_test_agent(&store, "agent-rt").await;
        let agents = store.list_agents().await.unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].id.as_str(), "agent-rt");
    }

    #[tokio::test]
    async fn save_agent_upserts_on_conflict() {
        let store = WarmStore::open_in_memory().unwrap();
        insert_test_agent(&store, "agent-u").await;
        store
            .save_agent(Agent {
                id: "agent-u".into(),
                name: "updated".to_string(),
                framework: "custom".to_string(),
                integration: IntegrationPath::Sdk,
                status: AgentStatus::Running,
                first_seen_at: 0,
                last_seen_at: 100,
            })
            .await
            .unwrap();
        let agents = store.list_agents().await.unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].status, AgentStatus::Running);
        assert_eq!(agents[0].last_seen_at, 100);
    }

    #[tokio::test]
    async fn intervention_outcome_round_trips() {
        let store = WarmStore::open_in_memory().unwrap();
        insert_test_agent(&store, "agent-1").await;
        store.save_trace(trace("t1")).await.unwrap();
        store
            .save_intervention_command(make_command("cmd-1", "t1"))
            .await
            .unwrap();

        let outcome = InterventionOutcome {
            id: "out-1".to_string(),
            command_id: "cmd-1".into(),
            trace_id: "t1".into(),
            pre_intervention_score: Some(40.0),
            post_intervention_score: Some(75.0),
            delta: Some(35.0),
            spans_measured: Some(4),
            measured_at: 999,
        };
        store
            .save_intervention_outcome(outcome.clone())
            .await
            .unwrap();

        let loaded = store
            .get_intervention_outcome(&CommandId::from("cmd-1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.id, "out-1");
        assert_eq!(loaded.delta, Some(35.0));
        assert_eq!(loaded.spans_measured, Some(4));
    }

    #[tokio::test]
    async fn get_intervention_outcomes_for_trace_filters_by_trace() {
        let store = WarmStore::open_in_memory().unwrap();
        insert_test_agent(&store, "agent-1").await;
        store.save_trace(trace("t1")).await.unwrap();
        store.save_trace(trace("t2")).await.unwrap();
        store
            .save_intervention_command(make_command("cmd-1", "t1"))
            .await
            .unwrap();
        store
            .save_intervention_command(make_command("cmd-2", "t2"))
            .await
            .unwrap();

        let outcome_t1 = InterventionOutcome {
            id: "out-1".to_string(),
            command_id: "cmd-1".into(),
            trace_id: "t1".into(),
            pre_intervention_score: Some(40.0),
            post_intervention_score: Some(75.0),
            delta: Some(35.0),
            spans_measured: Some(4),
            measured_at: 1,
        };
        let outcome_t2 = InterventionOutcome {
            id: "out-2".to_string(),
            command_id: "cmd-2".into(),
            trace_id: "t2".into(),
            pre_intervention_score: Some(50.0),
            post_intervention_score: Some(60.0),
            delta: Some(10.0),
            spans_measured: Some(2),
            measured_at: 2,
        };
        store.save_intervention_outcome(outcome_t1).await.unwrap();
        store.save_intervention_outcome(outcome_t2).await.unwrap();

        let results = store
            .get_intervention_outcomes_for_trace(&TraceId::from("t1"))
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "out-1");
        assert_eq!(results[0].trace_id, TraceId::from("t1"));
    }

    #[tokio::test]
    async fn get_intervention_outcome_missing_returns_none() {
        let store = WarmStore::open_in_memory().unwrap();
        let result = store
            .get_intervention_outcome(&CommandId::from("no-such"))
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn save_and_load_active_policy_cooldown() {
        let store = WarmStore::open_in_memory().unwrap();
        let agent_id = AgentId::from("agent-1");
        let rule_id = RuleId::from("builtin_low_health");
        let now_ms: i64 = 1_000_000;
        store
            .save_policy_cooldown(&agent_id, &rule_id, now_ms, 300)
            .await
            .unwrap();
        let rows = store
            .load_active_policy_cooldowns(now_ms + 1)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, agent_id);
        assert_eq!(rows[0].1, rule_id);
        assert_eq!(rows[0].2, now_ms);
    }

    #[tokio::test]
    async fn load_active_policy_cooldowns_excludes_expired() {
        let store = WarmStore::open_in_memory().unwrap();
        let agent_id = AgentId::from("agent-1");
        let rule_id = RuleId::from("builtin_low_health");
        let now_ms: i64 = 1_000_000;
        store
            .save_policy_cooldown(&agent_id, &rule_id, now_ms, 300)
            .await
            .unwrap();
        // Query with a time well past the cooldown window.
        let rows = store
            .load_active_policy_cooldowns(now_ms + 400_000)
            .await
            .unwrap();
        assert!(rows.is_empty());
    }
}
