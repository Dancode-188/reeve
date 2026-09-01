pub mod budget;
pub mod evaluation;
pub mod outcome;
pub mod policy;

use evaluation::TraceContext;
use evaluation::fingerprint::AgentFingerprint;
use evaluation::heuristic::{
    CostEfficiencyEvaluator, Evaluator, FingerprintDeviationEvaluator,
    IntentActionDivergenceEvaluator, LatencyNormalityEvaluator, LoopDetector,
};
use evaluation::llm_judge::{self, LlmJudge};
use outcome::OutcomeTracker;
use policy::dsl::PolicyContext;
use policy::{PolicyEngine, alert_fields};
use reeve_model::capability::AgentReach;
use reeve_model::entity::agent::IntegrationPath;
use reeve_model::entity::evaluation::{EvaluationResult, EvaluatorType, JudgeAttempt, TargetType};
use reeve_model::entity::intervention::{
    AppliedCommand, CommandStatus, CommandType, InterventionCommand, LiveCapabilities,
};
use reeve_model::entity::span::InternalSpan;
use reeve_model::ids::{AgentId, CommandId, EvalId, RuleId, SpanId, TraceId, current_ms};
use reeve_model::signal::{EngineEvent, EvaluationConfidence, IngestionEvent};
use reeve_storage::warm::WarmStore;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{broadcast, mpsc};

pub type DispatchSender = mpsc::Sender<(AgentId, InterventionCommand)>;

/// Commands the agents confirmed applied, written by the intervention
/// dispatcher and drained here for outcome measurement. Same shared-state
/// pattern as the NTP offset map and the paused-agents set.
pub type AppliedCommands = Arc<std::sync::Mutex<Vec<AppliedCommand>>>;

/// Set by the renderer when the developer presses r on the degraded
/// banner; consumed by the engine, which re-probes the evaluation backend.
/// Same shared-state pattern as the NTP offset map and the paused set.
pub type ReprobeRequested = Arc<std::sync::atomic::AtomicBool>;

/// How many recent Tier 2 sampling scores are kept per agent. Five is
/// enough for `is_score_stable` to see a trend without an old bad run
/// holding the sampling rate up long after the agent recovered.
const SCORE_HISTORY_WINDOW: usize = 5;

/// How long a disabled evaluation backend waits before the first
/// automatic re-probe, and the floor it returns to once one succeeds.
/// One tick of the reprobe timer: the failure this is here for is a
/// startup race lost by seconds.
const AUTO_PROBE_MIN_BACKOFF: std::time::Duration = std::time::Duration::from_secs(2);

/// The ceiling the automatic re-probe backs off to. A minute is short
/// enough that starting Ollama mid-session feels like it just works,
/// and long enough that a machine which will never have it is not
/// making a request every two seconds for the life of the process.
const AUTO_PROBE_MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(60);

/// Doubling backoff for the automatic re-probe, reset by any success.
/// Split out from the loop because the loop it lives in cannot be
/// driven from a test.
fn next_auto_probe_backoff(
    current: std::time::Duration,
    still_disabled: bool,
) -> std::time::Duration {
    if !still_disabled {
        return AUTO_PROBE_MIN_BACKOFF;
    }
    (current * 2).min(AUTO_PROBE_MAX_BACKOFF)
}

/// Everything the run loop carries between events.
///
/// It exists so the event arms can be methods. They touch sixteen values
/// between them, and a free function taking those is harder to read than
/// the inline arm it would replace, which is why #277 could not be done
/// the way it was first written.
///
/// The split is by lifetime, not by topic: the first block is fixed once
/// the loop starts, `judge` is replaced when the backend is re-probed, and
/// the rest accumulates as events arrive.
struct EngineLoop {
    warm: Arc<WarmStore>,
    engine_tx: broadcast::Sender<EngineEvent>,
    dispatch_tx: Option<DispatchSender>,
    applied_commands: Option<AppliedCommands>,
    live_capabilities: Option<LiveCapabilities>,
    evaluators: Vec<Box<dyn Evaluator>>,
    budgets: policy::config::Budgets,

    judge: Arc<LlmJudge>,
    /// Held only so a re-probe can hand the rebuilt judge the same
    /// reader. `None` whenever the operator has not consented to tier 2.
    capture_root: Option<PathBuf>,

    fingerprints: HashMap<AgentId, AgentFingerprint>,
    score_histories: HashMap<AgentId, VecDeque<f64>>,
    cost_accumulators: HashMap<TraceId, CostAccumulator>,
    trace_agents: HashMap<TraceId, AgentId>,
    policy_engine: PolicyEngine,
    outcome_tracker: OutcomeTracker,
    budget_tracker: budget::BudgetTracker,
    /// Where each agent last sat against its cap, so only a crossing warns
    /// or kills rather than every tick.
    budget_states: HashMap<AgentId, budget::BudgetState>,
}

