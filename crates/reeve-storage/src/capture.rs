//! Reading the proxy capture store.
//!
//! ADR-0046 put round trips on disk beside the database and said nothing
//! in the binary reads them back. ADR-0048 overturned that one clause:
//! the judge reads content from here, because the span attributes it
//! looks for are written by nothing in the workspace.
//!
//! The layout is stated here rather than beside the writer because both
//! sides need it and `reeve-engine` and `reeve-ingestion` are siblings.
//! Rounds are addressed by the span that produced them, so a reader
//! needs no index and no directory scan: on the proxy path a round's
//! `started_at_ms` and its span's `start_time` are the same instant
//! taken twice, and the proxy sets no clock offset to move them apart.
//! Off that path the file is simply absent and every call here returns
//! `None`, which is the behaviour of the judge before it had a reader.

use std::path::{Path, PathBuf};

/// Where a round trip is stored. Millis first so the directory sorts
/// chronologically; the span id keeps concurrent rounds from colliding.
pub fn round_path(root: &Path, started_at_ms: i64, span_id: &str) -> PathBuf {
    root.join("rounds")
        .join(format!("{started_at_ms}-{span_id}.json"))
}

/// The name a message is referenced by from inside a stored round.
pub fn message_name(hash: u64) -> String {
    format!("{hash:016x}")
}

/// Where a message is stored. Sharded by the first byte of its name: a
/// long-running corpus accumulates tens of thousands of these, and one
/// flat directory of them is slow to list and unpleasant to browse by
/// hand. Names shorter than a shard are rejected rather than clamped,
/// so a malformed reference misses instead of colliding with a real
/// file.
pub fn message_path(root: &Path, name: &str) -> Option<PathBuf> {
    if name.len() < 2 || !name.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(
        root.join("messages")
            .join(&name[..2])
            .join(format!("{name}.json")),
    )
}

/// A capture directory, read-only.
pub struct CaptureReader {
    root: PathBuf,
}

/// One stored round trip.
#[derive(Clone, Debug)]
pub struct CapturedRound {
    value: serde_json::Value,
}

/// How tool traffic is rendered into a judge context.
///
/// `WithTools` is what ships. `TextOnly` is the rule that used to,
/// kept because a replay needs to render the old context to have
/// anything to compare the new one against, and `CappedTools` is a
/// candidate that measurement has not justified: over the stored
/// corpus it moved the budget share by under a point at the median and
/// at both the tenth and twenty fifth percentile, so it buys nothing
/// while the budget is the binding constraint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextMode {
    /// Only `text` blocks. Superseded, and kept as a replay baseline.
    TextOnly,
    /// Tool calls with their arguments, and tool results whole.
    WithTools,
    /// Tool calls with their arguments, and each tool result cut to a
    /// per block cap so one large result cannot spend the whole budget.
    CappedTools(usize),
}

/// Which reply in a trace the judge is asked to grade.
///
/// `AllJoined` is what ships. A turn that called tools produces a
/// reply per round, most of them empty, and the earliest non empty one
/// is usually the sentence written before the work started, so under
/// `First` the conclusions a grounding check exists to test were never
/// read. The other three are kept because the choice was made by
/// measuring them against each other on the stored corpus, and a
/// number that cannot be recomputed is not evidence for long.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplyMode {
    /// The earliest non empty reply. The rule that used to ship, kept
    /// as the replay baseline.
    First,
    /// The last non empty reply, which is where a turn's conclusions
    /// land unless it ended on an acknowledgement.
    Last,
    /// The longest non empty reply. A tie keeps the earliest.
    Largest,
    /// Every non empty reply in trace order, whole replies kept from
    /// the end until the budget is spent.
    AllJoined,
}

