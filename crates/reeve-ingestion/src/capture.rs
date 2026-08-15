//! Content capture on the proxy path, active only under privacy tier 2.
//!
//! The proxy parses every request body to thread the conversation and
//! then drops it, so a stored request has never existed on this path and
//! nothing downstream can replay one. This module keeps it, on disk,
//! beside the database rather than inside it: a conversation's request
//! carries its entire history every turn, so storing bodies naively is
//! quadratic in the turn count and a single real request has been
//! measured at 1.34 MB.
//!
//! The fix is content addressing. Threading already fingerprints every
//! message individually; those fingerprints are reused here as filenames,
//! so the hundredth turn of a conversation stores one new message and a
//! list of hashes it shares with the ninety-nine before it. Storage
//! becomes linear in the conversation rather than in its turns.
//!
//! Captured content is read by a human during analysis. Nothing derived
//! from it is ever computed into a feature or fed to a model.
//!
//! Writing happens on a blocking pool, detached from the request that
//! produced it, because capture must never add latency to a live turn.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// One round trip, as it will be stored.
pub struct Round {
    pub span_id: String,
    pub trace_id: String,
    pub agent: String,
    pub started_at_ms: i64,
    pub ended_at_ms: i64,
    /// The original request body, before any intervention was applied:
    /// what the client actually sent, which is also what threading
    /// fingerprinted.
    pub request: serde_json::Value,
    /// One fingerprint per entry of `request["messages"]`, in order, as
    /// computed by the threading tracker. Empty disables deduplication
    /// and stores the messages inline.
    pub message_hashes: Vec<u64>,
    /// What came back, reassembled: the accumulated response for a
    /// stream, the parsed body for a single shot.
    pub response: serde_json::Value,
}

/// A capture directory, opened once at startup.
pub struct Capture {
    root: PathBuf,
    /// Message hashes known to be on disk already. Purely an
    /// optimisation over the existence check in `write_message`: a miss
    /// costs one extra stat, never a wrong result.
    written: Mutex<HashSet<u64>>,
}

impl Capture {
    /// Creates the layout under `root`, failing loudly here so a broken
    /// capture directory is found at startup rather than discovered as
    /// an empty corpus weeks later.
    pub fn open(root: PathBuf) -> std::io::Result<Self> {
        std::fs::create_dir_all(root.join("rounds"))?;
        std::fs::create_dir_all(root.join("messages"))?;
        Ok(Self {
            root,
            written: Mutex::new(HashSet::new()),
        })
    }

    /// Queues a round and returns immediately. Failures are logged and
    /// dropped: losing a captured round is a gap in the corpus, while
    /// propagating the error would be a fault in someone's working
    /// session.
    pub fn record(self: &Arc<Self>, round: Round) {
        let capture = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            if let Err(e) = capture.write(round) {
                tracing::warn!(error = %e, "capture write failed");
            }
        });
    }

    fn write(&self, mut round: Round) -> std::io::Result<()> {
        let messages = round
            .request
            .get_mut("messages")
            .map(std::mem::take)
            .unwrap_or(serde_json::Value::Null);

        // Deduplication needs the hashes to line up one-to-one with the
        // array they were computed from. They always do, since both come
        // from the same parse of the same body; if that ever stops being
        // true the messages go back inline rather than being mismatched.
        let refs = match messages {
            serde_json::Value::Array(msgs) if msgs.len() == round.message_hashes.len() => {
                let mut refs = Vec::with_capacity(msgs.len());
                for (msg, hash) in msgs.into_iter().zip(&round.message_hashes) {
                    refs.push(serde_json::Value::String(self.write_message(*hash, &msg)?));
                }
                serde_json::Value::Array(refs)
            }
            other => other,
        };
        if let Some(obj) = round.request.as_object_mut() {
            obj.insert("messages".to_string(), refs);
        }

        let record = serde_json::json!({
            "span_id": round.span_id,
            "trace_id": round.trace_id,
            "agent": round.agent,
            "started_at_ms": round.started_at_ms,
            "ended_at_ms": round.ended_at_ms,
            "request": round.request,
            "response": round.response,
        });
        // Millis first so the directory sorts chronologically; the span
        // id keeps concurrent rounds from colliding.
        let name = format!("{}-{}.json", round.started_at_ms, round.span_id);
        write_atomic(&self.root.join("rounds").join(name), &record)
    }

    /// Writes one message under its fingerprint and returns the name to
    /// reference it by. Already-stored content is left untouched: the
    /// hash is the identity, so a rewrite could only produce the same
    /// bytes.
    fn write_message(&self, hash: u64, msg: &serde_json::Value) -> std::io::Result<String> {
        let name = format!("{hash:016x}");
        if self
            .written
            .lock()
            .expect("capture mutex poisoned")
            .contains(&hash)
        {
            return Ok(name);
        }
        // Sharded by the first byte: a long-running corpus accumulates
        // tens of thousands of these, and one flat directory of them is
        // slow to list and unpleasant to browse by hand.
        let dir = self.root.join("messages").join(&name[..2]);
        let path = dir.join(format!("{name}.json"));
        if !path.exists() {
            std::fs::create_dir_all(&dir)?;
            write_atomic(&path, msg)?;
        }
        self.written
            .lock()
            .expect("capture mutex poisoned")
            .insert(hash);
        Ok(name)
    }
}

