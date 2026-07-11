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
//! edited history, a restart) starts a fresh conversation, which is the
//! proxy's pre-threading behavior: degradation is graceful by
//! construction.

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
                conv.message_hashes = hashes;
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
                };
                conv.turn = Some(turn);
                placement
            }
            None => {
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
            });
        }
    }
    tools
}

/// Per-message fingerprint. DefaultHasher is deterministic within a
/// process, which is the only scope this state lives in.
///
/// `cache_control` is stripped before hashing: prompt-caching clients
/// move the breakpoint marker forward to newer messages on every
/// request, so a resent message is byte-identical except for the marker
/// appearing or vanishing. The fingerprint covers what was said, not
/// how the API was told to cache it. Real Claude Code broke on this.
fn hash_message(msg: &serde_json::Value) -> u64 {
    let mut hasher = DefaultHasher::new();
    match msg.get("content").and_then(|c| c.as_array()) {
        Some(blocks) if blocks.iter().any(|b| b.get("cache_control").is_some()) => {
            let mut clean = msg.clone();
            if let Some(arr) = clean.get_mut("content").and_then(|c| c.as_array_mut()) {
                for block in arr.iter_mut() {
                    if let Some(obj) = block.as_object_mut() {
                        obj.remove("cache_control");
                    }
                }
            }
            clean.to_string().hash(&mut hasher);
        }
        _ => msg.to_string().hash(&mut hasher),
    }
    hasher.finish()
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
