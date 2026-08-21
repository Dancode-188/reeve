// Tier 2: LLM-as-judge via Ollama (phi4-mini local by default).
//
// faithfulness and hallucination_detection need the model's own words;
// tool_selection needs only operation names and tool metadata, so it
// scores at any privacy tier. Content arrives one of two ways. The SDK
// path puts it on the span, under the `gen_ai.*` keys `extract_content`
// looks for. The proxy path does not, and never did, which is why the
// two content metrics had returned nothing since this file was written
// no matter what tier the operator chose. ADR-0048 settled that by
// letting the judge read the capture store, so under tier 2 the reply
// comes off disk when the span does not carry one.
//
// Tier 1 still scores one metric of three, and now that is the tier
// gate doing its job rather than a gap nobody had noticed.

use reeve_model::entity::span::InternalSpan;
use reeve_model::signal::EvaluationConfidence;
use reeve_storage::capture::CaptureReader;
use reqwest::Client;
use serde::Deserialize;
use std::path::PathBuf;
use std::time::Duration;

const OLLAMA_ENDPOINT: &str = "http://localhost:11434";
const OLLAMA_MODEL: &str = "phi4-mini";
const MAX_RETRIES: u32 = 3;
const CONFIDENCE_HIGH_THRESHOLD: f64 = 0.10;
const CONFIDENCE_MEDIUM_THRESHOLD: f64 = 0.30;
/// How much of a captured round is allowed into a prompt.
///
/// The request below sets no `num_ctx`, so the model runs at whatever
/// Ollama defaults to, which is 4096 tokens on current builds. Roughly
/// four characters to a token, less the scaffolding and the room the
/// answer needs, leaves about twelve thousand characters to spend. A
/// captured round is far larger than that: the corpus puts the median
/// at 19 messages per round and the 90th percentile at 75, against
/// message files whose 90th percentile is 60 kB. Handing that over
/// whole does not evaluate more of the conversation, it just lets the
/// runtime decide silently which end to discard. Spending the budget
/// deliberately is the difference between a bounded prompt and a
/// truncated one.
///
/// Replies are measured at a median of 12 characters and a 90th
/// percentile of 2,377, so the smaller share still fits nearly all of
/// them intact.
const CAPTURE_CONTEXT_BUDGET: usize = 8_000;
const CAPTURE_REPLY_BUDGET: usize = 4_000;

#[derive(Debug, Clone)]
pub enum JudgeBackend {
    Local { endpoint: String, model: String },
    Disabled { reason: String },
}

pub struct LlmJudge {
    pub backend: JudgeBackend,
    client: Client,
    /// The capture directory, when the operator consented to tier 2.
    /// `None` leaves the judge exactly as it behaved before it had a
    /// reader, which is also what a missing round degrades to.
    capture_root: Option<PathBuf>,
}

#[derive(Deserialize)]
struct OllamaGenerateResponse {
    response: String,
}

#[derive(Debug, Clone)]
pub struct JudgeResult {
    pub score: f64,
    pub confidence: EvaluationConfidence,
    /// Both sides of the consistency pair, under keys `a` and `b`. Either
    /// may be null when that call fell through to a bare score.
    pub cot_json: Option<String>,
}

/// Probe for Ollama at the default endpoint. Returns the appropriate backend.
pub async fn probe() -> JudgeBackend {
    let client = Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap_or_else(|_| Client::new());
    let url = format!("{}/api/tags", OLLAMA_ENDPOINT);
    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(_) => {
            return JudgeBackend::Disabled {
                reason: "ollama not found".to_string(),
            };
        }
    };
    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => {
            return JudgeBackend::Disabled {
                reason: "ollama not found".to_string(),
            };
        }
    };
    let has_model = body
        .get("models")
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter().any(|m| {
                m.get("name")
                    .and_then(|n| n.as_str())
                    .map(|n| n == OLLAMA_MODEL || n.starts_with(&format!("{}:", OLLAMA_MODEL)))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);
    if has_model {
        JudgeBackend::Local {
            endpoint: OLLAMA_ENDPOINT.to_string(),
            model: OLLAMA_MODEL.to_string(),
        }
    } else {
        JudgeBackend::Disabled {
            reason: format!("run: ollama pull {}", OLLAMA_MODEL),
        }
    }
}

