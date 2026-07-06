pub mod evaluation;
pub mod policy;

use evaluation::TraceContext;
use evaluation::fingerprint::AgentFingerprint;
use evaluation::health_score;
use evaluation::heuristic::{
    CostEfficiencyEvaluator, Evaluator, FingerprintDeviationEvaluator,
    IntentActionDivergenceEvaluator, LatencyNormalityEvaluator, LoopDetector,
};
use evaluation::llm_judge::{self, LlmJudge};
use policy::dsl::PolicyContext;
use policy::{PolicyEngine, alert_fields};
use reeve_model::entity::evaluation::{EvaluationResult, EvaluatorType, TargetType};
use reeve_model::entity::intervention::InterventionCommand;
use reeve_model::entity::span::InternalSpan;
use reeve_model::ids::{AgentId, EvalId, RuleId, TraceId};
use reeve_model::signal::{EngineEvent, EvaluationConfidence, IngestionEvent};
use reeve_storage::warm::WarmStore;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, mpsc};

pub type DispatchSender = mpsc::Sender<(AgentId, InterventionCommand)>;

pub async fn run(
    mut ingestion_rx: broadcast::Receiver<IngestionEvent>,
    engine_tx: broadcast::Sender<EngineEvent>,
    warm: Arc<WarmStore>,
    dispatch_tx: Option<DispatchSender>,
) {
    let backend = llm_judge::probe().await;
    let (backend_name, backend_reason) = match &backend {
        llm_judge::JudgeBackend::Local { model, .. } => (format!("local ({})", model), None),
        llm_judge::JudgeBackend::Disabled { reason } => {
            ("disabled".to_string(), Some(reason.clone()))
        }
    };
    tracing::info!(backend = %backend_name, "evaluation backend ready");
    let _ = engine_tx.send(EngineEvent::EvaluationBackendReady {
        backend: backend_name,
        reason: backend_reason,
        privacy_tier: 1,
    });
    let judge = Arc::new(LlmJudge::new(backend));

    let config_path = std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".config/reeve/config.toml"))
        .unwrap_or_else(|_| PathBuf::from(".config/reeve/config.toml"));

    let mut fingerprints: HashMap<AgentId, AgentFingerprint> = HashMap::new();
    let mut score_histories: HashMap<AgentId, VecDeque<f64>> = HashMap::new();
    let mut cost_accumulators: HashMap<TraceId, CostAccumulator> = HashMap::new();
    let mut trace_agents: HashMap<TraceId, AgentId> = HashMap::new();
    let mut policy_engine = PolicyEngine::with_defaults();

    {
        let db_rules = warm.load_policy_rules().await.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "failed to load policy rules from database");
            vec![]
        });
        let cfg_rules = policy::config::load(&config_path);
        let mut combined = db_rules;
        combined.extend(cfg_rules);
        policy_engine.replace_user_rules(combined);
    }

    {
        let startup_ms = current_ms();
        let cooldowns = warm
            .load_active_policy_cooldowns(startup_ms)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "failed to load cooldown state from database");
                vec![]
            });
        if !cooldowns.is_empty() {
            tracing::info!(count = cooldowns.len(), "restored active policy cooldowns");
        }
        policy_engine.load_cooldowns(&cooldowns, startup_ms);
    }

    // SIGUSR1 triggers a policy rule reload. SIGHUP deliberately keeps its
    // default disposition (terminate): for a terminal app, hangup means the
    // terminal went away. An earlier SIGHUP-based reload made Reeve swallow
    // its own hangup and survive every terminal close, holding both ports.
    let mut reload_signal =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())
            .expect("failed to install SIGUSR1 handler");

    let evaluators: Vec<Box<dyn Evaluator>> = vec![
        Box::new(LoopDetector::new(3)),
        Box::new(CostEfficiencyEvaluator),
        Box::new(LatencyNormalityEvaluator),
        Box::new(IntentActionDivergenceEvaluator),
        Box::new(FingerprintDeviationEvaluator),
    ];

    loop {
        let event = tokio::select! {
            _ = reload_signal.recv() => {
                let db_rules = warm.load_policy_rules().await.unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "failed to reload policy rules from database");
                    vec![]
                });
                let cfg_rules = policy::config::load(&config_path);
                let mut combined = db_rules;
                combined.extend(cfg_rules);
                policy_engine.replace_user_rules(combined);
                continue;
            }
            ev = ingestion_rx.recv() => ev,
        };
        match event {
            Ok(IngestionEvent::TraceCompleted {
                trace_id,
                agent_id,
                span_count,
                cost,
            }) => {
                let spans = warm
                    .list_spans_for_trace(&trace_id)
                    .await
                    .unwrap_or_else(|e| {
                        tracing::warn!(
                            trace_id = %trace_id,
                            error = %e,
                            "engine failed to load spans for evaluation"
                        );
                        vec![]
                    });

                let min_start = spans.iter().map(|s| s.start_time).min();
                let max_end = spans.iter().filter_map(|s| s.end_time).max();
                let duration_secs = match (min_start, max_end) {
                    (Some(s), Some(e)) => e.saturating_sub(s).max(0) as f64 / 1e9,
                    _ => 0.0,
                };

                let fp = fingerprints.get(&agent_id);

                let ctx = TraceContext {
                    trace_id: trace_id.clone(),
                    agent_id: agent_id.clone(),
                    span_count,
                    cost,
                    spans: &spans,
                    fingerprint: fp,
                };

                let mut metric_scores: HashMap<&str, f64> = HashMap::new();

                for evaluator in &evaluators {
                    if let Some(score) = evaluator.evaluate(&ctx) {
                        let _ = engine_tx.send(EngineEvent::EvaluationComplete {
                            trace_id: trace_id.clone(),
                            span_id: None,
                            metric: evaluator.name().to_string(),
                            score,
                            confidence: None,
                        });
                        metric_scores.insert(evaluator.name(), score);
                    }
                }

                let mut tier1_health: Option<f64> = None;

                if let Some(hs) = health_score::compute(&metric_scores) {
                    tier1_health = Some(hs.value);
                    let event = EngineEvent::HealthScoreUpdated {
                        agent_id: agent_id.clone(),
                        trace_id: trace_id.clone(),
                        score: hs.value,
                        tier2_pending: hs.tier2_pending,
                        weight_coverage: hs.weight_coverage,
                    };
                    if engine_tx.send(event).is_err() {
                        tracing::debug!("no engine event subscribers");
                    }

                    if let Err(e) = warm.update_trace_health_score(&trace_id, hs.value).await {
                        tracing::warn!(
                            trace_id = %trace_id,
                            error = %e,
                            "failed to persist health score"
                        );
                    }

                    // Policy evaluation runs on Tier 1 results. Tier 2 does not
                    // re-trigger to avoid double-firing on the same trace.
                    let now_ms = current_ms();
                    let policy_ctx = PolicyContext::build(
                        hs.value,
                        cost,
                        span_count,
                        hs.tier2_pending,
                        hs.weight_coverage,
                        0.0,
                        &metric_scores,
                    );
                    let fired = policy_engine.evaluate(
                        &agent_id,
                        &trace_id,
                        &policy_ctx,
                        Instant::now(),
                        now_ms,
                    );
                    for fr in fired {
                        let (
                            rule_id_str,
                            description,
                            cmd_type,
                            requires_confirmation,
                            auto_confirm_after_secs,
                        ) = alert_fields(&fr);
                        let rule_id_owned = rule_id_str.to_string();
                        let _ = engine_tx.send(EngineEvent::PolicyAlert {
                            rule_id: rule_id_owned.clone(),
                            description: description.to_string(),
                            command_type: cmd_type.to_string(),
                            requires_confirmation,
                            auto_confirm_after_secs,
                        });
                        persist_cooldown(
                            &warm,
                            &agent_id,
                            &fr.rule.id,
                            now_ms,
                            fr.rule.cooldown_secs,
                        )
                        .await;
                        dispatch_or_save(
                            &dispatch_tx,
                            &warm,
                            &agent_id,
                            fr.command,
                            requires_confirmation,
                            &rule_id_owned,
                        )
                        .await;
                    }
                }

                fingerprints.entry(agent_id.clone()).or_default().update(
                    span_count,
                    cost,
                    duration_secs,
                );

                // Update per-agent health score history and compute Tier 2 rate.
                let history = score_histories.entry(agent_id.clone()).or_default();
                if let Some(score) = tier1_health {
                    history.push_back(score);
                    if history.len() > 5 {
                        history.pop_front();
                    }
                }
                let rate = tier2_sample_rate(history);

                // Tier 2 runs asynchronously after Tier 1 completes. Tier 1
                // always runs; only the Tier 2 spawn is gated by the sample rate.
                let tier1_scores: HashMap<String, f64> = metric_scores
                    .iter()
                    .map(|(k, v)| (k.to_string(), *v))
                    .collect();
                if rand::random::<f64>() < rate {
                    tokio::spawn(run_tier2(
                        trace_id.clone(),
                        agent_id.clone(),
                        spans,
                        tier1_scores,
                        engine_tx.clone(),
                        warm.clone(),
                        judge.clone(),
                    ));
                }

                cost_accumulators.remove(&trace_id);
                trace_agents.remove(&trace_id);
            }
            Ok(IngestionEvent::SpanCompleted { trace_id, span_id }) => {
                let span = match warm.get_span(&span_id).await {
                    Ok(Some(s)) => s,
                    _ => continue,
                };

                let input_tokens = span
                    .attributes
                    .get("gen_ai.usage.input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let output_tokens = span
                    .attributes
                    .get("gen_ai.usage.output_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let model = span
                    .attributes
                    .get("gen_ai.request.model")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let span_cost = span_cost_usd(input_tokens, output_tokens, model);

                if span_cost == 0.0 {
                    continue;
                }

                let agent_id = match trace_agents.get(&trace_id) {
                    Some(a) => a.clone(),
                    None => match warm.get_trace(&trace_id).await {
                        Ok(Some(t)) => {
                            let id = t.agent_id.clone();
                            trace_agents.insert(trace_id.clone(), id.clone());
                            id
                        }
                        _ => continue,
                    },
                };

                let now_ms = current_ms();
                let acc = cost_accumulators
                    .entry(trace_id.clone())
                    .or_insert_with(|| CostAccumulator {
                        started_at_ms: now_ms,
                        current_cost: 0.0,
                        samples: VecDeque::new(),
                    });

                acc.current_cost += span_cost;
                acc.samples.push_back((acc.current_cost, now_ms));
                if acc.samples.len() > 5 {
                    acc.samples.pop_front();
                }

                if acc.samples.len() < 2 {
                    continue;
                }

                let (old_cost, old_ts) = *acc.samples.front().unwrap();
                let window_secs = (now_ms - old_ts).max(1) as f64 / 1000.0;
                let rate = (acc.current_cost - old_cost) / window_secs;

                let elapsed_total = (now_ms - acc.started_at_ms).max(0) as f64 / 1000.0;
                let avg_duration = fingerprints
                    .get(&agent_id)
                    .map(|fp| fp.avg_duration_secs)
                    .unwrap_or(30.0);
                let remaining = (avg_duration - elapsed_total).max(0.0);
                let predicted = acc.current_cost + rate * remaining;

                if predicted <= 0.0 {
                    continue;
                }

                let mid_fired = policy_engine.evaluate_mid_trace(
                    &agent_id,
                    &trace_id,
                    predicted,
                    Instant::now(),
                    now_ms,
                );
                for fr in mid_fired {
                    let (
                        rule_id_str,
                        description,
                        cmd_type,
                        requires_confirmation,
                        auto_confirm_after_secs,
                    ) = alert_fields(&fr);
                    let rule_id_owned = rule_id_str.to_string();
                    let _ = engine_tx.send(EngineEvent::PolicyAlert {
                        rule_id: rule_id_owned.clone(),
                        description: description.to_string(),
                        command_type: cmd_type.to_string(),
                        requires_confirmation,
                        auto_confirm_after_secs,
                    });
                    persist_cooldown(&warm, &agent_id, &fr.rule.id, now_ms, fr.rule.cooldown_secs)
                        .await;
                    dispatch_or_save(
                        &dispatch_tx,
                        &warm,
                        &agent_id,
                        fr.command,
                        requires_confirmation,
                        &rule_id_owned,
                    )
                    .await;
                }
            }
            Ok(_) => {}
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(missed = n, "engine lagged behind ingestion channel");
            }
            Err(broadcast::error::RecvError::Closed) => {
                tracing::info!("ingestion channel closed, engine shutting down");
                break;
            }
        }
    }
}