/// What `select_reply` chose, with enough of what it chose between to
/// tell a preamble grading from a real one after the fact.
///
/// The counts travel with the text because nothing in the store
/// records which round a verdict came from, so a score computed on 62
/// characters of preamble and a score computed on a whole turn are
/// indistinguishable once written.
#[derive(Clone, Debug)]
pub struct SelectedReply {
    /// The text to grade, already cut to the budget.
    pub text: String,
    /// The round the context and the instruction are read from. For
    /// `AllJoined` this is the last round that carried a reply, since
    /// the conversation grounding a conclusion is the one it was
    /// written at the end of.
    pub anchor: CapturedRound,
    /// Where the anchor sits among the rounds that carried a reply,
    /// counting from zero in trace order.
    pub anchor_index: usize,
    /// How many rounds in the trace carried a reply at all.
    pub replies_available: usize,
    /// Characters of reply text in the whole trace before the budget,
    /// which is the denominator for how much of a turn a verdict saw.
    pub chars_available: usize,
}

impl CaptureReader {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// The round a span produced, or `None` when there is no file, it is
    /// unreadable, or it does not parse. A missing round is the ordinary
    /// case rather than a fault: it is what every SDK span and every
    /// tier 1 install looks like.
    pub fn round(&self, started_at_ms: i64, span_id: &str) -> Option<CapturedRound> {
        let path = round_path(&self.root, started_at_ms, span_id);
        let bytes = std::fs::read(path).ok()?;
        Some(CapturedRound {
            value: serde_json::from_slice(&bytes).ok()?,
        })
    }

    /// The conversation the round was sent with, most recent last, cut to
    /// `budget` characters from the end.
    ///
    /// The walk runs backwards and keeps whole messages, so what
    /// survives a cut is the end of the conversation rather than an
    /// arbitrary slice of it. Which end to keep is a real choice and
    /// not a storage detail, so the budget belongs to the caller, who
    /// also documents why it is the size it is.
    ///
    /// Tool calls and their results are rendered. They were dropped
    /// once, on the reasoning that a call is not prose, and that
    /// removed most of the conversation: over the stored corpus the
    /// text only rule deleted about two thirds of all messages, and
    /// what it deleted was every action the agent took and every piece
    /// of evidence it got back. A grader asked whether a reply was
    /// grounded, or whether the right tool was chosen, was being shown
    /// the talking and none of the doing. Replay confirms no budget
    /// repairs this: a known tool result stayed absent from the
    /// rendered context at four times the shipping budget and at
    /// sixteen, because the filter runs before the budget is ever
    /// consulted.
    pub fn context(&self, round: &CapturedRound, budget: usize) -> Option<String> {
        self.context_with(round, budget, ContextMode::WithTools)
    }

    /// `context`, with the rendering rule named by the caller.
    pub fn context_with(
        &self,
        round: &CapturedRound,
        budget: usize,
        mode: ContextMode,
    ) -> Option<String> {
        let messages = round.value.get("request")?.get("messages")?.as_array()?;
        let mut kept: Vec<String> = Vec::new();
        let mut used = 0usize;
        for entry in messages.iter().rev() {
            // A message that will not resolve is skipped rather than
            // abandoning the conversation with it. The walk already
            // drops the head once the budget is spent, so a hole is the
            // same kind of loss as the cut, and a partial context still
            // grounds the reply better than none.
            let Some(resolved) = self.resolve(entry) else {
                continue;
            };
            let Some(text) = render_message(&resolved, mode) else {
                continue;
            };
            if used + text.len() > budget {
                // Keep something rather than nothing when the newest
                // message alone is over budget: a reply grounded in a
                // 60 kB tool result is still better judged against the
                // head of that result than against silence.
                if kept.is_empty() {
                    kept.push(text.chars().take(budget).collect());
                }
                break;
            }
            used += text.len();
            kept.push(text);
        }
        if kept.is_empty() {
            return None;
        }
        kept.reverse();
        Some(kept.join("\n\n"))
    }

