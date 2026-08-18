//! Conversation threading for the proxy path: reconstructing an agentic
//! session's task structure from nothing but the traffic.
//!
//! An agentic client resends the full conversation on every call, so a
//! request whose `messages` array extends a known conversation's prefix
//! belongs to that conversation. One conversation turn (every round trip
//! from a user message until the assistant stops requesting tools) is one
//! trace: chat spans arrive as children of a synthetic turn root that is
//! emitted only when the turn ends, mirroring how SDK agents emit their
//! task root last. A `tool_use` block in a response plus the matching
//! `tool_result` in the next request reconstructs the tool call as a
//! child span covering the gap between the two.
//!
//! All state is in memory. A prefix mismatch (context compaction, an
//! edited history, a restart) starts a fresh conversation, and nothing
//! in the request tells that apart from a conversation genuinely
//! beginning, so every placement also records how far the nearest
//! candidate agreed and how many candidates there were. ADR-0047.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::{Duration, Instant, SystemTime};

/// Conversations quiet for this long are forgotten. Generous, because the
/// cost of remembering is a few hashes and the cost of forgetting is a
/// split trace.
const PRUNE_AFTER: Duration = Duration::from_secs(30 * 60);

/// A tool call the assistant requested whose result has not yet come
/// back through the conversation.
#[derive(Debug, Clone)]
pub struct PendingTool {
    pub tool_use_id: String,
    pub name: String,
    /// When the response carrying the tool_use finished: the tool span
    /// starts here.
    pub requested_at: SystemTime,
}

/// A reconstructed tool call, ready to be synthesized as a span.
#[derive(Debug)]
pub struct ToolCall {
    pub name: String,
    pub started_at: SystemTime,
    pub ended_at: SystemTime,
    pub is_error: bool,
    /// Parent chat span: the one whose response requested the tool.
    pub parent_span_id: Vec<u8>,
    /// Hash of the tool's input from the replayed `tool_use` block,
    /// computed in memory so the input itself is never stored. Loop
    /// detection uses it to tell "same tool, different work" from
    /// "same call repeated". None when the block was not found.
    pub input_hash: Option<String>,
}

/// What the tracker decided about an incoming request.
#[derive(Debug)]
pub struct TurnPlacement {
    pub trace_id: Vec<u8>,
    /// The synthetic root every span of this turn parents to.
    pub root_span_id: Vec<u8>,
    /// Tool calls closed by this request's tool_result blocks.
    pub tools: Vec<ToolCall>,
    /// Messages in the request, recorded on the chat span so context
    /// growth is visible per turn.
    pub message_count: usize,
    /// True when this request started a brand new conversation.
    pub new_conversation: bool,
    /// One fingerprint per message, in order. Stamped on the span so a
    /// threading decision can be re-derived from storage afterwards;
    /// hashes, not content, so this survives at any privacy tier.
    pub message_hashes: Vec<u64>,
    /// How many leading messages the closest known conversation agreed
    /// with, whether or not it agreed far enough to match. On a match
    /// this is that conversation's whole stored history; on a miss it is
    /// where the two histories diverged, which is the only thing that
    /// explains why the match failed.
    pub matched_prefix: usize,
    /// Known conversations for this agent when the decision was made.
    /// Zero with a miss means nothing was there to match; nonzero means
    /// something was, and did not fit.
    pub candidates: usize,
}

/// What the tracker needs to know about a finished response.
pub struct ResponseInfo {
    /// The chat span the proxy synthesized for this round trip.
    pub chat_span_id: Vec<u8>,
    /// Tool calls the assistant requested (id, name).
    pub tool_uses: Vec<(String, String)>,
    /// The stop reason; anything other than "tool_use" ends the turn.
    pub stop_reason: Option<String>,
    pub ended_at: SystemTime,
}

/// A turn root ready to be emitted: the no-parent span whose arrival
/// tells the assembler the trace is complete.
#[derive(Debug)]
pub struct TurnRoot {
    pub trace_id: Vec<u8>,
    pub span_id: Vec<u8>,
    pub name: String,
    pub started_at: SystemTime,
    pub ended_at: SystemTime,
}