async fn persist_cooldown(
    warm: &WarmStore,
    agent_id: &AgentId,
    rule_id: &RuleId,
    now_ms: i64,
    cooldown_secs: u64,
) {
    if let Err(e) = warm
        .save_policy_cooldown(agent_id, rule_id, now_ms, cooldown_secs)
        .await
    {
        tracing::warn!(rule_id = %rule_id, error = %e, "failed to persist cooldown");
    }
}

async fn dispatch_or_save(
    dispatch_tx: &Option<DispatchSender>,
    warm: &WarmStore,
    agent_id: &AgentId,
    command: InterventionCommand,
    requires_confirmation: bool,
    rule_id: &str,
) {
    if !requires_confirmation {
        if let Some(tx) = dispatch_tx {
            if tx.send((agent_id.clone(), command)).await.is_err() {
                tracing::warn!(rule_id, "dispatch channel closed; command dropped");
            }
        }
    } else if let Err(e) = warm.save_intervention_command(command).await {
        tracing::warn!(rule_id, error = %e, "failed to persist intervention command");
    }
}

fn current_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// Returns the Tier 2 sampling rate for an agent based on recent health scores.
///
/// Scores below 60 warrant full coverage; scores above 80 with no downward
/// spike in the last 5 traces can be sampled lightly. Everything else gets
/// the 20% default.
struct CostAccumulator {
    started_at_ms: i64,
    current_cost: f64,
    // (cumulative_cost_usd, timestamp_ms) for the last 5 cost-incurring spans
    samples: VecDeque<(f64, i64)>,
}