    /// What the agent was last actually asked to do, or `None` when the
    /// turn holds nothing but tool traffic and the client talking to
    /// itself.
    ///
    /// This is the newest user message that is not tool output, with
    /// the client's injected blocks taken out, and the walk keeps going
    /// back when a message strips to nothing. It is returned whole:
    /// one message is not a budgeted walk, and cutting it to fit a
    /// prompt is the caller's business along with the marker that cut
    /// leaves behind.
    ///
    /// It exists because the end of a conversation is the wrong place
    /// to look for the goal. Across 400 tool calling rounds in a real
    /// corpus the last 1,500 characters held the standing instruction
    /// half the time; when they missed it, it sat a median of twelve
    /// messages further back, under the tool output that displaced it.
    pub fn instruction(&self, round: &CapturedRound) -> Option<String> {
        let messages = round.value.get("request")?.get("messages")?.as_array()?;
        for entry in messages.iter().rev() {
            let Some(resolved) = self.resolve(entry) else {
                continue;
            };
            if resolved.get("role").and_then(|r| r.as_str()) != Some("user") {
                continue;
            }
            let Some(content) = resolved.get("content") else {
                continue;
            };
            if holds_tool_result(content) {
                continue;
            }
            let Some(text) = block_text(content) else {
                continue;
            };
            let stripped = strip_injections(&text);
            let stripped = stripped.trim();
            if !stripped.is_empty() {
                return Some(stripped.to_string());
            }
        }
        None
    }

    /// The reply to grade, chosen from every round the trace produced.
    ///
    /// `rounds` is the trace's spans in start order, which is what
    /// `list_spans_for_trace` returns and what the judge is handed. A
    /// span with no stored round, and a round whose reply is empty,
    /// are both passed over rather than counted: an empty reply is
    /// what a pure tool call looks like, and a tool using turn has
    /// more of those than it has replies.
    pub fn select_reply(
        &self,
        rounds: &[(i64, String)],
        budget: usize,
        mode: ReplyMode,
    ) -> Option<SelectedReply> {
        let mut found: Vec<(CapturedRound, String)> = Vec::new();
        for (started_at_ms, span_id) in rounds {
            let Some(round) = self.round(*started_at_ms, span_id) else {
                continue;
            };
            let Some(reply) = round.reply() else {
                continue;
            };
            found.push((round, reply));
        }
        let replies_available = found.len();
        if replies_available == 0 {
            return None;
        }
        let chars_available = found.iter().map(|(_, r)| r.chars().count()).sum();

        let anchor_index = match mode {
            ReplyMode::First => 0,
            ReplyMode::Last | ReplyMode::AllJoined => replies_available - 1,
            // Strictly greater, so a tie keeps the earliest instead of
            // depending on which way a comparison happens to fall.
            ReplyMode::Largest => {
                let mut best = 0usize;
                for (i, (_, reply)) in found.iter().enumerate() {
                    if reply.chars().count() > found[best].1.chars().count() {
                        best = i;
                    }
                }
                best
            }
        };

        let text = match mode {
            ReplyMode::AllJoined => join_from_end(&found, budget),
            _ => cut(&found[anchor_index].1, budget),
        };
        let anchor = found.swap_remove(anchor_index).0;

        Some(SelectedReply {
            text,
            anchor,
            anchor_index,
            replies_available,
            chars_available,
        })
    }

    /// One entry from a round's message list, which is either a name
    /// pointing into the store or the message written inline.
    fn resolve(&self, entry: &serde_json::Value) -> Option<serde_json::Value> {
        match entry.as_str() {
            Some(name) => {
                let path = message_path(&self.root, name)?;
                let bytes = std::fs::read(path).ok()?;
                serde_json::from_slice(&bytes).ok()
            }
            None => Some(entry.clone()),
        }
    }
}

impl CapturedRound {
    /// The assistant's reply. A stream is stored with its text already
    /// accumulated into one string; a single shot is stored as the body
    /// arrived, which is a list of blocks. An upstream error is stored
    /// too and carries no reply at all.
    pub fn reply(&self) -> Option<String> {
        let text = block_text(self.value.get("response")?.get("content")?)?;
        (!text.is_empty()).then_some(text)
    }
}

/// What separates two replies joined into one text to grade.
const REPLY_SEPARATOR: &str = "\n\n";

/// Cuts to a character budget, marking the cut so a judge scoring a
/// half sentence can see why it is one.
///
/// This is the rule the judge applied before the choice of reply moved
/// in here, so `ReplyMode::First` replays what shipped rather than
/// something close to it.
fn cut(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_string();
    }
    let kept: String = text.chars().take(budget).collect();
    format!("{kept}\n[truncated]")
}