impl EngineLoop {
    /// A trace finished: score it, run policy against the result, settle
    /// its cost, and decide whether to sample it for Tier 2. Extracted
    /// from the select! arm it used to be, which is why the steps read as
    /// one sequence rather than as separate concerns.
    async fn handle_trace_completed(
        &mut self,
        trace_id: TraceId,
        agent_id: AgentId,
        span_count: usize,
        cost: f64,
    ) {
        let spans = self
            .warm
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

        let fp = self.fingerprints.get(&agent_id);

        // Whether any of this was the agent working. Client helper calls
        // still settle their cost below, because they are real money and
        // real latency; what they must not do is move a health score, an
        // agent baseline, or the Tier 2 sample rate. Issue #340.
        let agent_work = is_agent_work(&spans);

        let ctx = TraceContext {
            trace_id: trace_id.clone(),
            agent_id: agent_id.clone(),
            span_count,
            cost,
            spans: &spans,
            fingerprint: fp,
        };

        let mut metric_scores: HashMap<&str, f64> = HashMap::new();

        if agent_work {
            for evaluator in &self.evaluators {
                if let Some(score) = evaluator.evaluate(&ctx) {
                    let _ = self.engine_tx.send(EngineEvent::EvaluationComplete {
                        trace_id: trace_id.clone(),
                        span_id: None,
                        metric: evaluator.name().to_string(),
                        score,
                        confidence: None,
                    });
                    metric_scores.insert(evaluator.name(), score);
                }
            }
        }

        let mut tier1_health: Option<f64> = None;

        if let Some(hs) = reeve_model::scoring::compute(&metric_scores) {
            tier1_health = Some(hs.value);
            let event = EngineEvent::HealthScoreUpdated {
                agent_id: agent_id.clone(),
                trace_id: trace_id.clone(),
                score: hs.value,
                tier2_pending: hs.tier2_pending,
                weight_coverage: hs.weight_coverage,
            };
            if self.engine_tx.send(event).is_err() {
                tracing::debug!("no engine event subscribers");
            }

            if let Err(e) = self
                .warm
                .update_trace_health_score(&trace_id, hs.value, hs.weight_coverage)
                .await
            {
                tracing::warn!(
                    trace_id = %trace_id,
                    error = %e,
                    "failed to persist health score"
                );
            }

            // Policy evaluation runs on Tier 1 results. Tier 2 does not
            // re-trigger to avoid double-firing on the same trace.
            let policy_ctx = PolicyContext::build(
                hs.value,
                cost,
                span_count,
                hs.tier2_pending,
                hs.weight_coverage,
                0.0,
                &metric_scores,
            );
            self.run_policy(&agent_id, &trace_id, &policy_ctx).await;
        }

        // Settle this trace's real cost against the agent's daily
        // budget. The completion path folds in no prediction: the
        // number is now known, so the check is exact.
        if self.budgets.cap_for(agent_id.as_str()).is_some() {
            self.budget_tracker.add_spend(&agent_id, cost);
            self.enforce_budget(&agent_id, &trace_id, 0.0, current_ms())
                .await;
        }

        let rate = if agent_work {
            self.fingerprints
                .entry(agent_id.clone())
                .or_default()
                .update(span_count, cost, duration_secs);

            self.record_outcomes(&agent_id, tier1_health, span_count)
                .await
        } else {
            0.0
        };

        // Tier 2 runs asynchronously after Tier 1 completes. Tier 1
        // always runs; only the Tier 2 spawn is gated by the sample rate.
        let tier1_scores: HashMap<String, f64> = metric_scores
            .iter()
            .map(|(k, v)| (k.to_string(), *v))
            .collect();
        if agent_work {
            // Written before the draw rather than inside the branch
            // that wins it. The traces passed over are what make the
            // survivors mean anything: a corpus holding only the
            // admitted ones records which traces were graded, and not
            // what they were graded out of, so nothing downstream can
            // undo the sampler's preference for unhealthy agents. The
            // rate exists for one coin flip and is gone after it, and
            // it cannot be reconstructed later from the score history
            // because that history has moved on.
            if let Err(e) = self.warm.record_tier2_inclusion(&trace_id, rate).await {
                tracing::warn!(error = %e, "failed to record tier 2 inclusion probability");
            }
            if rand::random::<f64>() < rate {
                tokio::spawn(run_tier2(
                    trace_id.clone(),
                    agent_id.clone(),
                    spans,
                    tier1_scores,
                    self.engine_tx.clone(),
                    self.warm.clone(),
                    self.judge.clone(),
                ));
            }
        }

        self.cost_accumulators.remove(&trace_id);
        self.trace_agents.remove(&trace_id);
    }

    /// Fires every rule whose condition the trace met, and for each one
    /// alerts, records the cooldown, and dispatches when the target can
    /// take the command. The alert goes out either way: ADR-0045.
    async fn run_policy(
        &mut self,
        agent_id: &AgentId,
        trace_id: &TraceId,
        policy_ctx: &PolicyContext,
    ) {
        let now_ms = current_ms();
        let live = declared_capabilities(&self.live_capabilities, agent_id);
        let reach = AgentReach::new(
            integration_path_for(&self.warm, agent_id).await,
            live.as_deref(),
        );
        let fired = self.policy_engine.evaluate(
            agent_id,
            trace_id,
            policy_ctx,
            reach,
            Instant::now(),
            now_ms,
        );
        for fr in fired {
            let (rule_id_str, description, cmd_type, requires_confirmation, auto_confirm) =
                alert_fields(&fr);
            let rule_id = rule_id_str.to_string();
            let effectiveness = effectiveness_hint(&self.warm, &fr.rule.id, agent_id).await;
            let _ = self.engine_tx.send(EngineEvent::PolicyAlert {
                agent_id: agent_id.clone(),
                rule_id: rule_id.clone(),
                description: description.to_string(),
                command_type: cmd_type.map(str::to_string),
                requires_confirmation,
                auto_confirm_after_secs: auto_confirm,
                effectiveness,
            });
            persist_cooldown(
                &self.warm,
                agent_id,
                &fr.rule.id,
                now_ms,
                fr.rule.cooldown_secs,
            )
            .await;
            // Nothing to dispatch when the target cannot run it. The alert
            // above already went out. ADR-0045.
            if fr.command_available {
                dispatch_or_save(
                    &self.dispatch_tx,
                    &self.warm,
                    agent_id,
                    fr.command,
                    requires_confirmation,
                    &rule_id,
                )
                .await;
            }
        }
    }