/// Approximate cost in USD for a single LLM span. Returns 0.0 for local or
/// unrecognised models. Used for prediction only, not billing.
///
/// USD per 1M tokens. Check more specific prefixes before broader ones.
fn span_cost_usd(input_tokens: u64, output_tokens: u64, model: &str) -> f64 {
    let (input_rate, output_rate): (f64, f64) = if model.starts_with("claude-opus") {
        (15.0, 75.0)
    } else if model.starts_with("claude-sonnet") || model.starts_with("claude-fable") {
        (3.0, 15.0)
    } else if model.starts_with("claude-haiku") {
        (0.8, 4.0)
    } else if model.starts_with("gpt-4.1") {
        (2.0, 8.0)
    } else if model.starts_with("gpt-4o-mini") {
        (0.15, 0.60)
    } else if model.starts_with("gpt-4o") {
        (2.5, 10.0)
    } else if model.contains("flash") {
        (0.075, 0.30)
    } else if model.starts_with("gemini-2.5")
        || model.starts_with("gemini-2.0")
        || model.starts_with("gemini-1.5")
    {
        (1.0, 4.0)
    } else {
        return 0.0;
    };
    (input_tokens as f64 * input_rate + output_tokens as f64 * output_rate) / 1_000_000.0
}

fn tier2_sample_rate(history: &VecDeque<f64>) -> f64 {
    let latest = match history.back() {
        Some(&s) => s,
        None => return 0.20,
    };
    if latest < 60.0 {
        return 1.0;
    }
    if latest > 80.0 && is_score_stable(history) {
        return 0.10;
    }
    0.20
}