struct Turn {
    trace_id: Vec<u8>,
    root_span_id: Vec<u8>,
    started_at: SystemTime,
    seq: u64,
    /// The chat span whose response is currently outstanding; tool spans
    /// synthesized from the NEXT request parent to it.
    last_chat_span: Vec<u8>,
    pending_tools: Vec<PendingTool>,
}

struct Conversation {
    /// Per-message fingerprints of the last request seen.
    message_hashes: Vec<u64>,
    turn: Option<Turn>,
    turns_completed: u64,
    last_seen: Instant,
}

#[derive(Default)]
pub struct ConversationTracker {
    /// Keyed by agent name: conversations from different tools never
    /// thread together even if their content collides.
    conversations: HashMap<String, Vec<Conversation>>,
}

impl ConversationTracker {
    /// Places an incoming request: same conversation and turn, same
    /// conversation but a new turn, or a brand new conversation. Also
    /// closes any pending tools the request's tool_result blocks answer.
    pub fn place_request(
        &mut self,
        agent: &str,
        messages: &[serde_json::Value],
        arrived: SystemTime,
        new_id: impl Fn(usize) -> Vec<u8>,
    ) -> TurnPlacement {
        self.prune();
        let hashes: Vec<u64> = messages.iter().map(hash_message).collect();
        let convs = self.conversations.entry(agent.to_string()).or_default();

        // Measured before the match rather than derived from it, because
        // the interesting number is the one on the failing path: a miss
        // tells you nothing, a miss at message 4 of 5 tells you exactly
        // which message the client rewrote.
        let candidates = convs.len();
        let matched_prefix = convs
            .iter()
            .map(|c| common_prefix(&c.message_hashes, &hashes))
            .max()
            .unwrap_or(0);

        // Longest stored prefix wins, so a conversation that happens to
        // extend another one's history matches its own record.
        let best = convs
            .iter_mut()
            .filter(|c| {
                !c.message_hashes.is_empty()
                    && hashes.len() >= c.message_hashes.len()
                    && hashes[..c.message_hashes.len()] == c.message_hashes[..]
            })
            .max_by_key(|c| c.message_hashes.len());

        match best {
            Some(conv) => {
                conv.last_seen = Instant::now();
                conv.message_hashes.clone_from(&hashes);
                let (turn, tools) = match conv.turn.take() {
                    // Turn still open: this request continues it (the
                    // client is answering tool calls).
                    Some(mut turn) => {
                        let tools = close_tools(&mut turn, messages, arrived);
                        (turn, tools)
                    }
                    // Previous turn ended: a new user message starts the
                    // next one, with a fresh trace.
                    None => (
                        Turn {
                            trace_id: new_id(16),
                            root_span_id: new_id(8),
                            started_at: arrived,
                            seq: conv.turns_completed + 1,
                            last_chat_span: Vec::new(),
                            pending_tools: Vec::new(),
                        },
                        Vec::new(),
                    ),
                };
                let placement = TurnPlacement {
                    trace_id: turn.trace_id.clone(),
                    root_span_id: turn.root_span_id.clone(),
                    tools,
                    message_count: messages.len(),
                    new_conversation: false,
                    message_hashes: hashes,
                    matched_prefix,
                    candidates,
                };
                conv.turn = Some(turn);
                placement
            }
            None => {
                // Every known conversation disagreed with this history
                // before it ended. Zero agreement is a genuinely new
                // conversation; partial agreement is the failure mode
                // that matters, because it means the client resent a
                // history Reeve had already recorded and no longer
                // recognises.
                tracing::debug!(
                    agent,
                    candidates,
                    matched_prefix,
                    incoming = hashes.len(),
                    known = ?convs.iter().map(|c| c.message_hashes.len()).collect::<Vec<_>>(),
                    "no conversation matched, starting a new one"
                );
                let turn = Turn {
                    trace_id: new_id(16),
                    root_span_id: new_id(8),
                    started_at: arrived,
                    seq: 1,
                    last_chat_span: Vec::new(),
                    pending_tools: Vec::new(),
                };
                let placement = TurnPlacement {
                    trace_id: turn.trace_id.clone(),
                    root_span_id: turn.root_span_id.clone(),
                    tools: Vec::new(),
                    message_count: messages.len(),
                    new_conversation: true,
                    message_hashes: hashes.clone(),
                    matched_prefix,
                    candidates,
                };
                convs.push(Conversation {
                    message_hashes: hashes,
                    turn: Some(turn),
                    turns_completed: 0,
                    last_seen: Instant::now(),
                });
                placement
            }
        }
    }

