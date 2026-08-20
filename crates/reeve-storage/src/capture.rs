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
pub struct CapturedRound {
    value: serde_json::Value,
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
    pub fn context(&self, round: &CapturedRound, budget: usize) -> Option<String> {
        let messages = round.value.get("request")?.get("messages")?.as_array()?;
        let mut kept: Vec<String> = Vec::new();
        let mut used = 0usize;
        for entry in messages.iter().rev() {
            // A message that will not resolve is skipped rather than
            // abandoning the conversation with it. The walk already
            // drops the head once the budget is spent, so a hole is the
            // same kind of loss as the cut, and a partial context still
            // grounds the reply better than none.
            let resolved = match entry.as_str() {
                Some(name) => {
                    let Some(path) = message_path(&self.root, name) else {
                        continue;
                    };
                    match std::fs::read(path)
                        .ok()
                        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                    {
                        Some(v) => v,
                        None => continue,
                    }
                }
                None => entry.clone(),
            };
            let Some(text) = render_message(&resolved) else {
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

/// Anthropic content, which is a bare string on one path and a list of
/// typed blocks on the other. Only `text` blocks are joined: a tool call
/// is not prose and reads as noise inside a faithfulness prompt.
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

fn render_message(msg: &serde_json::Value) -> Option<String> {
    let role = msg
        .get("role")
        .and_then(|r| r.as_str())
        .unwrap_or("unknown");
    let text = block_text(msg.get("content")?)?;
    (!text.trim().is_empty()).then(|| format!("{role}: {text}"))
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
