pub mod dsl;

use dsl::PolicyContext;
use reeve_model::entity::intervention::{CommandStatus, CommandType, InterventionCommand};
use reeve_model::entity::policy::{PolicyRule, RuleScope};
use reeve_model::ids::{AgentId, CommandId, RuleId, TraceId};
use std::collections::HashMap;
use std::time::{Duration, Instant};

const COMMAND_VALIDITY_MS: i64 = 60_000;

pub struct FiredRule {
    pub rule: PolicyRule,
    pub command: InterventionCommand,
}

pub struct PolicyEngine {
    rules: Vec<PolicyRule>,
    cooldowns: HashMap<(AgentId, RuleId), Instant>,
}

impl PolicyEngine {
    pub fn with_defaults() -> Self {
        let rules = vec![
            PolicyRule {
                id: RuleId::from("builtin_low_health"),
                name: "Low health score".to_string(),
                trigger_condition: "health_score < 30".to_string(),
                command_type: CommandType::Pause,
                requires_confirmation: true,
                cooldown_secs: 300,
                scope: RuleScope::Global,
                enabled: true,
                auto_confirm_after_secs: None,
            },
            PolicyRule {
                id: RuleId::from("builtin_high_cost"),
                name: "High trace cost".to_string(),
                trigger_condition: "cost_usd > 5.0".to_string(),
                command_type: CommandType::Pause,
                requires_confirmation: true,
                cooldown_secs: 300,
                scope: RuleScope::Global,
                enabled: true,
                auto_confirm_after_secs: None,
            },
            PolicyRule {
                id: RuleId::from("builtin_loop_detected"),
                name: "Loop detected".to_string(),
                trigger_condition: "loop_detection < 0.5".to_string(),
                command_type: CommandType::Pause,
                requires_confirmation: true,
                cooldown_secs: 300,
                scope: RuleScope::Global,
                enabled: true,
                auto_confirm_after_secs: None,
            },
        ];
        Self {
            rules,
            cooldowns: HashMap::new(),
        }
    }

    /// Evaluate all rules against `ctx` for the given agent and trace.
    ///
    /// Policy fires once per trace on Tier 1 results. The `now` parameter
    /// is passed in rather than read inside so tests can control the clock.
    pub fn evaluate(
        &mut self,
        agent_id: &AgentId,
        trace_id: &TraceId,
        ctx: &PolicyContext,
        now: Instant,
        now_ms: i64,
    ) -> Vec<FiredRule> {
        let mut fired = Vec::new();
        for rule in &self.rules {
            if !rule.enabled {
                continue;
            }
            let key = (agent_id.clone(), rule.id.clone());
            if let Some(&last) = self.cooldowns.get(&key) {
                if now.duration_since(last) < Duration::from_secs(rule.cooldown_secs) {
                    continue;
                }
            }
            if ctx.evaluate(&rule.trigger_condition) {
                self.cooldowns.insert(key, now);
                fired.push(FiredRule {
                    command: build_command(rule, trace_id, now_ms),
                    rule: rule.clone(),
                });
            }
        }
        fired
    }
}

fn build_command(rule: &PolicyRule, trace_id: &TraceId, now_ms: i64) -> InterventionCommand {
    let status = if rule.requires_confirmation {
        CommandStatus::PendingConfirmation
    } else {
        CommandStatus::Pending
    };
    InterventionCommand {
        id: CommandId::from(format!("{}:{}", rule.id, trace_id).as_str()),
        trace_id: trace_id.clone(),
        span_id: None,
        policy_id: Some(rule.id.clone()),
        command_type: rule.command_type.clone(),
        status,
        requires_confirmation: rule.requires_confirmation,
        issued_at: now_ms,
        acknowledged_at: None,
        issued_by: format!("policy:{}", rule.id),
        valid_until_ms: now_ms + COMMAND_VALIDITY_MS,
    }
}

fn command_type_str(ct: &CommandType) -> &'static str {
    match ct {
        CommandType::Pause => "pause",
        CommandType::Resume => "resume",
        CommandType::Kill => "kill",
        CommandType::Redirect { .. } => "redirect",
        CommandType::InjectContext { .. } => "inject_context",
    }
}