/// Writes through a temporary file so a crash mid-write cannot leave a
/// truncated file behind. Truncation matters more here than usual:
/// content-addressed files are written once and never revisited, so a
/// torn one stays torn for the life of the corpus.
///
/// The temporary name is unique per call rather than derived from the
/// destination. Two rounds sharing a conversation prefix write the same
/// message concurrently, and a shared temporary would let their writes
/// interleave into a file that is then renamed into place.
fn write_atomic(path: &Path, value: &serde_json::Value) -> std::io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let tmp = path.with_extension(format!(
        "{}.{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let bytes = serde_json::to_vec(value)?;
    let result = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        // The name is never reused, so a failure would otherwise leave
        // its temporary behind for good.
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round(span: &str, msgs: Vec<serde_json::Value>, hashes: Vec<u64>) -> Round {
        Round {
            span_id: span.to_string(),
            trace_id: "trace".to_string(),
            agent: "claude-cli".to_string(),
            started_at_ms: 1000,
            ended_at_ms: 2000,
            request: serde_json::json!({ "model": "claude-opus-5", "messages": msgs }),
            message_hashes: hashes,
            response: serde_json::json!({ "content": "hi" }),
        }
    }

    #[test]
    fn shared_history_is_stored_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let capture = Capture::open(dir.path().to_path_buf()).expect("open");

        let first = serde_json::json!({ "role": "user", "content": "one" });
        let second = serde_json::json!({ "role": "user", "content": "two" });
        capture
            .write(round("a", vec![first.clone()], vec![1]))
            .expect("first round");
        // The second turn resends the first message, as every real turn
        // does. It must not produce a second copy.
        capture
            .write(round("b", vec![first, second], vec![1, 2]))
            .expect("second round");

        let stored: Vec<_> = walkdir(&dir.path().join("messages"));
        assert_eq!(stored.len(), 2, "one file per distinct message");
        assert_eq!(walkdir(&dir.path().join("rounds")).len(), 2);
    }

    #[test]
    fn round_references_messages_by_hash() {
        let dir = tempfile::tempdir().expect("tempdir");
        let capture = Capture::open(dir.path().to_path_buf()).expect("open");
        capture
            .write(round(
                "a",
                vec![serde_json::json!({ "role": "user", "content": "one" })],
                vec![0xab],
            ))
            .expect("round");

        let path = &walkdir(&dir.path().join("rounds"))[0];
        let stored: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).expect("read")).expect("parse");
        assert_eq!(
            stored["request"]["messages"],
            serde_json::json!(["00000000000000ab"])
        );
        // The rest of the envelope survives verbatim: a fork needs the
        // model and every other request-level field, not just the turns.
        assert_eq!(stored["request"]["model"], "claude-opus-5");
        assert_eq!(stored["span_id"], "a");
    }

    /// The documented opt-out: no hashes at all means no deduplication.
    #[test]
    fn no_hashes_stores_the_messages_inline() {
        let dir = tempfile::tempdir().expect("tempdir");
        let capture = Capture::open(dir.path().to_path_buf()).expect("open");
        capture
            .write(round(
                "a",
                vec![serde_json::json!({ "role": "user", "content": "one" })],
                Vec::new(),
            ))
            .expect("round");

        let path = &walkdir(&dir.path().join("rounds"))[0];
        let stored: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).expect("read")).expect("parse");
        assert_eq!(stored["request"]["messages"][0]["content"], "one");
        assert!(walkdir(&dir.path().join("messages")).is_empty());
    }

    /// The case the length guard exists for. Storing these by hash would
    /// pair each message with the wrong fingerprint, which is worse than
    /// not deduplicating at all: every later round that shares the prefix
    /// would then reference the wrong content.
    #[test]
    fn mismatched_hashes_keep_the_messages_inline() {
        let dir = tempfile::tempdir().expect("tempdir");
        let capture = Capture::open(dir.path().to_path_buf()).expect("open");
        capture
            .write(round(
                "a",
                vec![
                    serde_json::json!({ "role": "user", "content": "one" }),
                    serde_json::json!({ "role": "assistant", "content": "two" }),
                ],
                vec![1],
            ))
            .expect("round");

        let path = &walkdir(&dir.path().join("rounds"))[0];
        let stored: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).expect("read")).expect("parse");
        assert_eq!(stored["request"]["messages"][0]["content"], "one");
        assert_eq!(stored["request"]["messages"][1]["content"], "two");
        assert!(walkdir(&dir.path().join("messages")).is_empty());
    }

    fn walkdir(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    out.push(path);
                }
            }
        }
        out.sort();
        out
    }
}
