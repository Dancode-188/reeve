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
    /// was unreachable, the retries ran out, the response did not
    /// parse, or the one dispatch slot never came free and the call was
    /// dropped rather than sent into a queue it could not survive.
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
/// It covers the causes that reach a dispatch, and one that stops just
/// short of it: a metric turned away by a full dispatch slot is
/// recorded, because that is a decision this crate made about a metric
/// it meant to send, which is exactly the blank this table exists to
/// remove. A metric that was never sampled, or that had no input, or
/// that was skipped because the backend was off, has no row here, and
/// that is a known gap rather than an oversight.
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
    /// How much of the turn this dispatch was shown. `None` off the
    /// capture path, where the reply rides on the span and there are no
    /// rounds to choose between.
    pub reply: Option<ReplyProvenance>,
}

/// How much of a turn the judge read before it answered.
///
/// A turn that called tools produces a reply per round, and which of
/// them gets graded is a rule rather than a given. Without these the
/// rule is invisible after the fact: a metric that refused to find a
/// claim in four words of acknowledgement and a metric that refused to
/// find one in a turn full of assertions write the same row.
///
/// Recorded on the dispatch rather than the result because the outcomes
/// worth explaining are the ones that never produce a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplyProvenance {
    /// Characters of reply text handed to the judge, after the budget.
    pub chars_shown: i64,
    /// Characters of reply text the turn held, before the budget. The
    /// denominator, and the only field that says what was left out.
    pub chars_available: i64,
    /// Which reply carrying round the context and instruction were read
    /// from, counting from zero in trace order.
    pub anchor_index: i64,
    /// How many rounds in the turn carried a reply at all.
    pub replies_available: i64,
}