/// Every reply, newest first, kept whole while the budget lasts and
/// then put back into trace order.
///
/// Whole replies, because half a reply grades no better than the
/// preamble this mode exists to stop grading. The separator is counted
/// against the budget: `context_with` does not count its own, and a
/// context there can pass the ceiling in bytes without its cut ever
/// running.
fn join_from_end(found: &[(CapturedRound, String)], budget: usize) -> String {
    let mut kept: Vec<&str> = Vec::new();
    let mut used = 0usize;
    for (_, reply) in found.iter().rev() {
        let cost = reply.len()
            + if kept.is_empty() {
                0
            } else {
                REPLY_SEPARATOR.len()
            };
        if used + cost > budget {
            // Keep something rather than nothing when the newest reply
            // alone is over budget, on the reasoning `context_with`
            // keeps the head of an oversized message.
            if kept.is_empty() {
                return cut(reply, budget);
            }
            break;
        }
        used += cost;
        kept.push(reply);
    }
    kept.reverse();
    kept.join(REPLY_SEPARATOR)
}

/// Anthropic content, which is a bare string on one path and a list of
/// typed blocks on the other. Only `text` blocks are joined.
///
/// This is the superseded rule. It is reachable only through
/// `ContextMode::TextOnly`, which exists so a replay can render what
/// the judge used to be shown and measure the difference.
fn block_text(content: &serde_json::Value) -> Option<String> {
    match content {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(blocks) => {
            let joined: Vec<&str> = blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect();
            Some(joined.join(""))
        }
        _ => None,
    }
}

/// Whether a message is the client handing tool output back. Decided
/// on block type and not on role, because the API carries these in the
/// user turn and none of it is the user speaking.
fn holds_tool_result(content: &serde_json::Value) -> bool {
    content.as_array().is_some_and(|blocks| {
        blocks
            .iter()
            .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
    })
}

/// Takes the client's own blocks back out of a user message.
///
/// Claude Code writes context reminders and a running token banner into
/// the user turn. They are the client addressing the model, not the
/// operator addressing the agent, and in a sample of tool calling
/// rounds they were the entire newest user message about one time in
/// twenty. Left in, they are what tool choice gets judged against.
fn strip_injections(text: &str) -> String {
    let mut out = text.to_string();
    for (open, close) in [
        ("<system-reminder>", "</system-reminder>"),
        ("<total_tokens>", "</total_tokens>"),
    ] {
        out = strip_spans(&out, open, close);
    }
    out
}

/// Drops every `open`..`close` span. An opener with no closer keeps
/// what follows it, because throwing away the rest of a message over
/// one malformed tag loses far more than it removes.
fn strip_spans(text: &str, open: &str, close: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(open) {
        let Some(end) = rest[start + open.len()..].find(close) else {
            break;
        };
        out.push_str(&rest[..start]);
        rest = &rest[start + open.len() + end + close.len()..];
    }
    out.push_str(rest);
    out
}

fn render_message(msg: &serde_json::Value, mode: ContextMode) -> Option<String> {
    let role = msg
        .get("role")
        .and_then(|r| r.as_str())
        .unwrap_or("unknown");
    let text = match mode {
        ContextMode::TextOnly => block_text(msg.get("content")?)?,
        ContextMode::WithTools => block_text_with_tools(msg.get("content")?, None)?,
        ContextMode::CappedTools(cap) => block_text_with_tools(msg.get("content")?, Some(cap))?,
    };
    (!text.trim().is_empty()).then(|| format!("{role}: {text}"))
}

