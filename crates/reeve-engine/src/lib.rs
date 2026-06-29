pub mod evaluation;
pub mod policy;

use evaluation::TraceContext;
use evaluation::fingerprint::AgentFingerprint;
use evaluation::health_score;
use evaluation::heuristic::{
    CostEfficiencyEvaluator, Evaluator, FingerprintDeviationEvaluator,
    IntentActionDivergenceEvaluator, LatencyNormalityEvaluator, LoopDetector,
};
use reeve_model::ids::AgentId;
use reeve_model::signal::{EngineEvent, IngestionEvent};
use reeve_storage::warm::WarmStore;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::broadcast;

pub async fn run(
    mut ingestion_rx: broadcast::Receiver<IngestionEvent>,
    engine_tx: broadcast::Sender<EngineEvent>,
    warm: Arc<WarmStore>,
) {
    let mut fingerprints: HashMap<AgentId, AgentFingerprint> = HashMap::new();

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
                }

                fingerprints
                    .entry(agent_id)
                    .or_default()
                    .update(span_count, cost, duration_secs);
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
