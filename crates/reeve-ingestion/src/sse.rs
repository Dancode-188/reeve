//! Incremental parser for the Messages API's Server-Sent Event stream.
//! The proxy forwards chunks the moment they arrive; this accumulator
//! reads the same bytes on the side and reconstructs what the round trip
//! was: model, token usage, and the text generated so far.
//!
//! Chunks split events at arbitrary byte positions, so the accumulator
//! buffers until a complete `\n\n`-terminated event is available and
//! carries the remainder forward.

/// What one fed chunk revealed, for the caller to act on.
#[derive(Debug, Default, PartialEq)]
pub struct SseUpdate {
    /// New text deltas arrived; the accumulated content changed.
    pub content_changed: bool,
    /// The stream carried an error event: the upstream failed mid-stream.
    pub api_failed: bool,
    /// message_stop arrived: the stream completed normally.
    pub completed: bool,
}

#[derive(Debug, Default)]
pub struct SseAccumulator {
    buffer: String,
    pub model: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Reasoning tokens inside `output_tokens`, from
    /// `output_tokens_details.thinking_tokens`. Zero when the response
    /// carried none or the model does not report them.
    pub thinking_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub content: String,
    pub stop_reason: Option<String>,
    /// tool_use blocks the assistant opened: (id, name).
    pub tool_uses: Vec<(String, String)>,
    /// Edit types the API applied from `context_management.applied_edits`,
    /// e.g. "clear_thinking_20251015". Empty when no compaction happened.
    pub applied_edits: Vec<String>,
}

impl SseAccumulator {
    /// Feeds one chunk of raw SSE bytes and reports what changed. Invalid
    /// UTF-8 boundaries are tolerated by lossy conversion: token text is
    /// display data here, never returned to the client (the client gets
    /// the original bytes untouched).
    pub fn feed(&mut self, chunk: &[u8]) -> SseUpdate {
        self.buffer.push_str(&String::from_utf8_lossy(chunk));
        let mut update = SseUpdate::default();

        while let Some(pos) = self.buffer.find("\n\n") {
            let event: String = self.buffer.drain(..pos + 2).collect();
            self.process_event(&event, &mut update);
        }
        update
    }