/// `block_text`, plus the tool traffic it drops.
///
/// A call is rendered with its arguments because the arguments are
/// what a tool choice is judged on, and a result is rendered because it
/// is the evidence a reply is grounded in. `cap` cuts each result
/// separately rather than cutting the conversation, so one large result
/// costs one large result instead of every message older than it.
fn block_text_with_tools(content: &serde_json::Value, cap: Option<usize>) -> Option<String> {
    match content {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(blocks) => {
            let mut parts: Vec<String> = Vec::new();
            for b in blocks {
                let kind = b.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match kind {
                    "text" => {
                        if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                            parts.push(t.to_string());
                        }
                    }
                    "tool_use" => {
                        let name = b.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                        let args = b
                            .get("input")
                            .map(|i| i.to_string())
                            .unwrap_or_else(|| "{}".to_string());
                        parts.push(format!("[calls {name} with {args}]"));
                    }
                    "tool_result" => {
                        let body = b.get("content").map(nested_text).unwrap_or_default();
                        parts.push(match cap {
                            Some(n) if body.chars().count() > n => {
                                let head: String = body.chars().take(n).collect();
                                let total = body.chars().count();
                                format!("[result, {total} chars, first {n}: {head}]")
                            }
                            _ => format!("[result: {body}]"),
                        });
                    }
                    _ => {}
                }
            }
            Some(parts.join("\n"))
        }
        _ => None,
    }
}

