use crate::ids::{EvalId, Timestamp};
use crate::signal::EvaluationConfidence;
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
    /// Chain-of-thought breakdown for faithfulness and hallucination_detection.
    /// JSON blob with keys: claims, supported, unsupported.
    pub cot_json: Option<String>,
    /// What the judge's self-consistency check said about this result.
    /// `None` for tier 1 evaluators, which are deterministic. A `Low`
    /// result is saved but excluded from the health score, so without
    /// this the row does not say whether it counted.
    pub confidence: Option<EvaluationConfidence>,
}

/// What became of one metric that was dispatched to the judge.
///
/// `Scored` is the only outcome that also leaves a row in
/// `evaluation_results`. The rest are the ways a dispatched metric ends
/// without a number, and they exist as distinct values because an
/// absent result already meant five different things at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptOutcome {
    /// The metric produced a number and a result row.
    Scored,
    /// A call ended without a verdict: the timeout expired, the backend
    /// was unreachable, the retries ran out, or the response did not
    /// parse.
    NoVerdict,
    /// One phrasing came back and the other did not, so the side that
    /// completed was discarded with the side that failed.
    HalfPair,
    /// The response was the claim shape and its claim list was empty,
    /// so the score after it was not constrained by anything the model
    /// extracted.
    NoClaims,
}

/// One metric that was dispatched to the judge, recorded whether or not
/// it came back with a number.
///
/// `evaluation_results` holds a row only when a metric produced a
/// score, so a metric that burned its timeout and a metric that was
/// never offered to the judge are stored identically, which is as
/// nothing. Coverage read off that table is present against absent,
/// over a blank that carries at least five meanings. This records the
/// dispatch, so coverage becomes attempted against succeeded.
///
/// It covers only the causes that reach a dispatch. A metric that was
/// never sampled, or that had no input, or that was skipped because the
/// backend was off, has no row here either, and that is a known gap
/// rather than an oversight.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JudgeAttempt {
    pub id: EvalId,
    pub trace_id: String,
    pub metric: String,
    pub outcome: AttemptOutcome,
    /// Why it ended without a score, in the words of whatever gave up.
    /// `None` when the outcome is `Scored`.
    pub reason: Option<String>,
    pub attempted_at: Timestamp,
    pub judge_model_version: Option<String>,
}