    fn process_event(&mut self, event: &str, update: &mut SseUpdate) {
        let Some(data_line) = event
            .lines()
            .find_map(|l| l.strip_prefix("data:").map(str::trim))
        else {
            return;
        };
        let Ok(data) = serde_json::from_str::<serde_json::Value>(data_line) else {
            return;
        };

        match data.get("type").and_then(|t| t.as_str()) {
            Some("message_start") => {
                let msg = data.get("message");
                self.model = msg
                    .and_then(|m| m.get("model"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                if let Some(usage) = msg.and_then(|m| m.get("usage")) {
                    let get = |k: &str| usage.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
                    self.input_tokens = get("input_tokens");
                    self.cache_read_tokens = get("cache_read_input_tokens");
                    self.cache_creation_tokens = get("cache_creation_input_tokens");
                }
                // Compaction applies to the request before generation, so
                // the applied edits should ride the start event, but the
                // delta arm also checks: the placement is beta surface, not
                // settled convention.
                if let Some(msg) = msg {
                    self.collect_applied_edits(msg);
                }
            }
            Some("content_block_start") => {
                if let Some(block) = data
                    .get("content_block")
                    .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
                {
                    if let (Some(id), Some(name)) = (
                        block.get("id").and_then(|v| v.as_str()),
                        block.get("name").and_then(|v| v.as_str()),
                    ) {
                        self.tool_uses.push((id.to_string(), name.to_string()));
                    }
                }
            }
            Some("content_block_delta") => {
                if let Some(text) = data
                    .get("delta")
                    .filter(|d| d.get("type").and_then(|t| t.as_str()) == Some("text_delta"))
                    .and_then(|d| d.get("text"))
                    .and_then(|v| v.as_str())
                {
                    self.content.push_str(text);
                    update.content_changed = true;
                }
            }
            Some("message_delta") => {
                if let Some(usage) = data.get("usage") {
                    // Cumulative in the wire format: overwrite, don't add.
                    if let Some(out) = usage.get("output_tokens").and_then(|v| v.as_u64()) {
                        self.output_tokens = out;
                    }
                    if let Some(thinking) = usage
                        .get("output_tokens_details")
                        .and_then(|d| d.get("thinking_tokens"))
                        .and_then(|v| v.as_u64())
                    {
                        self.thinking_tokens = thinking;
                    }
                }
                if let Some(reason) = data
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(|v| v.as_str())
                {
                    self.stop_reason = Some(reason.to_string());
                }
                self.collect_applied_edits(&data);
            }
            Some("message_stop") => {
                update.completed = true;
            }
            Some("error") => {
                update.api_failed = true;
            }
            _ => {}
        }
    }

    /// Collects edit types out of a `context_management.applied_edits`
    /// array on the given value, if one is there. Overwrites rather than
    /// appends: the wire reports the full list, not deltas.
    fn collect_applied_edits(&mut self, value: &serde_json::Value) {
        if let Some(edits) = value
            .get("context_management")
            .and_then(|c| c.get("applied_edits"))
            .and_then(|e| e.as_array())
            .filter(|e| !e.is_empty())
        {
            self.applied_edits = edits
                .iter()
                .filter_map(|e| e.get("type").and_then(|t| t.as_str()))
                .map(str::to_string)
                .collect();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const START: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-opus-4-8\",\"usage\":{\"input_tokens\":900,\"cache_read_input_tokens\":100,\"cache_creation_input_tokens\":0}}}\n\n";

    #[test]
    fn accumulates_a_complete_stream() {
        let mut acc = SseAccumulator::default();
        acc.feed(START.as_bytes());
        acc.feed(b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n");
        acc.feed(b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\n");
        acc.feed(b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":42}}\n\n");
        let last = acc.feed(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");

        assert_eq!(acc.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(acc.input_tokens, 900);
        assert_eq!(acc.cache_read_tokens, 100);
        assert_eq!(acc.output_tokens, 42);
        assert_eq!(acc.content, "Hello world");
        assert_eq!(acc.stop_reason.as_deref(), Some("end_turn"));
        assert!(last.completed);
    }

    #[test]
    fn events_split_across_chunk_boundaries_reassemble() {
        let mut acc = SseAccumulator::default();
        let event = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"split\"}}\n\n";
        // Feed one byte at a time: the cruelest possible chunking.
        let mut changed = false;
        for b in event.as_bytes() {
            changed |= acc.feed(&[*b]).content_changed;
        }
        assert!(changed);
        assert_eq!(acc.content, "split");
    }

    #[test]
    fn error_event_reports_api_failure() {
        let mut acc = SseAccumulator::default();
        let update = acc.feed(b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"busy\"}}\n\n");
        assert!(update.api_failed);
    }

    #[test]
    fn thinking_tokens_are_read_from_output_details() {
        let mut acc = SseAccumulator::default();
        // The exact usage shape live traffic carries in message_delta.
        acc.feed(b"data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":131,\"output_tokens_details\":{\"thinking_tokens\":47}}}\n\n");
        assert_eq!(acc.output_tokens, 131);
        assert_eq!(acc.thinking_tokens, 47);
        // A delta without the details block leaves the count alone
        // rather than zeroing it.
        acc.feed(b"data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":150}}\n\n");
        assert_eq!(acc.thinking_tokens, 47);
    }

    #[test]
    fn output_tokens_overwrite_rather_than_add() {
        let mut acc = SseAccumulator::default();
        acc.feed(b"data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":10}}\n\n");
        acc.feed(b"data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":25}}\n\n");
        assert_eq!(acc.output_tokens, 25, "the wire count is cumulative");
    }

    #[test]
    fn tool_use_blocks_are_collected() {
        let mut acc = SseAccumulator::default();
        acc.feed(b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"bash\"}}\n\n");
        assert_eq!(
            acc.tool_uses,
            vec![("toolu_1".to_string(), "bash".to_string())]
        );
    }

    #[test]
    fn applied_edits_are_collected_from_either_event() {
        // On the start event, inside the message object.
        let mut acc = SseAccumulator::default();
        acc.feed(b"data: {\"type\":\"message_start\",\"message\":{\"model\":\"m\",\"context_management\":{\"applied_edits\":[{\"type\":\"clear_thinking_20251015\"}]}}}\n\n");
        assert_eq!(acc.applied_edits, vec!["clear_thinking_20251015"]);

        // On the delta event, at the top level.
        let mut acc = SseAccumulator::default();
        acc.feed(b"data: {\"type\":\"message_delta\",\"delta\":{},\"usage\":{\"output_tokens\":5},\"context_management\":{\"applied_edits\":[{\"type\":\"clear_tool_uses_20250919\"}]}}\n\n");
        assert_eq!(acc.applied_edits, vec!["clear_tool_uses_20250919"]);

        // The empty array live traffic carries today stays empty.
        let mut acc = SseAccumulator::default();
        acc.feed(b"data: {\"type\":\"message_delta\",\"delta\":{},\"usage\":{},\"context_management\":{\"applied_edits\":[]}}\n\n");
        assert!(acc.applied_edits.is_empty());
    }

    #[test]
    fn garbage_data_is_ignored_not_fatal() {
        let mut acc = SseAccumulator::default();
        let update = acc.feed(b"data: not json at all\n\nevent: ping\n\n");
        assert_eq!(update, SseUpdate::default());
        assert_eq!(acc.content, "");
    }
}
