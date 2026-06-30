// Tier 2: LLM-as-judge via Ollama (phi4-mini local by default).
//
// Under privacy tier 1 (the default), span event content is null.
// faithfulness and hallucination_detection require LLM response text
// to evaluate and return None when content is absent.
// tool_selection operates on span operation names and tool call
// metadata which are always available regardless of privacy tier.
// A default installation therefore contributes one Tier 2 metric
// to the health score, not three. This is correct behaviour.
// Enable content capture (privacy tier 2 or higher) to unlock
// faithfulness and hallucination_detection.

use reeve_model::entity::span::InternalSpan;
use reeve_model::signal::EvaluationConfidence;
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;

const OLLAMA_ENDPOINT: &str = "http://localhost:11434";
const OLLAMA_MODEL: &str = "phi4-mini";
const MAX_RETRIES: u32 = 3;
const CONFIDENCE_HIGH_THRESHOLD: f64 = 0.10;
const CONFIDENCE_MEDIUM_THRESHOLD: f64 = 0.30;

#[derive(Debug, Clone)]
pub enum JudgeBackend {
    Local { endpoint: String, model: String },
    Disabled { reason: String },
}

pub struct LlmJudge {
    pub backend: JudgeBackend,
    client: Client,
}

#[derive(Deserialize)]
struct OllamaGenerateResponse {
    response: String,
}

#[derive(Debug, Clone, Copy)]
pub struct JudgeResult {
    pub score: f64,
    pub confidence: EvaluationConfidence,
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
    pub fn new(backend: JudgeBackend) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| Client::new());
        Self { backend, client }
    }

    /// Run all three Tier 2 evaluators against the completed trace spans.
    /// Returns `(metric_name, score, confidence)` for each metric that
    /// produced a result. Metrics requiring content return nothing under
    /// privacy tier 1 because span content is null.
    pub async fn evaluate_trace(
        &self,
        spans: &[InternalSpan],
    ) -> Vec<(&'static str, f64, EvaluationConfidence)> {
        let (endpoint, model) = match &self.backend {
            JudgeBackend::Local { endpoint, model } => (endpoint.as_str(), model.as_str()),
            JudgeBackend::Disabled { .. } => return vec![],
        };

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
                results.push(("tool_selection", r.score, r.confidence));
            }
        }

        if let Some(ref content) = extract_content(spans) {
            let context = extract_context(spans).unwrap_or_default();

            let faith_a = format!(
                "Does the following response use only information from the provided \
                 context? Score 0.0 if it introduces unsupported claims, 1.0 if \
                 fully grounded.\n\nContext: {}\n\nResponse: {}\n\n\
                 Return JSON: {{\"score\": <number>, \"reason\": \"<explanation>\"}}",
                context, content
            );
            let faith_b = format!(
                "Evaluate whether this response stays faithful to the given context. \
                 Score 0.0 if it fabricates information not in the context, 1.0 if \
                 entirely grounded.\n\nContext: {}\n\nResponse: {}\n\n\
                 Return JSON: {{\"score\": <number>, \"reason\": \"<explanation>\"}}",
                context, content
            );
            if let Some(r) = self
                .run_with_consistency(endpoint, model, &faith_a, &faith_b)
                .await
            {
                results.push(("faithfulness", r.score, r.confidence));
            }

            let hall_a = format!(
                "Does this response introduce claims not supported by the context? \
                 Score 0.0 if hallucinations are present, 1.0 if fully accurate.\n\n\
                 Context: {}\n\nResponse: {}\n\n\
                 Return JSON: {{\"score\": <number>, \"reason\": \"<explanation>\"}}",
                context, content
            );
            let hall_b = format!(
                "Identify any hallucinated content in this response not supported by \
                 the context. Score 0.0 if hallucinations are present, 1.0 if all \
                 claims are grounded.\n\nContext: {}\n\nResponse: {}\n\n\
                 Return JSON: {{\"score\": <number>, \"reason\": \"<explanation>\"}}",
                context, content
            );
            if let Some(r) = self
                .run_with_consistency(endpoint, model, &hall_a, &hall_b)
                .await
            {
                results.push(("hallucination_detection", r.score, r.confidence));
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
        let score_a = self.run_single(endpoint, model, prompt_a).await?;
        let score_b = self.run_single(endpoint, model, prompt_b).await?;
        let score = (score_a + score_b) / 2.0;
        let divergence = (score_a - score_b).abs();
        let confidence = if divergence < CONFIDENCE_HIGH_THRESHOLD {
            EvaluationConfidence::High
        } else if divergence < CONFIDENCE_MEDIUM_THRESHOLD {
            EvaluationConfidence::Medium
        } else {
            EvaluationConfidence::Low
        };
        Some(JudgeResult { score, confidence })
    }

    async fn run_single(&self, endpoint: &str, model: &str, prompt: &str) -> Option<f64> {
        for attempt in 0..MAX_RETRIES {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(2u64.pow(attempt - 1))).await;
            }
            match self.call_ollama(endpoint, model, prompt).await {
                Ok(score) => return Some(score),
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

    async fn call_ollama(&self, endpoint: &str, model: &str, prompt: &str) -> Result<f64, String> {
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
        parse_score(&ollama_resp.response)
    }
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