/// The text inside a `tool_result`, which the API carries as a bare
/// string on one path and as a block list on the other.
fn nested_text(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, value: serde_json::Value) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, serde_json::to_vec(&value).expect("encode")).expect("write");
    }

    #[test]
    fn a_round_is_addressed_by_its_span() {
        let root = Path::new("/tmp/c");
        assert_eq!(
            round_path(root, 1787225395662, "609f06dfe14f78d4"),
            Path::new("/tmp/c/rounds/1787225395662-609f06dfe14f78d4.json")
        );
    }

    #[test]
    fn a_message_is_sharded_by_the_head_of_its_name() {
        let root = Path::new("/tmp/c");
        assert_eq!(message_name(0x0031d9fdf644dc9d), "0031d9fdf644dc9d");
        assert_eq!(
            message_path(root, "0031d9fdf644dc9d").expect("path"),
            Path::new("/tmp/c/messages/00/0031d9fdf644dc9d.json")
        );
    }

    #[test]
    fn an_unusable_message_name_has_no_path() {
        let root = Path::new("/tmp/c");
        assert!(message_path(root, "a").is_none());
        assert!(message_path(root, "../../etc/passwd").is_none());
        assert!(message_path(root, "").is_none());
    }

    /// A turn of tool calls with replies at both ends, which is the
    /// shape the choice of rule exists for.
    fn tool_turn(dir: &Path) -> Vec<(i64, String)> {
        let rounds = [
            (1000, "a", Some("on it")),
            (2000, "b", None),
            (3000, "c", Some("halfway")),
            (4000, "d", None),
            (5000, "e", Some("the answer is 42, from three files")),
        ];
        for (ms, span, reply) in rounds {
            let response = match reply {
                Some(text) => serde_json::json!({"content": text}),
                None => serde_json::json!({"content": ""}),
            };
            write(
                &round_path(dir, ms, span),
                serde_json::json!({"response": response}),
            );
        }
        rounds
            .iter()
            .map(|(ms, span, _)| (*ms, span.to_string()))
            .collect()
    }

    #[test]
    fn each_rule_picks_the_reply_it_says_it_does() {
        let dir = tempfile::tempdir().expect("tempdir");
        let spans = tool_turn(dir.path());
        let reader = CaptureReader::new(dir.path().to_path_buf());

        let first = reader
            .select_reply(&spans, 4_000, ReplyMode::First)
            .expect("first");
        assert_eq!(first.text, "on it");
        assert_eq!(first.anchor_index, 0);

        let last = reader
            .select_reply(&spans, 4_000, ReplyMode::Last)
            .expect("last");
        assert_eq!(last.text, "the answer is 42, from three files");
        assert_eq!(last.anchor_index, 2);

        let largest = reader
            .select_reply(&spans, 4_000, ReplyMode::Largest)
            .expect("largest");
        assert_eq!(largest.text, "the answer is 42, from three files");

        let joined = reader
            .select_reply(&spans, 4_000, ReplyMode::AllJoined)
            .expect("joined");
        assert_eq!(
            joined.text,
            "on it\n\nhalfway\n\nthe answer is 42, from three files"
        );
    }

    /// The counts are the audit record, so they describe the turn and
    /// not the slice of it that was chosen.
    #[test]
    fn the_counts_describe_the_turn_not_the_choice() {
        let dir = tempfile::tempdir().expect("tempdir");
        let spans = tool_turn(dir.path());
        let reader = CaptureReader::new(dir.path().to_path_buf());
        for mode in [
            ReplyMode::First,
            ReplyMode::Last,
            ReplyMode::Largest,
            ReplyMode::AllJoined,
        ] {
            let sel = reader.select_reply(&spans, 4_000, mode).expect("reply");
            assert_eq!(sel.replies_available, 3, "{mode:?}");
            assert_eq!(sel.chars_available, 5 + 7 + 34, "{mode:?}");
        }
    }

    /// The rounds with no reply outnumber the ones with a reply on a
    /// tool using turn, and a span with nothing stored under it at all
    /// is the tier 1 case rather than a fault.
    #[test]
    fn empty_and_missing_rounds_are_passed_over() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut spans = tool_turn(dir.path());
        spans.insert(0, (500, "never-stored".to_string()));
        let reader = CaptureReader::new(dir.path().to_path_buf());
        let sel = reader
            .select_reply(&spans, 4_000, ReplyMode::First)
            .expect("reply");
        assert_eq!(sel.text, "on it");
        assert_eq!(sel.replies_available, 3);
    }

    #[test]
    fn a_turn_with_no_reply_anywhere_selects_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            &round_path(dir.path(), 1000, "a"),
            serde_json::json!({"response": {"content": ""}}),
        );
        let reader = CaptureReader::new(dir.path().to_path_buf());
        assert!(
            reader
                .select_reply(&[(1000, "a".to_string())], 4_000, ReplyMode::AllJoined)
                .is_none()
        );
    }

    /// Whole replies, oldest dropped first. Half a reply grades no
    /// better than the preamble the joining rule exists to stop
    /// grading.
    #[test]
    fn the_join_drops_whole_replies_from_the_front() {
        let dir = tempfile::tempdir().expect("tempdir");
        let spans = tool_turn(dir.path());
        let reader = CaptureReader::new(dir.path().to_path_buf());
        let sel = reader
            .select_reply(&spans, 45, ReplyMode::AllJoined)
            .expect("reply");
        assert_eq!(sel.text, "halfway\n\nthe answer is 42, from three files");
        // The turn is still reported whole, so the row records that
        // something was left out rather than hiding it.
        assert_eq!(sel.chars_available, 46);
    }

    /// The newest reply alone can be over budget, and the head of it
    /// still grades better than silence.
    #[test]
    fn an_oversized_newest_reply_is_cut_rather_than_dropped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let spans = tool_turn(dir.path());
        let reader = CaptureReader::new(dir.path().to_path_buf());
        let sel = reader
            .select_reply(&spans, 10, ReplyMode::AllJoined)
            .expect("reply");
        assert_eq!(sel.text, "the answer\n[truncated]");
    }

    #[test]
    fn a_streamed_reply_is_one_string() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            &round_path(dir.path(), 1000, "abc"),
            serde_json::json!({"response": {"content": "hello", "stream_outcome": "complete"}}),
        );
        let reader = CaptureReader::new(dir.path().to_path_buf());
        let round = reader.round(1000, "abc").expect("round");
        assert_eq!(round.reply(), Some("hello".to_string()));
    }

    #[test]
    fn a_single_shot_reply_joins_its_text_blocks() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            &round_path(dir.path(), 1000, "abc"),
            serde_json::json!({"response": {"content": [
                {"type": "text", "text": "hel"},
                {"type": "tool_use", "name": "grep"},
                {"type": "text", "text": "lo"},
            ]}}),
        );
        let reader = CaptureReader::new(dir.path().to_path_buf());
        let round = reader.round(1000, "abc").expect("round");
        assert_eq!(round.reply(), Some("hello".to_string()));
    }

    #[test]
    fn an_upstream_error_carries_no_reply() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            &round_path(dir.path(), 1000, "abc"),
            serde_json::json!({"response": {
                "type": "error",
                "error": {"message": "x-api-key header is required"}
            }}),
        );
        let reader = CaptureReader::new(dir.path().to_path_buf());
        assert_eq!(reader.round(1000, "abc").expect("round").reply(), None);
    }

    #[test]
    fn a_missing_round_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let reader = CaptureReader::new(dir.path().to_path_buf());
        assert!(reader.round(1000, "nothing-here").is_none());
    }

    #[test]
    fn context_resolves_hashed_names_in_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        for (name, role, text) in [
            ("0000000000000001", "user", "first"),
            ("0000000000000002", "assistant", "second"),
        ] {
            write(
                &message_path(dir.path(), name).expect("path"),
                serde_json::json!({"role": role, "content": text}),
            );
        }
        write(
            &round_path(dir.path(), 1000, "abc"),
            serde_json::json!({"request": {"messages": [
                "0000000000000001",
                "0000000000000002",
            ]}}),
        );
        let reader = CaptureReader::new(dir.path().to_path_buf());
        let round = reader.round(1000, "abc").expect("round");
        assert_eq!(
            reader.context(&round, 8_000),
            Some("user: first\n\nassistant: second".to_string())
        );
    }

    #[test]
    fn context_keeps_the_tail_when_the_budget_runs_out() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            &round_path(dir.path(), 1000, "abc"),
            serde_json::json!({"request": {"messages": [
                {"role": "user", "content": "an old message nobody needs"},
                {"role": "user", "content": "recent"},
            ]}}),
        );
        let reader = CaptureReader::new(dir.path().to_path_buf());
        let round = reader.round(1000, "abc").expect("round");
        assert_eq!(reader.context(&round, 20), Some("user: recent".to_string()));
    }

    #[test]
    fn context_keeps_the_head_of_a_message_that_alone_is_over_budget() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            &round_path(dir.path(), 1000, "abc"),
            serde_json::json!({"request": {"messages": [
                {"role": "user", "content": "a tool result far larger than the budget"},
            ]}}),
        );
        let reader = CaptureReader::new(dir.path().to_path_buf());
        let round = reader.round(1000, "abc").expect("round");
        assert_eq!(reader.context(&round, 10), Some("user: a to".to_string()));
    }

    #[test]
    fn context_renders_the_calls_and_the_results_they_returned() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            &round_path(dir.path(), 1000, "abc"),
            serde_json::json!({"request": {"messages": [
                {"role": "user", "content": "who owns the retry budget"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "checking"},
                    {"type": "tool_use", "name": "grep", "input": {"pattern": "retry"}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "content": "llm_judge.rs:517 holds it"},
                ]},
            ]}}),
        );
        let reader = CaptureReader::new(dir.path().to_path_buf());
        let round = reader.round(1000, "abc").expect("round");
        let ctx = reader.context(&round, 8_000).expect("context");
        assert!(ctx.contains("[calls grep with {\"pattern\":\"retry\"}]"));
        assert!(ctx.contains("[result: llm_judge.rs:517 holds it]"));
        assert!(ctx.contains("checking"));
    }

    #[test]
    fn the_superseded_rule_still_renders_nothing_but_text() {
        // The replay compares the shipping rule against the one it
        // replaced, so the old rendering has to keep working to be
        // worth comparing against. This pins it.
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            &round_path(dir.path(), 1000, "abc"),
            serde_json::json!({"request": {"messages": [
                {"role": "assistant", "content": [
                    {"type": "text", "text": "checking"},
                    {"type": "tool_use", "name": "grep", "input": {"pattern": "retry"}},
                ]},
            ]}}),
        );
        let reader = CaptureReader::new(dir.path().to_path_buf());
        let round = reader.round(1000, "abc").expect("round");
        assert_eq!(
            reader.context_with(&round, 8_000, ContextMode::TextOnly),
            Some("assistant: checking".to_string())
        );
    }

    #[test]
    fn a_capped_result_says_how_much_it_is_not_showing() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            &round_path(dir.path(), 1000, "abc"),
            serde_json::json!({"request": {"messages": [
                {"role": "user", "content": [
                    {"type": "tool_result", "content": "0123456789abcdef"},
                ]},
            ]}}),
        );
        let reader = CaptureReader::new(dir.path().to_path_buf());
        let round = reader.round(1000, "abc").expect("round");
        assert_eq!(
            reader.context_with(&round, 8_000, ContextMode::CappedTools(4)),
            Some("user: [result, 16 chars, first 4: 0123]".to_string())
        );
    }

    #[test]
    fn the_instruction_is_the_newest_thing_the_operator_said() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            &round_path(dir.path(), 1000, "abc"),
            serde_json::json!({"request": {"messages": [
                {"role": "user", "content": "find the leak"},
                {"role": "assistant", "content": "on it"},
                {"role": "user", "content": "actually start with the tests"},
                {"role": "assistant", "content": "running them"},
                {"role": "user", "content": [{"type": "tool_result", "content": "42 passed"}]},
            ]}}),
        );
        let reader = CaptureReader::new(dir.path().to_path_buf());
        let round = reader.round(1000, "abc").expect("round");
        assert_eq!(
            reader.instruction(&round),
            Some("actually start with the tests".to_string())
        );
    }

    #[test]
    fn the_instruction_walks_past_a_message_that_is_all_client_chatter() {
        // The turn the metric cares about often ends with the client
        // topping up its own context, and that is not an instruction
        // even though it arrives in the user role.
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            &round_path(dir.path(), 1000, "abc"),
            serde_json::json!({"request": {"messages": [
                {"role": "user", "content": "ship the release"},
                {"role": "assistant", "content": "starting"},
                {"role": "user", "content": "<system-reminder>be careful</system-reminder>"},
            ]}}),
        );
        let reader = CaptureReader::new(dir.path().to_path_buf());
        let round = reader.round(1000, "abc").expect("round");
        assert_eq!(
            reader.instruction(&round),
            Some("ship the release".to_string())
        );
    }

    #[test]
    fn injections_come_out_of_an_instruction_that_also_says_something() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            &round_path(dir.path(), 1000, "abc"),
            serde_json::json!({"request": {"messages": [{"role": "user", "content":
                "<system-reminder>context</system-reminder>rerun it<total_tokens>900</total_tokens>"
            }]}}),
        );
        let reader = CaptureReader::new(dir.path().to_path_buf());
        let round = reader.round(1000, "abc").expect("round");
        assert_eq!(reader.instruction(&round), Some("rerun it".to_string()));
    }

    #[test]
    fn an_unclosed_injection_does_not_swallow_the_instruction() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            &round_path(dir.path(), 1000, "abc"),
            serde_json::json!({"request": {"messages": [
                {"role": "user", "content": "<system-reminder>truncated mid tag rerun it"},
            ]}}),
        );
        let reader = CaptureReader::new(dir.path().to_path_buf());
        let round = reader.round(1000, "abc").expect("round");
        assert_eq!(
            reader.instruction(&round),
            Some("<system-reminder>truncated mid tag rerun it".to_string())
        );
    }

    #[test]
    fn a_turn_of_pure_tool_traffic_has_no_instruction() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            &round_path(dir.path(), 1000, "abc"),
            serde_json::json!({"request": {"messages": [
                {"role": "assistant", "content": "calling grep"},
                {"role": "user", "content": [{"type": "tool_result", "content": "no matches"}]},
            ]}}),
        );
        let reader = CaptureReader::new(dir.path().to_path_buf());
        let round = reader.round(1000, "abc").expect("round");
        assert_eq!(reader.instruction(&round), None);
    }

    #[test]
    fn an_unresolvable_message_does_not_lose_the_rest() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            &message_path(dir.path(), "0000000000000002").expect("path"),
            serde_json::json!({"role": "assistant", "content": "second"}),
        );
        write(
            &round_path(dir.path(), 1000, "abc"),
            serde_json::json!({"request": {"messages": [
                "0000000000000001",
                "0000000000000002",
            ]}}),
        );
        let reader = CaptureReader::new(dir.path().to_path_buf());
        let round = reader.round(1000, "abc").expect("round");
        assert_eq!(
            reader.context(&round, 8_000),
            Some("assistant: second".to_string())
        );
    }
}
