use crate::ids::{EvalId, Timestamp};
use serde::{Deserialize, Serialize};

/// The category that produced a score, not the specific check. The
/// specific check name (e.g. "loop_detection", "faithfulness") lives in
/// `EvaluationResult::metric`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluatorType {
    Heuristic,
    LlmJudge,
    Statistical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetType {
    Span,
    Trace,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvaluationResult {
    pub id: EvalId,
    /// Polymorphic: a span ID or a trace ID, disambiguated by `target_type`.
    pub target_id: String,
    pub target_type: TargetType,
    pub metric: String,
    pub score: f64,
    pub evaluator: EvaluatorType,
    pub evaluated_at: Timestamp,
    /// Stored for historical comparison integrity even after the judge
    /// model changes.
    pub judge_model_version: Option<String>,
}