    /// Records a finished response. Returns the turn root to emit when
    /// the response ended the turn (the assistant stopped requesting
    /// tools), or None while the turn stays open.
    /// Records a finished response against the exact turn its request
    /// opened, identified by the trace id the placement returned. A
    /// recency guess sat here once; Claude Code's side calls run
    /// concurrently with the main conversation, and a side response
    /// closing the main turn was the first thing real traffic proved.
    pub fn record_response(
        &mut self,
        agent: &str,
        trace_id: &[u8],
        info: ResponseInfo,
    ) -> Option<TurnRoot> {
        let conv = self
            .conversations
            .get_mut(agent)?
            .iter_mut()
            .find(|c| c.turn.as_ref().is_some_and(|t| t.trace_id == trace_id))?;
        let turn = conv.turn.as_mut()?;

        turn.last_chat_span = info.chat_span_id;
        for (id, name) in info.tool_uses {
            turn.pending_tools.push(PendingTool {
                tool_use_id: id,
                name,
                requested_at: info.ended_at,
            });
        }

        if info.stop_reason.as_deref() == Some("tool_use") {
            return None;
        }
        // The assistant is done: close the turn and emit its root.
        let turn = conv.turn.take().expect("turn checked above");
        conv.turns_completed += 1;
        Some(TurnRoot {
            trace_id: turn.trace_id,
            span_id: turn.root_span_id,
            name: format!("agent.turn.{}", turn.seq),
            started_at: turn.started_at,
            ended_at: info.ended_at,
        })
    }

    fn prune(&mut self) {
        for convs in self.conversations.values_mut() {
            convs.retain(|c| c.last_seen.elapsed() < PRUNE_AFTER);
        }
        self.conversations.retain(|_, v| !v.is_empty());
    }
}

/// How many leading messages two histories agree on.
fn common_prefix(a: &[u64], b: &[u64]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

/// Matches this request's tool_result blocks against the turn's pending
/// tools, producing reconstructed tool calls.
fn close_tools(
    turn: &mut Turn,
    messages: &[serde_json::Value],
    arrived: SystemTime,
) -> Vec<ToolCall> {
    let mut tools = Vec::new();
    // tool_result blocks live in the trailing user message(s); scanning
    // all messages is correct because already-closed ids are gone from
    // pending_tools.
    for msg in messages {
        let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        for block in blocks {
            if block.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
                continue;
            }
            let Some(id) = block.get("tool_use_id").and_then(|v| v.as_str()) else {
                continue;
            };
            let Some(pos) = turn.pending_tools.iter().position(|p| p.tool_use_id == id) else {
                continue;
            };
            let pending = turn.pending_tools.remove(pos);
            tools.push(ToolCall {
                name: pending.name,
                started_at: pending.requested_at,
                ended_at: arrived,
                is_error: block
                    .get("is_error")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                parent_span_id: turn.last_chat_span.clone(),
                input_hash: tool_input_hash(messages, id),
            });
        }
    }
    tools
}

/// Finds the `tool_use` block matching this id in the replayed history
/// and hashes its `input` value. The hash never leaves memory as
/// anything but a fingerprint, so tier 1 privacy holds: two spans with
/// equal hashes did the same thing, and nothing says what that was.
/// serde_json's Display is deterministic for a given value (map order
/// preserved from the wire), and the comparison is within one turn's
/// replayed history, where the client resends the block verbatim.
fn tool_input_hash(messages: &[serde_json::Value], tool_use_id: &str) -> Option<String> {
    use std::hash::{DefaultHasher, Hash, Hasher};
    for msg in messages {
        let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        for block in blocks {
            if block.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                && block.get("id").and_then(|v| v.as_str()) == Some(tool_use_id)
            {
                let input = block.get("input")?;
                let mut hasher = DefaultHasher::new();
                input.to_string().hash(&mut hasher);
                return Some(format!("{:016x}", hasher.finish()));
            }
        }
    }
    None
}