/// Returns true when no consecutive pair in `history` shows a downward delta
/// greater than 5 points.
fn is_score_stable(history: &VecDeque<f64>) -> bool {
    history
        .iter()
        .zip(history.iter().skip(1))
        .all(|(prev, curr)| prev - curr <= 5.0)
}

async fn run_tier2(
    trace_id: reeve_model::ids::TraceId,
    agent_id: AgentId,
    spans: Vec<InternalSpan>,
    tier1_scores: HashMap<String, f64>,
    engine_tx: broadcast::Sender<EngineEvent>,
    warm: Arc<WarmStore>,
    judge: Arc<LlmJudge>,
) {
    let results = judge.evaluate_trace(&spans).await;

    let model_version = match &judge.backend {
        llm_judge::JudgeBackend::Local { model, .. } => Some(model.clone()),
        llm_judge::JudgeBackend::Disabled { .. } => None,
    };
    let now = current_ms();

    for (metric, score, confidence, cot_json) in &results {
        let _ = engine_tx.send(EngineEvent::EvaluationComplete {
            trace_id: trace_id.clone(),
            span_id: None,
            metric: metric.to_string(),
            score: *score,
            confidence: Some(*confidence),
        });
        let eval = EvaluationResult {
            id: EvalId::from(format!("{}-{}", trace_id, metric)),
            target_id: trace_id.to_string(),
            target_type: TargetType::Trace,
            metric: metric.to_string(),
            score: *score,
            evaluator: EvaluatorType::LlmJudge,
            evaluated_at: now,
            judge_model_version: model_version.clone(),
            cot_json: cot_json.clone(),
        };
        if let Err(e) = warm.save_evaluation_result(eval).await {
            tracing::warn!(error = %e, metric, "failed to persist tier2 evaluation");
        }
    }

    // Merge Tier 1 scores with non-Low-confidence Tier 2 scores before
    // recomputing. Low-confidence results are still emitted above so the
    // policy engine and renderer can act on them, but they do not affect
    // the health score value.
    let mut all_scores: HashMap<&str, f64> =
        tier1_scores.iter().map(|(k, v)| (k.as_str(), *v)).collect();
    for (metric, score, confidence, _) in &results {
        if *confidence != EvaluationConfidence::Low {
            all_scores.insert(metric, *score);
        }
    }

    if let Some(hs) = health_score::compute(&all_scores) {
        let event = EngineEvent::HealthScoreUpdated {
            agent_id,
            trace_id: trace_id.clone(),
            score: hs.value,
            tier2_pending: false,
            weight_coverage: hs.weight_coverage,
        };
        let _ = engine_tx.send(event);
        if let Err(e) = warm.update_trace_health_score(&trace_id, hs.value).await {
            tracing::warn!(
                trace_id = %trace_id,
                error = %e,
                "failed to persist tier2 health score"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history(scores: &[f64]) -> VecDeque<f64> {
        scores.iter().copied().collect()
    }

    #[test]
    fn rate_is_full_when_score_below_60() {
        let h = history(&[45.0]);
        assert!((tier2_sample_rate(&h) - 1.0).abs() < 0.001);
    }

    #[test]
    fn rate_is_default_when_no_history() {
        let h = VecDeque::new();
        assert!((tier2_sample_rate(&h) - 0.20).abs() < 0.001);
    }

    #[test]
    fn rate_is_low_when_stable_above_80() {
        let h = history(&[82.0, 83.0, 84.0, 85.0, 86.0]);
        assert!((tier2_sample_rate(&h) - 0.10).abs() < 0.001);
    }

    #[test]
    fn rate_is_default_when_above_80_but_unstable() {
        // Drop from 90 to 84 is a delta of 6 — unstable.
        let h = history(&[82.0, 90.0, 84.0, 85.0, 86.0]);
        assert!((tier2_sample_rate(&h) - 0.20).abs() < 0.001);
    }

    #[test]
    fn rate_is_default_for_mid_range_stable_scores() {
        let h = history(&[70.0, 71.0, 72.0]);
        assert!((tier2_sample_rate(&h) - 0.20).abs() < 0.001);
    }

    #[test]
    fn stability_exact_5_point_drop_is_stable() {
        // A delta of exactly 5.0 is not a downward spike — the threshold is > 5.
        let h = history(&[85.0, 80.0]);
        assert!(is_score_stable(&h));
    }

    #[test]
    fn stability_over_5_point_drop_is_unstable() {
        let h = history(&[90.0, 84.0]);
        assert!(!is_score_stable(&h));
    }

    #[test]
    fn single_entry_history_is_stable() {
        let h = history(&[85.0]);
        assert!(is_score_stable(&h));
    }

    #[test]
    fn claude_sonnet_costs_are_nonzero() {
        let cost = span_cost_usd(1_000_000, 100_000, "claude-sonnet-4-6");
        assert!(cost > 0.0);
        assert!((cost - (3.0 + 15.0 * 0.1)).abs() < 0.001);
    }

    #[test]
    fn gpt4o_mini_does_not_match_gpt4o_rate() {
        let mini = span_cost_usd(1_000_000, 0, "gpt-4o-mini");
        let full = span_cost_usd(1_000_000, 0, "gpt-4o");
        assert!(mini < full);
        assert!((mini - 0.15).abs() < 0.001);
        assert!((full - 2.5).abs() < 0.001);
    }

    #[test]
    fn gemini_flash_matches_flash_rate() {
        let flash = span_cost_usd(1_000_000, 0, "gemini-2.0-flash");
        assert!((flash - 0.075).abs() < 0.001);
    }

    #[test]
    fn local_model_returns_zero() {
        assert_eq!(span_cost_usd(100_000, 50_000, "phi4-mini"), 0.0);
        assert_eq!(span_cost_usd(100_000, 50_000, "llama3.1"), 0.0);
    }

    #[test]
    fn unknown_model_returns_zero() {
        assert_eq!(span_cost_usd(100_000, 50_000, "my-custom-model"), 0.0);
    }
}
