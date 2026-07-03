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
use reeve_model::entity::span::InternalSpan;
use reeve_model::ids::AgentId;
use reeve_model::signal::{EngineEvent, EvaluationConfidence, IngestionEvent};
use reeve_storage::warm::WarmStore;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;

pub async fn run(
    mut ingestion_rx: broadcast::Receiver<IngestionEvent>,
    engine_tx: broadcast::Sender<EngineEvent>,
    warm: Arc<WarmStore>,
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

    let mut fingerprints: HashMap<AgentId, AgentFingerprint> = HashMap::new();
    let mut policy_engine = PolicyEngine::with_defaults();

    let evaluators: Vec<Box<dyn Evaluator>> = vec![
        Box::new(LoopDetector::new(3)),
        Box::new(CostEfficiencyEvaluator),
        Box::new(LatencyNormalityEvaluator),
        Box::new(IntentActionDivergenceEvaluator),
        Box::new(FingerprintDeviationEvaluator),
    ];

    loop {
        match ingestion_rx.recv().await {
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

                if let Some(hs) = health_score::compute(&metric_scores) {
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
                        let (rule_id_str, description, cmd_type, requires_confirmation) =
                            alert_fields(&fr);
                        let rule_id_owned = rule_id_str.to_string();
                        let _ = engine_tx.send(EngineEvent::PolicyAlert {
                            rule_id: rule_id_owned.clone(),
                            description: description.to_string(),
                            command_type: cmd_type.to_string(),
                            requires_confirmation,
                        });
                        if let Err(e) = warm.save_intervention_command(fr.command).await {
                            tracing::warn!(
                                rule_id = %rule_id_owned,
                                error = %e,
                                "failed to persist intervention command"
                            );
                        }
                    }
                }

                fingerprints.entry(agent_id.clone()).or_default().update(
                    span_count,
                    cost,
                    duration_secs,
                );

                // Tier 2 runs asynchronously after Tier 1 completes.
                let tier1_scores: HashMap<String, f64> = metric_scores
                    .iter()
                    .map(|(k, v)| (k.to_string(), *v))
                    .collect();
                tokio::spawn(run_tier2(
                    trace_id,
                    agent_id,
                    spans,
                    tier1_scores,
                    engine_tx.clone(),
                    warm.clone(),
                    judge.clone(),
                ));
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

fn current_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
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

    for &(metric, score, confidence) in &results {
        let _ = engine_tx.send(EngineEvent::EvaluationComplete {
            trace_id: trace_id.clone(),
            span_id: None,
            metric: metric.to_string(),
            score,
            confidence: Some(confidence),
        });
    }

    // Merge Tier 1 scores with non-Low-confidence Tier 2 scores before
    // recomputing. Low-confidence results are still emitted above so the
    // policy engine and renderer can act on them, but they do not affect
    // the health score value.
    let mut all_scores: HashMap<&str, f64> =
        tier1_scores.iter().map(|(k, v)| (k.as_str(), *v)).collect();
    for &(metric, score, confidence) in &results {
        if confidence != EvaluationConfidence::Low {
            all_scores.insert(metric, score);
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