impl LlmJudge {
    pub fn new(backend: JudgeBackend, capture_root: Option<PathBuf>) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            backend,
            client,
            capture_root,
        }
    }

    /// The reply and its conversation, read off disk for the first span
    /// that has a round stored.
    ///
    /// Spans are tried in order and the first hit wins, which matches
    /// how `extract_content` already picks among them. A chat span off
    /// the proxy addresses its own round directly, for the reason set
    /// out in `reeve_storage::capture`; anything else finds no file and
    /// this returns nothing.
    ///
    /// The reads go to a blocking thread. Resolving a conversation is
    /// one small file plus up to a few dozen larger ones, which is more
    /// than an async runtime should be asked to sit through even on a
    /// detached task.
    async fn content_from_capture(
        &self,
        spans: &[InternalSpan],
    ) -> (Option<String>, Option<String>) {
        let Some(root) = self.capture_root.clone() else {
            return (None, None);
        };
        let keys: Vec<(i64, String)> = spans
            .iter()
            .map(|s| (s.start_time, s.id.to_string()))
            .collect();
        tokio::task::spawn_blocking(move || {
            let reader = CaptureReader::new(root);
            for (started_at_ms, span_id) in keys {
                let Some(round) = reader.round(started_at_ms, &span_id) else {
                    continue;
                };
                let Some(reply) = round.reply() else {
                    continue;
                };
                let context = reader.context(&round, CAPTURE_CONTEXT_BUDGET);
                return (Some(truncate(&reply, CAPTURE_REPLY_BUDGET)), context);
            }
            (None, None)
        })
        .await
        .unwrap_or((None, None))
    }

    /// Run all three Tier 2 evaluators against the completed trace spans.
    /// Returns `(metric_name, score, confidence, cot_json)` for each metric
    /// that produced a result. The two content metrics return nothing when
    /// neither the spans nor the capture store hold the reply, which is
    /// every trace under privacy tier 1. `cot_json` is Some whenever either
    /// half of the consistency pair came back with a justification.
    pub async fn evaluate_trace(
        &self,
        spans: &[InternalSpan],
    ) -> Vec<(&'static str, f64, EvaluationConfidence, Option<String>)> {
        let (endpoint, model) = match &self.backend {
            JudgeBackend::Local { endpoint, model } => (endpoint.as_str(), model.as_str()),
            JudgeBackend::Disabled { .. } => return vec![],
        };

        let cot_schema = r#"{"claims": ["<each factual claim in the response>"], "supported": ["<claims grounded in context>"], "unsupported": ["<claims not grounded in context>"], "score": <0.0-1.0>, "reason": "<explanation>"}"#;

        let mut results = Vec::new();

        let tool_calls = extract_tool_calls(spans);
        if !tool_calls.is_empty() {
            let list = tool_calls.join(", ");
            let prompt_a = format!(
                "Given this sequence of tool calls in order: [{}]. Score the \
                 appropriateness of tool selection and ordering from 0.0 (entirely \
                 wrong tools or sequence) to 1.0 (optimal). \
                 Return JSON: {{\"score\": <number>, \"reason\": \"<explanation>\"}}",
                list
            );
            let prompt_b = format!(
                "Review these tool invocations: [{}]. Assign a quality score where \
                 0.0 means completely inappropriate tool choice or ordering and 1.0 \
                 means ideal selection and sequence. \
                 Return JSON: {{\"score\": <number>, \"reason\": \"<explanation>\"}}",
                list
            );
            if let Some(r) = self
                .run_with_consistency(endpoint, model, &prompt_a, &prompt_b)
                .await
            {
                results.push(("tool_selection", r.score, r.confidence, r.cot_json));
            }
        }

        // Attributes first, capture second. The order matters: a span
        // that carries its own content is the SDK path describing
        // itself, and that beats reconstructing the same turn from a
        // file written by a different code path.
        let (content, context) = match extract_content(spans) {
            Some(c) => (Some(c), extract_context(spans)),
            None => self.content_from_capture(spans).await,
        };

        if let Some(ref content) = content {
            let context = context.unwrap_or_default();

            let faith_a = format!(
                "Does the following response use only information from the provided \
                 context? Score 0.0 if it introduces unsupported claims, 1.0 if \
                 fully grounded.\n\nContext: {}\n\nResponse: {}\n\n\
                 Return JSON: {}",
                context, content, cot_schema
            );
            let faith_b = format!(
                "Evaluate whether this response stays faithful to the given context. \
                 Score 0.0 if it fabricates information not in the context, 1.0 if \
                 entirely grounded.\n\nContext: {}\n\nResponse: {}\n\n\
                 Return JSON: {}",
                context, content, cot_schema
            );
            if let Some(r) = self
                .run_with_consistency(endpoint, model, &faith_a, &faith_b)
                .await
            {
                results.push(("faithfulness", r.score, r.confidence, r.cot_json));
            }

            let hall_a = format!(
                "Does this response introduce claims not supported by the context? \
                 Score 0.0 if hallucinations are present, 1.0 if fully accurate.\n\n\
                 Context: {}\n\nResponse: {}\n\n\
                 Return JSON: {}",
                context, content, cot_schema
            );
            let hall_b = format!(
                "Identify any hallucinated content in this response not supported by \
                 the context. Score 0.0 if hallucinations are present, 1.0 if all \
                 claims are grounded.\n\nContext: {}\n\nResponse: {}\n\n\
                 Return JSON: {}",
                context, content, cot_schema
            );
            if let Some(r) = self
                .run_with_consistency(endpoint, model, &hall_a, &hall_b)
                .await
            {
                results.push(("hallucination_detection", r.score, r.confidence, r.cot_json));
            }
        }

        results
    }

    async fn run_with_consistency(
        &self,
        endpoint: &str,
        model: &str,
        prompt_a: &str,
        prompt_b: &str,
    ) -> Option<JudgeResult> {
        let (score_a, cot_a) = self.run_single(endpoint, model, prompt_a).await?;
        let (score_b, cot_b) = self.run_single(endpoint, model, prompt_b).await?;
        let score = (score_a + score_b) / 2.0;
        let divergence = (score_a - score_b).abs();
        let confidence = if divergence < CONFIDENCE_HIGH_THRESHOLD {
            EvaluationConfidence::High
        } else if divergence < CONFIDENCE_MEDIUM_THRESHOLD {
            EvaluationConfidence::Medium
        } else {
            EvaluationConfidence::Low
        };
        // Both sides are kept. The prompts are two phrasings of one
        // question, so the pair of justifications carries something the
        // averaged score cannot: matching reasoning under different wording
        // means the model answered the shape of the request, not the turn.
        let cot_json = if cot_a.is_none() && cot_b.is_none() {
            None
        } else {
            Some(serde_json::json!({ "a": embed(cot_a), "b": embed(cot_b) }).to_string())
        };
        Some(JudgeResult {
            score,
            confidence,
            cot_json,
        })
    }

    async fn run_single(
        &self,
        endpoint: &str,
        model: &str,
        prompt: &str,
    ) -> Option<(f64, Option<String>)> {
        for attempt in 0..MAX_RETRIES {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(2u64.pow(attempt - 1))).await;
            }
            match self.call_ollama(endpoint, model, prompt).await {
                Ok(result) => return Some(result),
                Err(e) => {
                    tracing::debug!(attempt, error = %e, "ollama call failed");
                }
            }
        }
        tracing::warn!(
            "ollama eval exhausted {} retries, skipping metric",
            MAX_RETRIES
        );
        None
    }

    async fn call_ollama(
        &self,
        endpoint: &str,
        model: &str,
        prompt: &str,
    ) -> Result<(f64, Option<String>), String> {
        let body = serde_json::json!({
            "model": model,
            "prompt": prompt,
            "stream": false,
            "format": "json"
        });
        let resp = self
            .client
            .post(format!("{}/api/generate", endpoint))
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let ollama_resp: OllamaGenerateResponse = resp.json().await.map_err(|e| e.to_string())?;
        let text = &ollama_resp.response;
        if let Some((score, cot)) = parse_cot(text) {
            return Ok((score, Some(cot)));
        }
        parse_score(text).map(|s| (s, None))
    }
}