pub fn alert_fields(fired: &FiredRule) -> (&str, &'static str, bool) {
    (
        fired.rule.id.as_str(),
        command_type_str(&fired.rule.command_type),
        fired.rule.requires_confirmation,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn agent() -> AgentId {
        AgentId::from("agent-1")
    }

    fn trace() -> TraceId {
        TraceId::from("trace-1")
    }

    fn ctx(health_score: f64, cost_usd: f64, loop_detection: f64) -> PolicyContext {
        let mut metrics = HashMap::new();
        metrics.insert("loop_detection", loop_detection);
        PolicyContext::build(health_score, cost_usd, 5, false, 0.45, &metrics)
    }

    #[test]
    fn low_health_score_fires_rule() {
        let mut engine = PolicyEngine::with_defaults();
        let fired = engine.evaluate(&agent(), &trace(), &ctx(25.0, 1.0, 0.9), Instant::now(), 0);
        assert!(
            fired
                .iter()
                .any(|f| f.rule.id.as_str() == "builtin_low_health")
        );
    }

    #[test]
    fn high_cost_fires_rule() {
        let mut engine = PolicyEngine::with_defaults();
        let fired = engine.evaluate(&agent(), &trace(), &ctx(80.0, 6.0, 0.9), Instant::now(), 0);
        assert!(
            fired
                .iter()
                .any(|f| f.rule.id.as_str() == "builtin_high_cost")
        );
    }

    #[test]
    fn loop_detection_fires_rule() {
        let mut engine = PolicyEngine::with_defaults();
        let fired = engine.evaluate(&agent(), &trace(), &ctx(80.0, 1.0, 0.3), Instant::now(), 0);
        assert!(
            fired
                .iter()
                .any(|f| f.rule.id.as_str() == "builtin_loop_detected")
        );
    }

    #[test]
    fn healthy_trace_fires_no_rules() {
        let mut engine = PolicyEngine::with_defaults();
        let fired = engine.evaluate(&agent(), &trace(), &ctx(85.0, 1.0, 0.9), Instant::now(), 0);
        assert!(fired.is_empty());
    }

    #[test]
    fn multiple_rules_can_fire_together() {
        let mut engine = PolicyEngine::with_defaults();
        let fired = engine.evaluate(&agent(), &trace(), &ctx(25.0, 6.0, 0.3), Instant::now(), 0);
        assert_eq!(fired.len(), 3);
    }

    #[test]
    fn cooldown_prevents_immediate_refire() {
        let mut engine = PolicyEngine::with_defaults();
        let c = ctx(25.0, 1.0, 0.9);
        let now = Instant::now();
        let first = engine.evaluate(&agent(), &trace(), &c, now, 0);
        assert!(
            first
                .iter()
                .any(|f| f.rule.id.as_str() == "builtin_low_health")
        );
        let second = engine.evaluate(&agent(), &trace(), &c, now, 0);
        assert!(
            second
                .iter()
                .all(|f| f.rule.id.as_str() != "builtin_low_health")
        );
    }

    #[test]
    fn cooldown_expires_after_duration() {
        let mut engine = PolicyEngine::with_defaults();
        let c = ctx(25.0, 1.0, 0.9);
        let now = Instant::now();
        engine.evaluate(&agent(), &trace(), &c, now, 0);
        let later = now + Duration::from_secs(301);
        let fired = engine.evaluate(&agent(), &trace(), &c, later, 0);
        assert!(
            fired
                .iter()
                .any(|f| f.rule.id.as_str() == "builtin_low_health")
        );
    }

    #[test]
    fn command_id_is_rule_and_trace_composite() {
        let mut engine = PolicyEngine::with_defaults();
        let fired = engine.evaluate(&agent(), &trace(), &ctx(25.0, 1.0, 0.9), Instant::now(), 0);
        let cmd = &fired
            .iter()
            .find(|f| f.rule.id.as_str() == "builtin_low_health")
            .unwrap()
            .command;
        assert_eq!(cmd.id.as_str(), "builtin_low_health:trace-1");
    }

    #[test]
    fn command_status_is_pending_confirmation_when_required() {
        let mut engine = PolicyEngine::with_defaults();
        let fired = engine.evaluate(&agent(), &trace(), &ctx(25.0, 1.0, 0.9), Instant::now(), 0);
        let cmd = &fired
            .iter()
            .find(|f| f.rule.id.as_str() == "builtin_low_health")
            .unwrap()
            .command;
        assert_eq!(cmd.status, CommandStatus::PendingConfirmation);
    }

    #[test]
    fn different_agents_have_independent_cooldowns() {
        let mut engine = PolicyEngine::with_defaults();
        let c = ctx(25.0, 1.0, 0.9);
        let now = Instant::now();
        engine.evaluate(&AgentId::from("agent-a"), &trace(), &c, now, 0);
        let fired = engine.evaluate(&AgentId::from("agent-b"), &trace(), &c, now, 0);
        assert!(
            fired
                .iter()
                .any(|f| f.rule.id.as_str() == "builtin_low_health")
        );
    }
}