    /// Folds this trace's score into the agent's history and closes out any
    /// intervention still waiting to be measured against it. Returns the
    /// Tier 2 sampling rate, which the history is what decides.
    ///
    /// Applied commands are picked up BEFORE the new score lands, because
    /// the last score recorded at pickup time is the honest before-picture
    /// for an intervention.
    async fn record_outcomes(
        &mut self,
        agent_id: &AgentId,
        tier1_health: Option<f64>,
        span_count: usize,
    ) -> f64 {
        if let Some(feed) = &self.applied_commands {
            let drained: Vec<AppliedCommand> = feed.lock().unwrap().drain(..).collect();
            for record in drained {
                let pre = self
                    .score_histories
                    .get(&record.agent_id)
                    .and_then(|h| h.back().copied());
                self.outcome_tracker.command_applied(record, pre);
            }
        }

        let history = self.score_histories.entry(agent_id.clone()).or_default();
        if let Some(score) = tier1_health {
            history.push_back(score);
            if history.len() > SCORE_HISTORY_WINDOW {
                history.pop_front();
            }
        }
        let rate = tier2_sample_rate(history);

        if let Some(score) = tier1_health {
            let now_ms = current_ms();
            for outcome in
                self.outcome_tracker
                    .trace_scored(agent_id, score, span_count as u32, now_ms)
            {
                tracing::info!(
                    command_id = %outcome.command_id,
                    delta = ?outcome.delta,
                    "intervention outcome measured"
                );
                if let Err(e) = self.warm.save_intervention_outcome(outcome).await {
                    tracing::warn!(error = %e, "failed to persist intervention outcome");
                }
            }
        }
        rate
    }

    /// A span landed mid-trace: accumulate its cost, extrapolate where the
    /// trace is heading, and let the predicted-cost rules and the budget
    /// see that figure before the trace finishes.
    async fn handle_span_completed(&mut self, trace_id: TraceId, span_id: SpanId) {
        let span = match self.warm.get_span(&span_id).await {
            Ok(Some(s)) => s,
            _ => return,
        };

        // The pipeline already priced every priceable span (the
        // proxy for its own traffic, normalize for SDK spans), so
        // prediction accumulates the stamped cost instead of
        // re-deriving it from tokens. The engine keeping its own
        // price table meant two tables for one quantity, and the
        // engine's had drifted: predictive stops silently never
        // fired for model families only the pipeline knew.
        let span_cost = span
            .attributes
            .get("gen_ai.usage.cost")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);

        if span_cost <= 0.0 {
            return;
        }

        let agent_id = match self.trace_agents.get(&trace_id) {
            Some(a) => a.clone(),
            None => match self.warm.get_trace(&trace_id).await {
                Ok(Some(t)) => {
                    let id = t.agent_id.clone();
                    self.trace_agents.insert(trace_id.clone(), id.clone());
                    id
                }
                _ => return,
            },
        };