/// Per-message fingerprint. DefaultHasher is deterministic within a
/// process, which is the only scope this state lives in.
///
/// The message is put in a canonical form first, because the raw JSON a
/// client sends encodes choices that carry no meaning, and a fingerprint
/// that sees them is a fingerprint of the serializer rather than of what
/// was said. See `canonical_message` for the two that bite.
fn hash_message(msg: &serde_json::Value) -> u64 {
    let mut hasher = DefaultHasher::new();
    match canonical_message(msg) {
        Some(canonical) => canonical.to_string().hash(&mut hasher),
        None => msg.to_string().hash(&mut hasher),
    }
    hasher.finish()
}

/// Rewrites a message into the form every encoding of it shares, or None
/// when it already is that form and can be hashed as it stands.
///
/// Two rewrites, both found in real Claude Code traffic:
///
/// `cache_control` is dropped, because prompt-caching clients move the
/// breakpoint marker forward to newer messages on every request, so a
/// resent message is byte-identical except for the marker appearing or
/// vanishing (#178).
///
/// A lone text block collapses to the bare string that says the same
/// thing, because a client may send one message either way and switch
/// between them mid-conversation. Claude Code's own token-budget system
/// message arrives as `[{"type":"text","text":"..."}]` at the tail of the
/// request that introduces it and as `"..."` when the next request
/// replays it, which diverged the prefix one message from its end on
/// every single turn and started a new conversation each time (#308).
fn canonical_message(msg: &serde_json::Value) -> Option<serde_json::Value> {
    let blocks = msg.get("content")?.as_array()?;

    if let Some(text) = lone_text(blocks) {
        let mut canonical = msg.clone();
        canonical["content"] = serde_json::Value::String(text.to_owned());
        return Some(canonical);
    }

    if !blocks.iter().any(|b| b.get("cache_control").is_some()) {
        return None;
    }
    let mut canonical = msg.clone();
    for block in canonical["content"].as_array_mut()?.iter_mut() {
        if let Some(obj) = block.as_object_mut() {
            obj.remove("cache_control");
        }
    }
    Some(canonical)
}