/// Re-parses a stored fragment so a pair embeds as JSON rather than as two
/// escaped strings.
fn embed(cot: Option<String>) -> serde_json::Value {
    cot.and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(serde_json::Value::Null)
}

fn parse_cot(text: &str) -> Option<(f64, String)> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let score = v.get("score")?.as_f64()?.clamp(0.0, 1.0);
    let reason = v.get("reason").cloned().unwrap_or(serde_json::Value::Null);
    if v.get("claims").is_none() && v.get("supported").is_none() && v.get("unsupported").is_none() {
        // The tool_selection prompts ask for score and reason only, so a
        // response with no claim arrays is well formed rather than broken.
        if reason.is_null() {
            return None;
        }
        return Some((score, serde_json::json!({ "reason": reason }).to_string()));
    }
    let cot = serde_json::json!({
        "claims": v.get("claims").cloned().unwrap_or(serde_json::json!([])),
        "supported": v.get("supported").cloned().unwrap_or(serde_json::json!([])),
        "unsupported": v.get("unsupported").cloned().unwrap_or(serde_json::json!([])),
        "reason": reason,
    });
    Some((score, cot.to_string()))
}

fn parse_score(text: &str) -> Result<f64, String> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
        if let Some(score) = v.get("score").and_then(|s| s.as_f64()) {
            return Ok(score.clamp(0.0, 1.0));
        }
    }
    // Fallback: scan for "score": <number> when the JSON is malformed.
    let lower = text.to_lowercase();
    if let Some(idx) = lower.find("\"score\"") {
        let after = lower[idx + 7..].trim_start_matches([' ', ':', '\n', '\r', '\t']);
        let end = after
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(after.len());
        if let Ok(v) = after[..end].parse::<f64>() {
            return Ok(v.clamp(0.0, 1.0));
        }
    }
    Err(format!(
        "could not parse score: {}",
        text.chars().take(120).collect::<String>()
    ))
}

