use reeve_model::entity::intervention::CommandType;
use reeve_model::entity::policy::{PolicyRule, RuleScope};
use reeve_model::ids::RuleId;
use serde::Deserialize;
use std::path::Path;

#[derive(Deserialize)]
struct ConfigFile {
    #[serde(default)]
    rules: Vec<RuleEntry>,
    privacy_tier: Option<u8>,
    notifications: Option<NotificationsSection>,
    budgets: Option<BudgetsSection>,
    secrets: Option<SecretsSection>,
    retention: Option<RetentionSection>,
}

#[derive(Deserialize)]
struct SecretsSection {
    block: Option<bool>,
}

#[derive(Deserialize)]
struct RetentionSection {
    max_trace_age_days: Option<u32>,
}

#[derive(Deserialize)]
struct BudgetsSection {
    /// Applies to every agent without its own entry.
    default_daily: Option<f64>,
    /// Per-agent daily caps, keyed by agent id (e.g. "claude-cli:proxy").
    #[serde(default)]
    per_agent: std::collections::HashMap<String, f64>,
}

/// Resolved daily spend caps. An agent's cap is its per-agent entry, or
/// the default, or none (unbudgeted). Read once at startup like the rest
/// of the config.
#[derive(Debug, Clone, Default)]
pub struct Budgets {
    pub default_daily: Option<f64>,
    pub per_agent: std::collections::HashMap<String, f64>,
}

impl Budgets {
    /// The daily cap for an agent, if any. A per-agent entry overrides
    /// the default; a zero or negative cap is treated as unbudgeted so a
    /// stray `0.0` never stops every request.
    pub fn cap_for(&self, agent_id: &str) -> Option<f64> {
        self.per_agent
            .get(agent_id)
            .copied()
            .or(self.default_daily)
            .filter(|c| *c > 0.0)
    }
}

/// Loads daily budgets from the config, empty when the file or section
/// is absent.
pub fn load_budgets(path: &Path) -> Budgets {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Budgets::default();
    };
    match toml::from_str::<ConfigFile>(&text) {
        Ok(c) => match c.budgets {
            Some(b) => Budgets {
                default_daily: b.default_daily,
                per_agent: b.per_agent,
            },
            None => Budgets::default(),
        },
        Err(e) => {
            tracing::warn!(error = %e, "failed to parse budgets from config");
            Budgets::default()
        }
    }
}

/// Whether detected outbound secrets should block the request instead of
/// only warning. Default off: warn-first, because a false positive that
/// blocks legitimate traffic destroys trust in the whole feature. A
/// missing or unparseable file means warn-only, never surprise blocking.
pub fn load_secrets_block(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    match toml::from_str::<ConfigFile>(&text) {
        Ok(c) => c.secrets.and_then(|s| s.block).unwrap_or(false),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "could not parse config; secret blocking defaults to off");
            false
        }
    }
}

/// How many days of completed traces the warm store keeps. Default 30:
/// a stranger who never reads the config should not discover an
/// unbounded database months in. Zero means keep forever, and a
/// missing or unparseable file gets the default rather than surprise
/// deletion of nothing or everything.
pub fn load_retention_days(path: &Path) -> u32 {
    const DEFAULT_DAYS: u32 = 30;
    let Ok(text) = std::fs::read_to_string(path) else {
        return DEFAULT_DAYS;
    };
    match toml::from_str::<ConfigFile>(&text) {
        Ok(c) => c
            .retention
            .and_then(|r| r.max_trace_age_days)
            .unwrap_or(DEFAULT_DAYS),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "could not parse config; retention defaults to 30 days");
            DEFAULT_DAYS
        }
    }
}

#[derive(Deserialize)]
struct NotificationsSection {
    enabled: Option<bool>,
}

#[derive(Deserialize)]
struct RuleEntry {
    id: String,
    name: String,
    #[serde(default)]
    description: String,
    trigger_condition: String,
    command_type: String,
    #[serde(default = "default_true")]
    requires_confirmation: bool,
    #[serde(default = "default_cooldown")]
    cooldown_secs: u64,
    #[serde(default)]
    scope: String,
    #[serde(default = "default_true")]
    enabled: bool,
    auto_confirm_after_secs: Option<u64>,
}

