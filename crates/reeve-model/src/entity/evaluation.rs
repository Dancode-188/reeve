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

/// Why a dispatched metric ended the way it did, as a value rather than
/// a sentence.
///
/// `AttemptOutcome` says what was lost and this says what took it. The
/// two are orthogonal rather than nested: the same wait bound ends one
/// metric outright and costs another a completed first phrasing, which
/// is one cause under two outcomes. `reason` has carried this in prose
/// from the start, but three of those sentences interpolate a timeout,
/// a character count or a dump of the keys that came back, so grouping
/// by it splits a cause across as many values as there were failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptCause {
    /// An unweighted metric released the one dispatch slot to a metric
    /// that carries weight instead of competing for it.
    StoodAside,
    /// The dispatch bound expired with the slot still held, and the
    /// call was dropped rather than queued past it.
    WaitBound,
    /// Enough calls were already waiting that this one could not have
    /// been served inside the bound it would have been held to, so it
    /// was refused on arrival instead of at the end of that wait.
    QueueFull,
    /// The dispatch slot was closed, which happens on shutdown.
    SlotClosed,
    /// The backend took the call and did not answer inside the
    /// evaluation timeout.
    BackendTimeout,
    /// The backend refused the call, reset it, or could not be reached.
    BackendUnreachable,
    /// An answer came back in the claim shape naming no claim, so
    /// nothing the model extracted constrained the score after it.
    NoClaims,
    /// An answer came back and no score could be read out of it.
    Unparseable,
}

/// Which of a metric's two phrasings decided its row.
///
/// A row here covers a metric rather than a call, because the pair is
/// collapsed before it is recorded. This names the side that ended it,
/// and is `None` when both sides answered and the pair was averaged.
/// Without it the two failures that read alike in every other column,
/// a first phrasing that never dispatched and a second that threw away
/// a served first, are separable only by rereading the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phrasing {
    A,
    B,
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
    /// The same ending as `reason`, as a value that groups and that a
    /// change to a timeout cannot reword. `None` when the outcome is
    /// `Scored`, and on every row written before it existed.
    pub cause: Option<AttemptCause>,
    /// Which phrasing ended the metric. `None` when both answered.
    pub phrasing: Option<Phrasing>,
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