fn extract_tool_calls(spans: &[InternalSpan]) -> Vec<String> {
    spans
        .iter()
        .filter_map(|s| {
            if let Some(name) = s
                .attributes
                .get("gen_ai.tool.name")
                .and_then(|v| v.as_str())
                .filter(|n| !n.is_empty())
            {
                return Some(name.to_string());
            }
            if s.operation.contains("tool") || s.operation.starts_with("gen_ai.execute") {
                Some(s.operation.clone())
            } else {
                None
            }
        })
        .collect()
}

/// Cuts to a character budget on a char boundary, marking the cut so a
/// judge scoring a half-sentence can see why it is one.
fn truncate(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_string();
    }
    let kept: String = text.chars().take(budget).collect();
    format!("{kept}\n[truncated]")
}

fn extract_content(spans: &[InternalSpan]) -> Option<String> {
    for s in spans {
        for key in &[
            "gen_ai.assistant.message.content",
            "gen_ai.output.content",
            "gen_ai.completion",
        ] {
            if let Some(text) = s
                .attributes
                .get(*key)
                .and_then(|v| v.as_str())
                .filter(|t| !t.is_empty())
            {
                return Some(text.to_string());
            }
        }
    }
    None
}

fn extract_context(spans: &[InternalSpan]) -> Option<String> {
    for s in spans {
        for key in &[
            "gen_ai.retrieval.content",
            "gen_ai.input.context",
            "gen_ai.prompt",
        ] {
            if let Some(text) = s
                .attributes
                .get(*key)
                .and_then(|v| v.as_str())
                .filter(|t| !t.is_empty())
            {
                return Some(text.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use reeve_model::entity::span::SpanStatus;
    use std::collections::HashMap;

    fn make_span(op: &str, attrs: serde_json::Value) -> InternalSpan {
        InternalSpan {
            id: op.into(),
            trace_id: "t1".into(),
            parent_id: None,
            operation: op.to_string(),
            status: SpanStatus::Completed,
            start_time: 0,
            end_time: Some(1000),
            arrived_at: 0,
            attributes: attrs,
            raw_attributes: HashMap::new(),
        }
    }

    #[test]
    fn parse_cot_extracts_structured_response() {
        let json = r#"{"claims":["sky is blue"],"supported":["sky is blue"],"unsupported":[],"score":0.9,"reason":"ok"}"#;
        let (score, cot) = parse_cot(json).unwrap();
        assert!((score - 0.9).abs() < 0.001);
        let v: serde_json::Value = serde_json::from_str(&cot).unwrap();
        assert_eq!(v["claims"][0], "sky is blue");
        assert!(v["unsupported"].as_array().unwrap().is_empty());
    }

    #[test]
    fn parse_cot_keeps_reason_from_flat_format() {
        let json = r#"{"score": 0.8, "reason": "looks fine"}"#;
        let (score, cot) = parse_cot(json).unwrap();
        assert!((score - 0.8).abs() < 0.001);
        let v: serde_json::Value = serde_json::from_str(&cot).unwrap();
        assert_eq!(v["reason"], "looks fine");
    }

    #[test]
    fn parse_cot_returns_none_when_nothing_to_keep() {
        let json = r#"{"score": 0.8}"#;
        assert!(parse_cot(json).is_none());
    }

    #[test]
    fn parse_cot_clamps_score() {
        let json = r#"{"claims":["x"],"supported":["x"],"unsupported":[],"score":1.5}"#;
        let (score, _) = parse_cot(json).unwrap();
        assert!((score - 1.0).abs() < 0.001);
    }

    #[test]
    fn parse_cot_strips_extra_fields() {
        let json = r#"{"claims":["a"],"supported":["a"],"unsupported":[],"score":0.7,"reason":"r","extra":"ignored"}"#;
        let (_, cot) = parse_cot(json).unwrap();
        let v: serde_json::Value = serde_json::from_str(&cot).unwrap();
        assert!(v.get("extra").is_none());
        assert_eq!(v["reason"], "r");
    }

    #[test]
    fn embed_pairs_both_sides() {
        let a = Some(r#"{"reason":"first"}"#.to_string());
        let paired = serde_json::json!({ "a": embed(a), "b": embed(None) });
        assert_eq!(paired["a"]["reason"], "first");
        assert!(paired["b"].is_null());
    }

    #[test]
    fn parse_score_valid_json() {
        let r = parse_score(r#"{"score": 0.85, "reason": "good"}"#);
        assert!((r.unwrap() - 0.85).abs() < 0.001);
    }

    #[test]
    fn parse_score_clamps_above_one() {
        let r = parse_score(r#"{"score": 1.5, "reason": "too high"}"#);
        assert!((r.unwrap() - 1.0).abs() < 0.001);
    }

    #[test]
    fn parse_score_clamps_below_zero() {
        let r = parse_score(r#"{"score": -0.2, "reason": "negative"}"#);
        assert!(r.unwrap().abs() < 0.001);
    }

    #[test]
    fn parse_score_fallback_from_prose() {
        let r = parse_score(r#"My assessment: "score": 0.70 based on the evidence"#);
        assert!((r.unwrap() - 0.70).abs() < 0.001);
    }

    #[test]
    fn parse_score_no_score_returns_err() {
        let r = parse_score("I cannot evaluate this trace.");
        assert!(r.is_err());
    }

    #[test]
    fn confidence_high_when_scores_agree() {
        let divergence = 0.05_f64;
        let c = if divergence < CONFIDENCE_HIGH_THRESHOLD {
            EvaluationConfidence::High
        } else if divergence < CONFIDENCE_MEDIUM_THRESHOLD {
            EvaluationConfidence::Medium
        } else {
            EvaluationConfidence::Low
        };
        assert_eq!(c, EvaluationConfidence::High);
    }

    #[test]
    fn confidence_medium_between_thresholds() {
        let divergence = 0.20_f64;
        let c = if divergence < CONFIDENCE_HIGH_THRESHOLD {
            EvaluationConfidence::High
        } else if divergence < CONFIDENCE_MEDIUM_THRESHOLD {
            EvaluationConfidence::Medium
        } else {
            EvaluationConfidence::Low
        };
        assert_eq!(c, EvaluationConfidence::Medium);
    }

    #[test]
    fn confidence_low_when_scores_diverge() {
        let divergence = 0.35_f64;
        let c = if divergence < CONFIDENCE_HIGH_THRESHOLD {
            EvaluationConfidence::High
        } else if divergence < CONFIDENCE_MEDIUM_THRESHOLD {
            EvaluationConfidence::Medium
        } else {
            EvaluationConfidence::Low
        };
        assert_eq!(c, EvaluationConfidence::Low);
    }

    #[test]
    fn confidence_boundary_at_high_threshold_is_medium() {
        let divergence = CONFIDENCE_HIGH_THRESHOLD;
        let c = if divergence < CONFIDENCE_HIGH_THRESHOLD {
            EvaluationConfidence::High
        } else if divergence < CONFIDENCE_MEDIUM_THRESHOLD {
            EvaluationConfidence::Medium
        } else {
            EvaluationConfidence::Low
        };
        assert_eq!(c, EvaluationConfidence::Medium);
    }

    #[test]
    fn confidence_boundary_at_medium_threshold_is_low() {
        let divergence = CONFIDENCE_MEDIUM_THRESHOLD;
        let c = if divergence < CONFIDENCE_HIGH_THRESHOLD {
            EvaluationConfidence::High
        } else if divergence < CONFIDENCE_MEDIUM_THRESHOLD {
            EvaluationConfidence::Medium
        } else {
            EvaluationConfidence::Low
        };
        assert_eq!(c, EvaluationConfidence::Low);
    }

    #[test]
    fn extract_tool_calls_finds_gen_ai_tool_name() {
        let span = make_span(
            "gen_ai.tool.call",
            serde_json::json!({"gen_ai.tool.name": "search"}),
        );
        let calls = extract_tool_calls(&[span]);
        assert_eq!(calls, vec!["search"]);
    }

    #[test]
    fn extract_tool_calls_falls_back_to_operation_name() {
        let span = make_span("tool.bash", serde_json::Value::Null);
        let calls = extract_tool_calls(&[span]);
        assert_eq!(calls, vec!["tool.bash"]);
    }

    #[test]
    fn extract_tool_calls_skips_non_tool_spans() {
        let span = make_span("gen_ai.chat", serde_json::Value::Null);
        let calls = extract_tool_calls(&[span]);
        assert!(calls.is_empty());
    }

    fn span_at(id: &str, start_time: i64) -> InternalSpan {
        let mut span = make_span("gen_ai.chat", serde_json::json!({}));
        span.id = id.into();
        span.start_time = start_time;
        span
    }

    fn disabled_judge(capture_root: Option<PathBuf>) -> LlmJudge {
        LlmJudge::new(
            JudgeBackend::Disabled {
                reason: "test".into(),
            },
            capture_root,
        )
    }

    fn store_round(root: &std::path::Path, start: i64, span: &str, round: serde_json::Value) {
        let path = reeve_storage::capture::round_path(root, start, span);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, serde_json::to_vec(&round).expect("encode")).expect("write");
    }

    #[tokio::test]
    async fn tier_one_has_no_capture_to_fall_back_on() {
        let judge = disabled_judge(None);
        assert_eq!(
            judge.content_from_capture(&[span_at("abc", 1000)]).await,
            (None, None)
        );
    }

    #[tokio::test]
    async fn a_span_without_content_finds_its_round_on_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        store_round(
            dir.path(),
            1787225395662,
            "609f06dfe14f78d4",
            serde_json::json!({
                "request": {"messages": [{"role": "user", "content": "why is CI red"}]},
                "response": {"content": "a flaky mirror held the run open"},
            }),
        );
        let judge = disabled_judge(Some(dir.path().to_path_buf()));
        let spans = [
            span_at("a-root-span", 1787225395000),
            span_at("609f06dfe14f78d4", 1787225395662),
        ];
        let (content, context) = judge.content_from_capture(&spans).await;
        assert_eq!(content.as_deref(), Some("a flaky mirror held the run open"));
        assert_eq!(context.as_deref(), Some("user: why is CI red"));
    }

    #[tokio::test]
    async fn a_span_off_the_proxy_path_finds_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let judge = disabled_judge(Some(dir.path().to_path_buf()));
        assert_eq!(
            judge.content_from_capture(&[span_at("sdk-span", 42)]).await,
            (None, None)
        );
    }

    #[test]
    fn truncate_marks_where_it_cut() {
        assert_eq!(truncate("short", 100), "short");
        assert_eq!(truncate("abcdefgh", 3), "abc\n[truncated]");
    }

    #[test]
    fn truncate_does_not_split_a_character() {
        // Budgets are in bytes but the cut counts chars, so a multi-byte
        // reply is shortened rather than made invalid.
        let text = "\u{00e9}\u{00e9}\u{00e9}\u{00e9}";
        assert_eq!(truncate(text, 3), "\u{00e9}\u{00e9}\u{00e9}\n[truncated]");
    }

    #[test]
    fn extract_content_returns_none_when_absent() {
        let span = make_span("gen_ai.completion", serde_json::json!({}));
        assert!(extract_content(&[span]).is_none());
    }

    #[test]
    fn extract_content_finds_assistant_message() {
        let span = make_span(
            "gen_ai.chat",
            serde_json::json!({"gen_ai.assistant.message.content": "hello world"}),
        );
        assert_eq!(extract_content(&[span]), Some("hello world".to_string()));
    }
}