fn default_true() -> bool {
    true
}

fn default_cooldown() -> u64 {
    300
}

fn parse_command_type(s: &str) -> Option<CommandType> {
    match s.to_ascii_lowercase().as_str() {
        "pause" => Some(CommandType::Pause),
        "resume" => Some(CommandType::Resume),
        "kill" => Some(CommandType::Kill),
        _ => None,
    }
}

fn parse_scope(s: &str) -> RuleScope {
    match s.split_once(':') {
        Some(("agent", id)) => RuleScope::Agent(id.to_string()),
        Some(("framework", name)) => RuleScope::Framework(name.to_string()),
        _ => RuleScope::Global,
    }
}

/// Loads user-defined rules from the given path (normally `~/.config/reeve/config.toml`).
/// Returns an empty vec if the file does not exist; no config file is valid.
pub fn load(path: &Path) -> Vec<PolicyRule> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return vec![],
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "could not read policy config");
            return vec![];
        }
    };

    let config: ConfigFile = match toml::from_str(&text) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "could not parse policy config");
            return vec![];
        }
    };

    config
        .rules
        .into_iter()
        .filter_map(|entry| {
            let command_type = match parse_command_type(&entry.command_type) {
                Some(ct) => ct,
                None => {
                    tracing::warn!(
                        rule_id = %entry.id,
                        command_type = %entry.command_type,
                        "unsupported command_type in config rule; skipping"
                    );
                    return None;
                }
            };
            Some(PolicyRule {
                id: RuleId::from(entry.id.as_str()),
                name: entry.name,
                description: entry.description,
                trigger_condition: entry.trigger_condition,
                command_type,
                requires_confirmation: entry.requires_confirmation,
                cooldown_secs: entry.cooldown_secs,
                scope: parse_scope(&entry.scope),
                enabled: entry.enabled,
                auto_confirm_after_secs: entry.auto_confirm_after_secs,
            })
        })
        .collect()
}

/// The configured privacy tier, default 1 (metadata only, no content).
/// Tier 2 enables SpanEvent content capture. Values above 2 are accepted
/// and behave as 2 until redaction layers claim them. A missing or
/// unparseable file falls back to 1: privacy fails closed.
pub fn load_privacy_tier(path: &Path) -> u8 {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return 1,
    };
    match toml::from_str::<ConfigFile>(&text) {
        Ok(c) => c.privacy_tier.unwrap_or(1).max(1),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "could not parse config; privacy tier defaults to 1");
            1
        }
    }
}