        let now_ms = current_ms();
        let acc = self
            .cost_accumulators
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
            return;
        }

        let (old_cost, old_ts) = *acc.samples.front().unwrap();
        let window_secs = (now_ms - old_ts).max(1) as f64 / 1000.0;
        let rate = (acc.current_cost - old_cost) / window_secs;

        let elapsed_total = (now_ms - acc.started_at_ms).max(0) as f64 / 1000.0;
        let avg_duration = self
            .fingerprints
            .get(&agent_id)
            .map(|fp| fp.avg_duration_secs)
            .unwrap_or(30.0);
        let remaining = (avg_duration - elapsed_total).max(0.0);
        let predicted = acc.current_cost + rate * remaining;

        if predicted <= 0.0 {
            return;
        }

        let live = declared_capabilities(&self.live_capabilities, &agent_id);
        let mid_fired = self.policy_engine.evaluate_mid_trace(
            &agent_id,
            &trace_id,
            predicted,
            AgentReach::new(
                integration_path_for(&self.warm, &agent_id).await,
                live.as_deref(),
            ),
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
            let effectiveness = effectiveness_hint(&self.warm, &fr.rule.id, &agent_id).await;
            let _ = self.engine_tx.send(EngineEvent::PolicyAlert {
                agent_id: agent_id.clone(),
                rule_id: rule_id_owned.clone(),
                description: description.to_string(),
                command_type: cmd_type.map(str::to_string),
                requires_confirmation,
                auto_confirm_after_secs,
                effectiveness,
            });
            persist_cooldown(
                &self.warm,
                &agent_id,
                &fr.rule.id,
                now_ms,
                fr.rule.cooldown_secs,
            )
            .await;
            if fr.command_available {
                dispatch_or_save(
                    &self.dispatch_tx,
                    &self.warm,
                    &agent_id,
                    fr.command,
                    requires_confirmation,
                    &rule_id_owned,
                )
                .await;
            }
        }

        // Fold the predicted final cost of this in-flight trace into
        // the budget check so a run that will blow the cap is stopped
        // before it finishes spending. Settled spend does not yet
        // include this trace, so `predicted` is the whole extra.
        if self.budgets.cap_for(agent_id.as_str()).is_some() {
            self.enforce_budget(&agent_id, &trace_id, predicted, now_ms)
                .await;
        }
    }

    /// Checks an agent's spend against its cap and acts on it: always emits a
    /// `BudgetUpdated` so the cockpit's bar tracks the ceiling, warns ALERTS once
    /// on entry into the warn band, and fires a kill the moment settled or
    /// predicted spend crosses the cap. `extra` folds a mid-trace prediction into
    /// the check so the stop lands before the money is gone; it is zero at
    /// completion, when spend is already settled. `last_states` remembers where
    /// each agent sat so only a transition speaks: a fresh alert every tick, or a
    /// re-fired kill against an already-engaged breaker, would be noise.
    async fn enforce_budget(
        &mut self,
        agent_id: &AgentId,
        trace_id: &TraceId,
        extra: f64,
        now_ms: i64,
    ) {
        let Some(cap) = self.budgets.cap_for(agent_id.as_str()) else {
            return;
        };
        let view = self.budget_tracker.view(agent_id, cap, extra);
        let over = view.state == budget::BudgetState::Over;
        let _ = self.engine_tx.send(EngineEvent::BudgetUpdated {
            agent_id: agent_id.clone(),
            spent_today: view.spent_today,
            cap: view.cap,
            over,
        });

        let prev = self.budget_states.insert(agent_id.clone(), view.state);
        let projected = view.spent_today + extra.max(0.0);
        let pct = (projected / cap * 100.0).round() as i64;

        if view.state == budget::BudgetState::Warn
            && !matches!(
                prev,
                Some(budget::BudgetState::Warn | budget::BudgetState::Over)
            )
        {
            let _ = self.engine_tx.send(EngineEvent::PolicyAlert {
                agent_id: agent_id.clone(),
                rule_id: "builtin_budget_warn".to_string(),
                description: format!("budget: {agent_id} nearing its ${cap:.2} daily cap ({pct}%)"),
                command_type: Some("warning".to_string()),
                requires_confirmation: false,
                auto_confirm_after_secs: None,
                effectiveness: None,
            });
        }

        if over && prev != Some(budget::BudgetState::Over) {
            let _ = self.engine_tx.send(EngineEvent::PolicyAlert {
                agent_id: agent_id.clone(),
                rule_id: "builtin_budget_kill".to_string(),
                description: format!("budget: stopped {agent_id} at its ${cap:.2} daily cap"),
                command_type: Some("kill".to_string()),
                requires_confirmation: false,
                auto_confirm_after_secs: None,
                effectiveness: None,
            });
            let command = budget_kill_command(agent_id, trace_id, now_ms);
            dispatch_or_save(
                &self.dispatch_tx,
                &self.warm,
                agent_id,
                command,
                false,
                "builtin_budget",
            )
            .await;
        }
    }
}

/// Everything the engine loop is wired to. A struct rather than a
/// parameter list because the list had already reached the point where
/// call sites read as a row of `None`s, which is how `IngestionConfig`
/// came about on the other side of the seam.
pub struct EngineConfig {
    pub ingestion_rx: broadcast::Receiver<IngestionEvent>,
    pub engine_tx: broadcast::Sender<EngineEvent>,
    pub warm: Arc<WarmStore>,
    pub dispatch_tx: Option<DispatchSender>,
    pub applied_commands: Option<AppliedCommands>,
    pub reprobe_requested: Option<ReprobeRequested>,
    pub live_capabilities: Option<LiveCapabilities>,
    /// Where the proxy stores round trips, at privacy tier 2. The judge
    /// reads a reply from here when the span does not carry one, which
    /// on the proxy path is always. `None` at tier 1, where there is no
    /// store to read.
    pub capture_root: Option<PathBuf>,
}