/// The text of a content array that says exactly what the bare-string
/// form of the same message says: a single text block carrying nothing
/// but its text. A `cache_control` marker does not disqualify it, since
/// the fingerprint drops that anyway. Any other key does, because the
/// string form could not have carried it.
fn lone_text(blocks: &[serde_json::Value]) -> Option<&str> {
    let [only] = blocks else { return None };
    let obj = only.as_object()?;
    if obj.get("type")?.as_str()? != "text" {
        return None;
    }
    if obj
        .keys()
        .any(|k| !matches!(k.as_str(), "type" | "text" | "cache_control"))
    {
        return None;
    }
    obj.get("text")?.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, text: &str) -> serde_json::Value {
        serde_json::json!({"role": role, "content": text})
    }

    fn tool_result(id: &str, is_error: bool) -> serde_json::Value {
        serde_json::json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": id, "content": "ok", "is_error": is_error}
        ]})
    }

    fn ids(n: usize) -> Vec<u8> {
        crate::proxy::test_random_bytes(n)
    }

    #[test]
    fn a_tool_loop_threads_into_one_turn() {
        let mut t = ConversationTracker::default();
        let now = SystemTime::now();

        // Request 1: user asks. New conversation, new turn.
        let m1 = vec![msg("user", "list the files")];
        let p1 = t.place_request("claude-cli", &m1, now, ids);
        assert!(p1.new_conversation);

        // Response 1: assistant requests a tool.
        let root = t.record_response(
            "claude-cli",
            &p1.trace_id,
            ResponseInfo {
                chat_span_id: vec![1; 8],
                tool_uses: vec![("toolu_1".into(), "bash".into())],
                stop_reason: Some("tool_use".into()),
                ended_at: now,
            },
        );
        assert!(root.is_none(), "tool_use keeps the turn open");

        // Request 2: same history + assistant msg + tool_result.
        let m2 = vec![
            msg("user", "list the files"),
            msg("assistant", "running bash"),
            tool_result("toolu_1", false),
        ];
        let p2 = t.place_request("claude-cli", &m2, now, ids);
        assert!(!p2.new_conversation);
        assert_eq!(p2.trace_id, p1.trace_id, "same turn, same trace");
        assert_eq!(p2.tools.len(), 1, "the tool call is reconstructed");
        assert_eq!(p2.tools[0].name, "bash");
        assert_eq!(
            p2.tools[0].parent_span_id,
            vec![1; 8],
            "tool parents to the chat span that requested it"
        );

        // Response 2: assistant finishes.
        let root = t.record_response(
            "claude-cli",
            &p2.trace_id,
            ResponseInfo {
                chat_span_id: vec![2; 8],
                tool_uses: vec![],
                stop_reason: Some("end_turn".into()),
                ended_at: now,
            },
        );
        let root = root.expect("end_turn closes the turn");
        assert_eq!(root.trace_id, p1.trace_id);
        assert_eq!(root.name, "agent.turn.1");
    }

    #[test]
    fn closed_tools_carry_the_input_fingerprint() {
        let mut t = ConversationTracker::default();
        let now = SystemTime::now();
        let p1 = t.place_request("cli", &[msg("user", "read both files")], now, ids);
        t.record_response(
            "cli",
            &p1.trace_id,
            ResponseInfo {
                chat_span_id: vec![1; 8],
                tool_uses: vec![
                    ("toolu_1".into(), "Read".into()),
                    ("toolu_2".into(), "Read".into()),
                ],
                stop_reason: Some("tool_use".into()),
                ended_at: now,
            },
        );
        // The replayed history carries the tool_use blocks verbatim,
        // inputs included; the results close both calls.
        let m2 = vec![
            msg("user", "read both files"),
            serde_json::json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "Read",
                 "input": {"path": "a.rs"}},
                {"type": "tool_use", "id": "toolu_2", "name": "Read",
                 "input": {"path": "b.rs"}},
            ]}),
            tool_result("toolu_1", false),
            tool_result("toolu_2", false),
        ];
        let p2 = t.place_request("cli", &m2, now, ids);
        assert_eq!(p2.tools.len(), 2);
        let h1 = p2.tools[0].input_hash.as_ref().expect("hash present");
        let h2 = p2.tools[1].input_hash.as_ref().expect("hash present");
        assert_ne!(h1, h2, "different inputs, different fingerprints");

        // The same input hashes the same: what a genuine loop looks like.
        assert_eq!(
            tool_input_hash(&m2, "toolu_1"),
            tool_input_hash(&m2, "toolu_1")
        );
        // A block that never appears yields no hash, not a fake one.
        assert_eq!(tool_input_hash(&m2, "toolu_missing"), None);
    }

    #[test]
    fn a_moving_cache_control_marker_does_not_break_the_prefix() {
        // The shape real Claude Code sends (#178): request 1 marks its
        // last content block as a cache breakpoint; request 2 resends
        // the same message WITHOUT the marker, because the breakpoint
        // moved forward to the newly appended messages.
        let mut t = ConversationTracker::default();
        let now = SystemTime::now();

        let marked = serde_json::json!({"role": "user", "content": [
            {"type": "text", "text": "list the files",
             "cache_control": {"type": "ephemeral", "ttl": "1h"}}
        ]});
        let unmarked = serde_json::json!({"role": "user", "content": [
            {"type": "text", "text": "list the files"}
        ]});

        let p1 = t.place_request("claude-cli", &[marked], now, ids);
        assert!(p1.new_conversation);
        t.record_response(
            "claude-cli",
            &p1.trace_id,
            ResponseInfo {
                chat_span_id: vec![1; 8],
                tool_uses: vec![("toolu_1".into(), "bash".into())],
                stop_reason: Some("tool_use".into()),
                ended_at: now,
            },
        );

        let m2 = vec![
            unmarked,
            msg("assistant", "running bash"),
            tool_result("toolu_1", false),
        ];
        let p2 = t.place_request("claude-cli", &m2, now, ids);
        assert!(
            !p2.new_conversation,
            "a moved cache marker must not read as a new conversation"
        );
        assert_eq!(p2.trace_id, p1.trace_id, "same turn, same trace");
        assert_eq!(p2.tools.len(), 1, "the tool span survives the marker move");
    }

    #[test]
    fn a_re_encoded_tail_message_does_not_split_the_conversation() {
        // The shape real Claude Code sends (#308): its token-budget
        // system message is the tail of the request that introduces it,
        // carried as a one-element content array, and comes back as a
        // bare string once the next request replays it mid-history. Same
        // text, two encodings, one message. Hashing the raw JSON made
        // the histories diverge one message from the end of every single
        // turn, which is the narrowest possible miss and started a fresh
        // conversation each time.
        let mut t = ConversationTracker::default();
        let now = SystemTime::now();

        let budget = "<total_tokens>1000000 tokens left</total_tokens>";
        let as_blocks = serde_json::json!({
            "role": "system",
            "content": [{"type": "text", "text": budget}]
        });

        let m1 = vec![msg("user", "list the files"), as_blocks];
        let p1 = t.place_request("claude-cli", &m1, now, ids);
        assert!(p1.new_conversation, "nothing to thread into yet");
        t.record_response(
            "claude-cli",
            &p1.trace_id,
            ResponseInfo {
                chat_span_id: vec![1; 8],
                tool_uses: vec![("toolu_1".into(), "bash".into())],
                stop_reason: Some("tool_use".into()),
                ended_at: now,
            },
        );

        let m2 = vec![
            msg("user", "list the files"),
            msg("system", budget),
            msg("assistant", "running bash"),
            tool_result("toolu_1", false),
        ];
        let p2 = t.place_request("claude-cli", &m2, now, ids);
        assert_eq!(
            p2.matched_prefix, 2,
            "the re-encoded message must agree, not just the one before it"
        );
        assert!(
            !p2.new_conversation,
            "a re-encoded tail message must not read as a new conversation"
        );
        assert_eq!(p2.trace_id, p1.trace_id, "same turn, same trace");
        assert_eq!(p2.tools.len(), 1, "the tool span survives the re-encoding");
    }

    #[test]
    fn only_a_bare_text_block_collapses_to_its_string() {
        let text = "the same words";
        let as_string = msg("user", text);
        let as_blocks = serde_json::json!({
            "role": "user", "content": [{"type": "text", "text": text}]
        });
        let marked = serde_json::json!({"role": "user", "content": [
            {"type": "text", "text": text, "cache_control": {"type": "ephemeral"}}
        ]});
        assert_eq!(hash_message(&as_string), hash_message(&as_blocks));
        assert_eq!(hash_message(&as_string), hash_message(&marked));

        // Only the content is canonicalized, so who said it still counts.
        assert_ne!(hash_message(&as_blocks), hash_message(&msg("system", text)));

        // A block carrying more than its text says something the string
        // form cannot, so it keeps a fingerprint of its own.
        let cited = serde_json::json!({"role": "user", "content": [
            {"type": "text", "text": text, "citations": ["doc-1"]}
        ]});
        assert_ne!(hash_message(&as_string), hash_message(&cited));

        // And two blocks are not one, however they read concatenated.
        let split = serde_json::json!({"role": "user", "content": [
            {"type": "text", "text": "the same "},
            {"type": "text", "text": "words"}
        ]});
        assert_ne!(hash_message(&as_string), hash_message(&split));
    }

    #[test]
    fn a_concurrent_side_call_cannot_close_the_main_turn() {
        // The shape from real Claude Code (#179): a side call (topic
        // detection) runs in parallel with the main conversation, and
        // its fast end_turn response arrives while the main response is
        // still streaming. It must close its own turn and only its own.
        let mut t = ConversationTracker::default();
        let now = SystemTime::now();

        let side = t.place_request("claude-cli", &[msg("user", "<session> topic?")], now, ids);
        let main = t.place_request("claude-cli", &[msg("user", "list the files")], now, ids);

        // The side response lands first, after the MAIN conversation was
        // the most recently seen: a recency guess closes the wrong turn.
        let root = t
            .record_response(
                "claude-cli",
                &side.trace_id,
                ResponseInfo {
                    chat_span_id: vec![9; 8],
                    tool_uses: vec![],
                    stop_reason: Some("end_turn".into()),
                    ended_at: now,
                },
            )
            .expect("the side turn closes");
        assert_eq!(root.trace_id, side.trace_id, "its own trace, not main's");

        // The main response then keeps its turn open with a tool request,
        // and the follow-up threads into the SAME main trace.
        let root = t.record_response(
            "claude-cli",
            &main.trace_id,
            ResponseInfo {
                chat_span_id: vec![1; 8],
                tool_uses: vec![("toolu_1".into(), "Bash".into())],
                stop_reason: Some("tool_use".into()),
                ended_at: now,
            },
        );
        assert!(root.is_none(), "main turn survives the side call");

        let m2 = vec![
            msg("user", "list the files"),
            msg("assistant", "running"),
            tool_result("toolu_1", false),
        ];
        let p2 = t.place_request("claude-cli", &m2, now, ids);
        assert_eq!(p2.trace_id, main.trace_id, "the tool loop stays one trace");
        assert_eq!(p2.tools.len(), 1, "and the tool span is reconstructed");
    }

    #[test]
    fn the_next_user_message_starts_a_new_trace() {
        let mut t = ConversationTracker::default();
        let now = SystemTime::now();

        let m1 = vec![msg("user", "hello")];
        let p1 = t.place_request("cli", &m1, now, ids);
        t.record_response(
            "cli",
            &p1.trace_id,
            ResponseInfo {
                chat_span_id: vec![1; 8],
                tool_uses: vec![],
                stop_reason: Some("end_turn".into()),
                ended_at: now,
            },
        );

        let m2 = vec![
            msg("user", "hello"),
            msg("assistant", "hi"),
            msg("user", "again"),
        ];
        let p2 = t.place_request("cli", &m2, now, ids);
        assert!(!p2.new_conversation, "same conversation continues");
        assert_ne!(p2.trace_id, p1.trace_id, "but each turn is its own trace");

        let root = t
            .record_response(
                "cli",
                &p2.trace_id,
                ResponseInfo {
                    chat_span_id: vec![2; 8],
                    tool_uses: vec![],
                    stop_reason: Some("end_turn".into()),
                    ended_at: now,
                },
            )
            .unwrap();
        assert_eq!(root.name, "agent.turn.2", "turn numbering survives");
    }

    #[test]
    fn prefix_mismatch_starts_a_fresh_conversation() {
        let mut t = ConversationTracker::default();
        let now = SystemTime::now();

        let m1 = vec![msg("user", "hello"), msg("user", "more")];
        let p1 = t.place_request("cli", &m1, now, ids);

        // Compaction rewrote history: nothing matches.
        let m2 = vec![msg("user", "summary of earlier"), msg("user", "next")];
        let p2 = t.place_request("cli", &m2, now, ids);
        assert!(
            p2.new_conversation,
            "mismatch degrades to a new conversation"
        );
        assert_ne!(p2.trace_id, p1.trace_id);
    }

    #[test]
    fn agents_never_thread_together() {
        let mut t = ConversationTracker::default();
        let now = SystemTime::now();
        let m = vec![msg("user", "identical")];
        let p1 = t.place_request("tool-a", &m, now, ids);
        let p2 = t.place_request("tool-b", &m, now, ids);
        assert_ne!(
            p1.trace_id, p2.trace_id,
            "identical content from different tools stays separate"
        );
    }

    #[test]
    fn errored_tool_results_mark_the_tool_failed() {
        let mut t = ConversationTracker::default();
        let now = SystemTime::now();
        let p1 = t.place_request("cli", &[msg("user", "go")], now, ids);
        t.record_response(
            "cli",
            &p1.trace_id,
            ResponseInfo {
                chat_span_id: vec![1; 8],
                tool_uses: vec![("toolu_9".into(), "web_search".into())],
                stop_reason: Some("tool_use".into()),
                ended_at: now,
            },
        );
        let m2 = vec![
            msg("user", "go"),
            msg("assistant", "searching"),
            tool_result("toolu_9", true),
        ];
        let p2 = t.place_request("cli", &m2, now, ids);
        assert!(p2.tools[0].is_error);
    }
}
