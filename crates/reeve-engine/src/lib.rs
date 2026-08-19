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
use reeve_model::entity::evaluation::{EvaluationResult, EvaluatorType, TargetType};
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

        let ctx = TraceContext {
            trace_id: trace_id.clone(),
            agent_id: agent_id.clone(),
            span_count,
            cost,
            spans: &spans,
            fingerprint: fp,
        };

        let mut metric_scores: HashMap<&str, f64> = HashMap::new();

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

        self.fingerprints
            .entry(agent_id.clone())
            .or_default()
            .update(span_count, cost, duration_secs);

        let rate = self
            .record_outcomes(&agent_id, tier1_health, span_count)
            .await;

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
                self.engine_tx.clone(),
                self.warm.clone(),
                self.judge.clone(),
            ));
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
            if history.len() > 5 {
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

pub async fn run(
    mut ingestion_rx: broadcast::Receiver<IngestionEvent>,
    engine_tx: broadcast::Sender<EngineEvent>,
    warm: Arc<WarmStore>,
    dispatch_tx: Option<DispatchSender>,
    applied_commands: Option<AppliedCommands>,
    reprobe_requested: Option<ReprobeRequested>,
    live_capabilities: Option<LiveCapabilities>,
) {
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
        judge: Arc::new(LlmJudge::new(backend)),
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
                if requested {
                    let backend = llm_judge::probe().await;
                    let (backend_name, backend_reason) = match &backend {
                        llm_judge::JudgeBackend::Local { model, .. } => {
                            (format!("local ({})", model), None)
                        }
                        llm_judge::JudgeBackend::Disabled { reason } => {
                            ("disabled".to_string(), Some(reason.clone()))
                        }
                    };
                    tracing::info!(backend = %backend_name, "evaluation backend re-probed");
                    let _ = engine_tx.send(EngineEvent::EvaluationBackendReady {
                        backend: backend_name,
                        reason: backend_reason,
                        privacy_tier: startup_privacy_tier,
                    });
                    engine.judge = Arc::new(LlmJudge::new(backend));
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
    for (metric, score, confidence, _) in &results {
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

    fn history(scores: &[f64]) -> VecDeque<f64> {
        scores.iter().copied().collect()
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
}