/// Whether desktop notifications are enabled. Default off: reaching
/// outside the terminal is opt-in, and a missing or unparseable file
/// means no notifications rather than surprise ones.
pub fn load_notifications_enabled(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    match toml::from_str::<ConfigFile>(&text) {
        Ok(c) => c.notifications.and_then(|n| n.enabled).unwrap_or(false),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "could not parse config; notifications default to off");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_temp(content: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f
    }

    #[test]
    fn notifications_default_off_and_fail_closed() {
        assert!(!load_notifications_enabled(Path::new("/nonexistent")));
        let f = write_temp("privacy_tier = 2");
        assert!(
            !load_notifications_enabled(f.path()),
            "absent section is off"
        );
        let f = write_temp("[notifications]\nenabled = true");
        assert!(load_notifications_enabled(f.path()));
        let f = write_temp("not [ valid toml");
        assert!(!load_notifications_enabled(f.path()), "unparseable is off");
    }

    #[test]
    fn load_returns_empty_for_missing_file() {
        let rules = load(std::path::Path::new("/nonexistent/path/config.toml"));
        assert!(rules.is_empty());
    }

    #[test]
    fn load_parses_valid_rule() {
        let f = write_temp(
            r#"
[[rules]]
id = "my_rule"
name = "My Rule"
description = "test"
trigger_condition = "health_score < 50"
command_type = "pause"
"#,
        );
        let rules = load(f.path());
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].id.as_str(), "my_rule");
        assert_eq!(rules[0].command_type, CommandType::Pause);
        assert!(rules[0].requires_confirmation);
        assert_eq!(rules[0].cooldown_secs, 300);
        assert_eq!(rules[0].scope, RuleScope::Global);
    }

    #[test]
    fn load_skips_unknown_command_type() {
        let f = write_temp(
            r#"
[[rules]]
id = "bad_rule"
name = "Bad"
trigger_condition = "health_score < 50"
command_type = "redirect"
"#,
        );
        let rules = load(f.path());
        assert!(rules.is_empty());
    }

    #[test]
    fn privacy_tier_defaults_to_one() {
        assert_eq!(
            load_privacy_tier(std::path::Path::new("/nonexistent/config.toml")),
            1
        );
        let empty = write_temp("");
        assert_eq!(load_privacy_tier(empty.path()), 1);
    }

    #[test]
    fn privacy_tier_reads_configured_value() {
        let f = write_temp("privacy_tier = 2\n");
        assert_eq!(load_privacy_tier(f.path()), 2);
    }

    #[test]
    fn privacy_tier_zero_clamps_to_one() {
        let f = write_temp("privacy_tier = 0\n");
        assert_eq!(load_privacy_tier(f.path()), 1);
    }

    #[test]
    fn privacy_tier_coexists_with_rules() {
        let f = write_temp(
            r#"
privacy_tier = 2

[[rules]]
id = "r1"
name = "R"
trigger_condition = "health_score < 50"
command_type = "pause"
"#,
        );
        assert_eq!(load_privacy_tier(f.path()), 2);
        assert_eq!(load(f.path()).len(), 1);
    }

    #[test]
    fn load_parses_agent_scope() {
        let f = write_temp(
            r#"
[[rules]]
id = "scoped_rule"
name = "Scoped"
trigger_condition = "cost_usd > 1.0"
command_type = "kill"
scope = "agent:my-agent-id"
"#,
        );
        let rules = load(f.path());
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].scope, RuleScope::Agent("my-agent-id".to_string()));
    }

    #[test]
    fn secrets_block_defaults_off_and_fails_to_warn_only() {
        assert!(!load_secrets_block(Path::new("/nonexistent")));
        let f = write_temp("privacy_tier = 1");
        assert!(!load_secrets_block(f.path()), "absent section warns only");
        let f = write_temp("[secrets]\nblock = true");
        assert!(load_secrets_block(f.path()));
        let f = write_temp("not [ valid toml");
        assert!(
            !load_secrets_block(f.path()),
            "unparseable config must never surprise-block"
        );
    }

    #[test]
    fn retention_defaults_to_thirty_and_zero_disables() {
        assert_eq!(load_retention_days(Path::new("/nonexistent")), 30);
        let f = write_temp("privacy_tier = 1");
        assert_eq!(load_retention_days(f.path()), 30, "absent section defaults");
        let f = write_temp("[retention]\nmax_trace_age_days = 7");
        assert_eq!(load_retention_days(f.path()), 7);
        let f = write_temp("[retention]\nmax_trace_age_days = 0");
        assert_eq!(load_retention_days(f.path()), 0, "zero is keep-forever");
        let f = write_temp("not [ valid");
        assert_eq!(load_retention_days(f.path()), 30, "unparseable defaults");
    }

    #[test]
    fn budgets_resolve_per_agent_over_default() {
        let f = write_temp(
            r#"
[budgets]
default_daily = 5.0

[budgets.per_agent]
"claude-cli:proxy" = 20.0
"#,
        );
        let b = load_budgets(f.path());
        assert_eq!(b.cap_for("claude-cli:proxy"), Some(20.0), "per-agent wins");
        assert_eq!(b.cap_for("other:proxy"), Some(5.0), "falls back to default");
    }

    #[test]
    fn a_zero_cap_is_unbudgeted_not_a_wall() {
        let f = write_temp("[budgets]\ndefault_daily = 0.0\n");
        assert_eq!(
            load_budgets(f.path()).cap_for("any"),
            None,
            "a stray zero must not stop every request"
        );
        assert!(
            load_budgets(Path::new("/nonexistent"))
                .cap_for("any")
                .is_none(),
            "no config, no budget"
        );
    }
}