pub async fn run(config: EngineConfig) {
    let EngineConfig {
        mut ingestion_rx,
        engine_tx,
        warm,
        dispatch_tx,
        applied_commands,
        reprobe_requested,
        live_capabilities,
        capture_root,
    } = config;
    let backend = llm_judge::probe().await;
    let (backend_name, backend_reason) = match &backend {
        llm_judge::JudgeBackend::Local { model, .. } => (format!("local ({})", model), None),
        llm_judge::JudgeBackend::Disabled { reason } => {
            ("disabled".to_string(), Some(reason.clone()))
        }
    };
    let config_path = std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".config/reeve/config.toml"))
        .unwrap_or_else(|_| PathBuf::from(".config/reeve/config.toml"));

    tracing::info!(backend = %backend_name, "evaluation backend ready");
    // One read for every setting this crate needs. The privacy tier is
    // resent unchanged on reprobe: it deliberately does not reload while
    // Reeve runs.
    let config = policy::config::Config::load(&config_path);
    let startup_privacy_tier = config.privacy_tier;
    let _ = engine_tx.send(EngineEvent::EvaluationBackendReady {
        backend: backend_name,
        reason: backend_reason,
        privacy_tier: startup_privacy_tier,
    });
    let budgets = config.budgets.clone();
    let mut engine = EngineLoop {
        warm: warm.clone(),
        engine_tx: engine_tx.clone(),
        dispatch_tx,
        applied_commands,
        live_capabilities,
        evaluators: vec![
            Box::new(LoopDetector::new(3)),
            Box::new(CostEfficiencyEvaluator),
            Box::new(LatencyNormalityEvaluator),
            Box::new(IntentActionDivergenceEvaluator),
            Box::new(FingerprintDeviationEvaluator),
        ],
        budgets: budgets.clone(),
        judge: Arc::new(LlmJudge::new(backend, capture_root.clone())),
        capture_root,
        fingerprints: HashMap::new(),
        score_histories: HashMap::new(),
        cost_accumulators: HashMap::new(),
        trace_agents: HashMap::new(),
        policy_engine: PolicyEngine::with_defaults(),
        outcome_tracker: OutcomeTracker::default(),
        budget_tracker: budget::BudgetTracker::default(),
        budget_states: HashMap::new(),
    };

    {
        let db_rules = warm.load_policy_rules().await.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "failed to load policy rules from database");
            vec![]
        });
        let mut combined = db_rules;
        combined.extend(config.rules.clone());
        engine.policy_engine.replace_user_rules(combined);
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
        engine.policy_engine.load_cooldowns(&cooldowns, startup_ms);
    }

    {
        let samples = warm
            .recent_fingerprint_samples(evaluation::fingerprint::REPLAY_WINDOW)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "failed to load agent history from database");
                vec![]
            });
        // Replayed in arrival order, not averaged flat: the baseline is a
        // moving average, so order decides the weights, and a flat mean
        // would hand back a different number than the one the process had
        // a moment before it stopped.
        let restored = samples.len();
        for sample in samples {
            let duration_secs = match (sample.min_start, sample.max_end) {
                (Some(s), Some(e)) => e.saturating_sub(s).max(0) as f64 / 1e9,
                _ => 0.0,
            };
            engine
                .fingerprints
                .entry(sample.agent_id)
                .or_default()
                .update(sample.span_count, sample.cost, duration_secs);
        }
        if restored > 0 {
            tracing::info!(
                traces = restored,
                agents = engine.fingerprints.len(),
                "restored agent baselines"
            );
        }
    }

    {
        let scores = warm
            .recent_health_scores(SCORE_HISTORY_WINDOW)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "failed to load score history from database");
                vec![]
            });
        // A judged trace has its final_health_score blended across both
        // tiers, so the replayed value is not exactly the Tier 1 number
        // the live path pushed. The judge reaches a few percent of traces
        // and this window holds five, so the difference is bounded, and
        // cheaper than storing the Tier 1 score in its own column.
        let restored = scores.len();
        for (agent_id, score) in scores {
            engine
                .score_histories
                .entry(agent_id)
                .or_default()
                .push_back(score);
        }
        if restored > 0 {
            tracing::info!(
                traces = restored,
                agents = engine.score_histories.len(),
                "restored tier 2 sampling history"
            );
        }
    }

    // SIGUSR1 triggers a policy rule reload. SIGHUP deliberately keeps its
    // default disposition (terminate): for a terminal app, hangup means the
    // terminal went away. An earlier SIGHUP-based reload made Reeve swallow
    // its own hangup and survive every terminal close, holding both ports.
    let mut reload_signal =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())
            .expect("failed to install SIGUSR1 handler");

    // The reprobe flag needs a timer, not event piggybacking: with no
    // agents connected there are no events, and that is exactly when a
    // developer is most likely to be starting Ollama.
    let mut reprobe_tick = tokio::time::interval(std::time::Duration::from_secs(2));

    // A disabled backend also re-probes itself on that timer. The flag
    // had exactly one writer, a keypress on the degraded banner, so an
    // unattended cockpit stayed degraded for the life of the process
    // even when the cause was Ollama losing a startup race by seconds
    // (#331). The first automatic attempt is one tick away, which is
    // the case worth catching quickly; from there the wait doubles to a
    // minute so a machine that will never have Ollama costs one request
    // a minute forever.
    let mut auto_probe_backoff = AUTO_PROBE_MIN_BACKOFF;
    let mut auto_probe_due = tokio::time::Instant::now() + auto_probe_backoff;

    // Budget resync: the tracker's ledger is fed by broadcast events,
    // and a lagged receiver drops them, so under load it silently
    // undercounts (#247). Every 30 seconds the settled figure is
    // rebuilt from the warm store, which heard every span the pipeline
    // kept. Incremental: the first tick covers the day so far, each
    // later tick only the slice since the last, because re-summing the
    // whole day every 30 seconds measured in whole seconds on a soaked
    // store while a slice is under a millisecond. Skipped entirely
    // when no budgets are configured.
    let mut budget_resync_tick = tokio::time::interval(std::time::Duration::from_secs(30));
    let budgets_configured = budgets.default_daily.is_some() || !budgets.per_agent.is_empty();
    // Per-agent settled spend accumulated from store windows, and the
    // day + frontier the accumulation is valid for.
    let mut store_settled: HashMap<AgentId, f64> = HashMap::new();
    let mut resync_day: i64 = budget::local_midnight_ms();
    let mut resync_frontier: i64 = resync_day;

    loop {
        let event = tokio::select! {
            _ = budget_resync_tick.tick(), if budgets_configured => {
                // Midnight rolled: yesterday's accumulation is void.
                let midnight = budget::local_midnight_ms();
                if midnight != resync_day {
                    store_settled.clear();
                    resync_day = midnight;
                    resync_frontier = midnight;
                }
                let until = current_ms();
                match warm.agent_spend_between(resync_frontier, until).await {
                    Ok(window) => {
                        resync_frontier = until;
                        for (agent_id, spend) in window {
                            *store_settled.entry(agent_id).or_insert(0.0) += spend;
                        }
                        for (agent_id, settled) in &store_settled {
                            if budgets.cap_for(agent_id.as_str()).is_none() {
                                continue;
                            }
                            let agent_id = agent_id.clone();
                            engine.budget_tracker.resync(&agent_id, *settled);
                            // A crossing the dropped events hid fires here,
                            // late but fired. Trace id is synthetic: the
                            // kill is day-scoped, not trace-scoped.
                            engine
                                .enforce_budget(
                                    &agent_id,
                                    &TraceId::from(
                                        format!("budget-resync-{}", current_ms()).as_str(),
                                    ),
                                    0.0,
                                    current_ms(),
                                )
                                .await;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "budget resync query failed; ledger continues on events");
                    }
                }
                continue;
            }
            _ = reprobe_tick.tick() => {
                let requested = reprobe_requested
                    .as_ref()
                    .is_some_and(|f| f.swap(false, std::sync::atomic::Ordering::Relaxed));
                // Disabled carries its reason, and the reason can change
                // without the state doing so: a missing Ollama and a
                // missing model are both disabled and want different
                // words on the banner.
                let previous = match &engine.judge.backend {
                    llm_judge::JudgeBackend::Local { model, .. } => (false, model.clone()),
                    llm_judge::JudgeBackend::Disabled { reason } => (true, reason.clone()),
                };
                let auto_due = previous.0 && tokio::time::Instant::now() >= auto_probe_due;
                if requested || auto_due {
                    let backend = llm_judge::probe().await;
                    let (backend_name, backend_reason) = match &backend {
                        llm_judge::JudgeBackend::Local { model, .. } => {
                            (format!("local ({})", model), None)
                        }
                        llm_judge::JudgeBackend::Disabled { reason } => {
                            ("disabled".to_string(), Some(reason.clone()))
                        }
                    };
                    let current = match &backend {
                        llm_judge::JudgeBackend::Local { model, .. } => (false, model.clone()),
                        llm_judge::JudgeBackend::Disabled { reason } => (true, reason.clone()),
                    };
                    auto_probe_backoff = next_auto_probe_backoff(auto_probe_backoff, current.0);
                    auto_probe_due = tokio::time::Instant::now() + auto_probe_backoff;
                    // A silent failed retry is the point. The renderer
                    // writes an info line for every disabled event it is
                    // handed, into a log that is never rotated, so an
                    // unchanged answer stays here. A keypress is always
                    // answered: the banner it raised is waiting for this
                    // event and clears on nothing else.
                    if current != previous || requested {
                        tracing::info!(backend = %backend_name, "evaluation backend re-probed");
                        let _ = engine_tx.send(EngineEvent::EvaluationBackendReady {
                            backend: backend_name,
                            reason: backend_reason,
                            privacy_tier: startup_privacy_tier,
                        });
                    }
                    if current != previous {
                        engine.judge =
                            Arc::new(LlmJudge::new(backend, engine.capture_root.clone()));
                    }
                }
                continue;
            }
            _ = reload_signal.recv() => {
                let db_rules = warm.load_policy_rules().await.unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "failed to reload policy rules from database");
                    vec![]
                });
                let cfg_rules = policy::config::Config::load(&config_path).rules;
                let mut combined = db_rules;
                combined.extend(cfg_rules);
                engine.policy_engine.replace_user_rules(combined);
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
                engine
                    .handle_trace_completed(trace_id, agent_id, span_count, cost)
                    .await;
            }
            Ok(IngestionEvent::SpanCompleted { trace_id, span_id }) => {
                engine.handle_span_completed(trace_id, span_id).await;
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

/// What has historically worked for this rule on this agent, from the
/// measured-outcome aggregation. A minimum of three samples guards against
/// suggesting from noise; below it the alert simply carries no hint. Query
/// failure degrades the same way: an alert without a hint beats no alert.
/// What this agent declared over a live control stream, or `None` if it
/// holds none. `None` is not "declared nothing": it means no channel, which
/// is what `can_command` needs to tell apart. ADR-0045.
fn declared_capabilities(
    live: &Option<LiveCapabilities>,
    agent_id: &AgentId,
) -> Option<Vec<String>> {
    live.as_ref()?.lock().ok()?.get(agent_id).cloned()
}

/// How this agent reaches Reeve, which decides what can be commanded of it.
///
/// Fails open to `Sdk`, the path that rules nothing out. A missing or
/// unreadable agent row is a storage problem; silently suppressing every
/// policy alert for that agent would turn it into a monitoring problem.
async fn integration_path_for(warm: &WarmStore, agent_id: &AgentId) -> IntegrationPath {
    match warm.get_agent(agent_id).await {
        Ok(Some(agent)) => agent.integration,
        Ok(None) => IntegrationPath::Sdk,
        Err(e) => {
            tracing::warn!(
                agent_id = %agent_id,
                error = %e,
                "could not read the agent's integration path; assuming sdk"
            );
            IntegrationPath::Sdk
        }
    }
}

async fn effectiveness_hint(
    warm: &WarmStore,
    rule_id: &RuleId,
    agent_id: &AgentId,
) -> Option<reeve_model::signal::EffectivenessHint> {
    match warm.best_intervention_for_rule(rule_id, agent_id, 3).await {
        Ok(best) => {
            best.map(
                |(command, avg_delta, sample_count)| reeve_model::signal::EffectivenessHint {
                    command,
                    avg_delta,
                    sample_count,
                },
            )
        }
        Err(e) => {
            tracing::warn!(rule_id = %rule_id, error = %e, "effectiveness lookup failed");
            None
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

/// Builds the unconfirmed Kill a crossed budget dispatches through the same
/// policy-to-dispatcher path a rule uses. Issued by "budget", not a policy id,
/// so the audit trail names why it fired.
fn budget_kill_command(agent_id: &AgentId, trace_id: &TraceId, now_ms: i64) -> InterventionCommand {
    InterventionCommand {
        id: CommandId::from(format!("budget:{agent_id}:{trace_id}").as_str()),
        trace_id: trace_id.clone(),
        span_id: None,
        policy_id: None,
        command_type: CommandType::Kill,
        status: CommandStatus::Pending,
        requires_confirmation: false,
        issued_at: now_ms,
        acknowledged_at: None,
        issued_by: "budget".to_string(),
        valid_until_ms: now_ms + 60_000,
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

/// Output cap, in tokens, at or below which a request cannot plausibly be
/// an agent working. Real turns ask for 8192 or 64000 and the client's own
/// helper calls ask for 64 or fewer, so the line sits in a gap two orders
/// of magnitude wide and nothing observed lands near it.
const HELPER_MAX_OUTPUT_TOKENS: i64 = 256;

/// Whether a trace is the agent doing work, as opposed to the client
/// talking to the model on its own account.
///
/// Clients multiplex their own calls onto the same proxy. Claude Code
/// runs a severity classifier and warms connections with a one token
/// request, and both arrive here as ordinary traces that score close to
/// 100 because there is nothing in them to go wrong.
///
/// Only the spans that actually record a request get a say. A trace also
/// carries a synthesized turn span with no attributes on it, and reading
/// that as evidence of anything is what the first cut of this got wrong.
/// A trace where nothing recorded a request is agent work by default,
/// which covers spans arriving off the proxy and the case where the
/// spans failed to load at all: absent evidence must never set a trace
/// aside.
fn is_agent_work(spans: &[InternalSpan]) -> bool {
    let mut saw_a_request = false;
    for helper in spans.iter().filter_map(request_is_helper) {
        saw_a_request = true;
        if !helper {
            return true;
        }
    }
    !saw_a_request
}

/// `None` when the span records no request, otherwise whether that
/// request was a client helper call.
///
/// The test is a conjunction on purpose. Offering no tools is on its own
/// too weak, since a compaction summary offers none either and is real
/// work; capping the output alone would be too weak for the same reason
/// in reverse. It takes both signals agreeing before a trace is set
/// aside.
fn request_is_helper(span: &InternalSpan) -> Option<bool> {
    let max_tokens = span
        .raw_attributes
        .get("reeve.request.max_tokens")
        .and_then(serde_json::Value::as_i64)?;
    let tools = span
        .raw_attributes
        .get("reeve.request.tools")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);
    Some(max_tokens <= HELPER_MAX_OUTPUT_TOKENS && tools == 0)
}

/// A trace sitting exactly on both baselines scores 75.9, not 100, since the
/// cost and latency gauges stopped scoring the average a perfect 1.0. These
/// two thresholds are the old 60% and 80% of typical carried onto that scale,
/// so the dispatch rate lands where it did before. The display bands have not
/// moved; those are a separate call.
const SAMPLE_ALL_BELOW: f64 = 45.5;
const SAMPLE_LESS_ABOVE: f64 = 60.7;

fn tier2_sample_rate(history: &VecDeque<f64>) -> f64 {
    let latest = match history.back() {
        Some(&s) => s,
        None => return 0.20,
    };
    if latest < SAMPLE_ALL_BELOW {
        return 1.0;
    }
    if latest > SAMPLE_LESS_ABOVE && is_score_stable(history) {
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
    let run = judge.evaluate_trace(&spans).await;
    let results = &run.results;

    let model_version = match &judge.backend {
        llm_judge::JudgeBackend::Local { model, .. } => Some(model.clone()),
        llm_judge::JudgeBackend::Disabled { .. } => None,
    };
    let now = current_ms();

    // Written before the results, so a crash between the two leaves a
    // dispatch with no score rather than a score with no dispatch. The
    // first is the state this table exists to describe; the second
    // would be a row claiming a metric was never tried when it was.
    for (metric, outcome, reason) in &run.attempts {
        let attempt = JudgeAttempt {
            id: EvalId::from(format!("{}-{}", trace_id, metric)),
            trace_id: trace_id.to_string(),
            metric: metric.to_string(),
            outcome: *outcome,
            reason: reason.clone(),
            attempted_at: now,
            judge_model_version: model_version.clone(),
        };
        if let Err(e) = warm.save_judge_attempt(attempt).await {
            tracing::warn!(error = %e, metric, "failed to persist judge attempt");
        }
    }

    for (metric, score, confidence, cot_json) in results {
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
            confidence: Some(*confidence),
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
    for (metric, score, confidence, _) in results {
        if *confidence != EvaluationConfidence::Low {
            all_scores.insert(metric, *score);
        }
    }

    if let Some(hs) = reeve_model::scoring::compute(&all_scores) {
        let event = EngineEvent::HealthScoreUpdated {
            agent_id,
            trace_id: trace_id.clone(),
            score: hs.value,
            tier2_pending: false,
            weight_coverage: hs.weight_coverage,
        };
        let _ = engine_tx.send(event);
        if let Err(e) = warm
            .update_trace_health_score(&trace_id, hs.value, hs.weight_coverage)
            .await
        {
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
    use std::time::Duration;

    fn history(scores: &[f64]) -> VecDeque<f64> {
        scores.iter().copied().collect()
    }

    /// A span the proxy synthesizes to close the turn. It records no
    /// request, which is the shape that broke the first cut of this.
    fn turn_span() -> InternalSpan {
        span_asking(None, None)
    }

    fn span_asking(max_tokens: Option<i64>, tools: Option<i64>) -> InternalSpan {
        let mut raw: HashMap<String, serde_json::Value> = HashMap::new();
        if let Some(m) = max_tokens {
            raw.insert("reeve.request.max_tokens".into(), m.into());
        }
        if let Some(t) = tools {
            raw.insert("reeve.request.tools".into(), t.into());
        }
        InternalSpan {
            id: "s1".into(),
            trace_id: "t1".into(),
            parent_id: None,
            operation: "chat".to_string(),
            status: reeve_model::entity::span::SpanStatus::Completed,
            start_time: 0,
            end_time: Some(1000),
            arrived_at: 0,
            attributes: serde_json::Value::Null,
            raw_attributes: raw,
        }
    }

    #[test]
    fn a_capped_toolless_request_is_not_the_agent() {
        // The client's severity classifier: 64 output tokens, no tools,
        // beside the turn span every trace carries.
        let spans = vec![span_asking(Some(64), None), turn_span()];
        assert!(!is_agent_work(&spans));
        // And the one token connection warmup.
        assert!(!is_agent_work(&[
            span_asking(Some(1), Some(0)),
            turn_span()
        ]));
    }

    #[test]
    fn a_span_recording_no_request_is_not_evidence() {
        // The turn span must not vote. Alone it leaves the trace counted,
        // and beside a helper call it must not rescue it.
        assert!(is_agent_work(&[turn_span()]));
        assert!(!is_agent_work(&[turn_span(), span_asking(Some(64), None)]));
    }

    #[test]
    fn one_real_round_makes_the_whole_trace_agent_work() {
        let spans = vec![
            span_asking(Some(64), None),
            span_asking(Some(64000), Some(105)),
            span_asking(Some(64), None),
            turn_span(),
        ];
        assert!(is_agent_work(&spans));
    }

    #[test]
    fn either_signal_alone_leaves_a_trace_counted() {
        // A compaction summary offers no tools but asks for real output.
        assert!(is_agent_work(&[span_asking(Some(8192), None)]));
        // A tool-bearing request stays agent work whatever its cap.
        assert!(is_agent_work(&[span_asking(Some(64), Some(105))]));
    }

    #[test]
    fn absent_evidence_never_sets_a_trace_aside() {
        // Spans that did not come through the proxy carry neither key.
        assert!(is_agent_work(&[span_asking(None, Some(0))]));
        // Nor does a trace whose spans failed to load.
        assert!(is_agent_work(&[]));
    }

    #[test]
    fn auto_probe_backoff_doubles_to_a_ceiling() {
        let mut wait = AUTO_PROBE_MIN_BACKOFF;
        let mut seen = vec![wait];
        for _ in 0..8 {
            wait = next_auto_probe_backoff(wait, true);
            seen.push(wait);
        }
        assert_eq!(
            seen,
            vec![
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
                Duration::from_secs(16),
                Duration::from_secs(32),
                Duration::from_secs(60),
                Duration::from_secs(60),
                Duration::from_secs(60),
                Duration::from_secs(60),
            ]
        );
    }

    #[test]
    fn auto_probe_backoff_resets_on_success() {
        // A backend that comes back after a long outage and drops out
        // again should be retried promptly, not at the ceiling it had
        // climbed to before.
        let settled = next_auto_probe_backoff(AUTO_PROBE_MAX_BACKOFF, true);
        assert_eq!(settled, AUTO_PROBE_MAX_BACKOFF);
        assert_eq!(
            next_auto_probe_backoff(settled, false),
            AUTO_PROBE_MIN_BACKOFF
        );
    }

    #[test]
    fn budget_kill_is_an_unconfirmed_kill_attributed_to_the_budget() {
        let cmd = budget_kill_command(&"claude-cli:proxy".into(), &"trace-1".into(), 1_000);
        assert_eq!(cmd.command_type, CommandType::Kill);
        // Unconfirmed so it dispatches straight through the breaker path, the
        // way a policy kill with requires_confirmation false does.
        assert!(!cmd.requires_confirmation);
        assert_eq!(cmd.status, CommandStatus::Pending);
        // No policy id: the audit trail names the budget, not a rule.
        assert_eq!(cmd.policy_id, None);
        assert_eq!(cmd.issued_by, "budget");
        assert_eq!(cmd.valid_until_ms, 61_000);
    }

    #[test]
    fn rate_is_full_when_score_below_the_floor() {
        let h = history(&[45.0]);
        assert!((tier2_sample_rate(&h) - 1.0).abs() < 0.001);
    }

    #[test]
    fn rate_is_default_when_no_history() {
        let h = VecDeque::new();
        assert!((tier2_sample_rate(&h) - 0.20).abs() < 0.001);
    }

    #[test]
    fn rate_is_low_when_stable_above_the_ceiling() {
        let h = history(&[82.0, 83.0, 84.0, 85.0, 86.0]);
        assert!((tier2_sample_rate(&h) - 0.10).abs() < 0.001);
    }

    #[test]
    fn rate_is_default_when_above_the_ceiling_but_unstable() {
        // Drop from 90 to 84 is a delta of 6 — unstable.
        let h = history(&[82.0, 90.0, 84.0, 85.0, 86.0]);
        assert!((tier2_sample_rate(&h) - 0.20).abs() < 0.001);
    }

    #[test]
    fn rate_is_default_for_mid_range_stable_scores() {
        let h = history(&[50.0, 51.0, 52.0]);
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
}
