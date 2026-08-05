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

/// Everything the config file configures, read and parsed once.
///
/// Six loaders used to open and deserialise this file for one setting
/// apiece. The duplication was cheap; the risk was that each carried its
/// own default for a missing or unreadable file, and those defaults are
/// the interesting part. They are not uniformly "empty": privacy fails
/// closed to tier 1, retention keeps thirty days rather than everything
/// or nothing, and secret blocking stays off so a false positive cannot
/// silently break legitimate traffic. Collecting them in one place is
/// what makes them reviewable together.
#[derive(Debug, Clone)]
pub struct Config {
    pub rules: Vec<PolicyRule>,
    pub budgets: Budgets,
    /// Tier 1 is metadata only. Fails closed, and values above 2 behave
    /// as 2 until redaction layers claim them.
    pub privacy_tier: u8,
    /// Warn first. Blocking on a false positive destroys trust in the
    /// whole feature, so this is opt-in.
    pub secrets_block: bool,
    /// Zero means keep forever. The default exists so a stranger who
    /// never reads the config does not find an unbounded database.
    pub retention_days: u32,
    /// Reaching outside the terminal is opt-in.
    pub notifications_enabled: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            rules: Vec::new(),
            budgets: Budgets::default(),
            privacy_tier: 1,
            secrets_block: false,
            retention_days: 30,
            notifications_enabled: false,
        }
    }
}

impl Config {
    /// Reads and parses the config once. A missing file is valid and
    /// yields defaults; an unparseable one yields defaults and says so,
    /// rather than failing startup over a stray character.
    pub fn load(path: &Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "could not read config; using defaults");
                return Self::default();
            }
        };
        let parsed: ConfigFile = match toml::from_str(&text) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "could not parse config; using defaults");
                return Self::default();
            }
        };
        let defaults = Self::default();
        Self {
            rules: parsed.rules.into_iter().filter_map(to_rule).collect(),
            budgets: parsed
                .budgets
                .map(|b| Budgets {
                    default_daily: b.default_daily,
                    per_agent: b.per_agent,
                })
                .unwrap_or_default(),
            privacy_tier: parsed.privacy_tier.unwrap_or(defaults.privacy_tier).max(1),
            secrets_block: parsed
                .secrets
                .and_then(|s| s.block)
                .unwrap_or(defaults.secrets_block),
            retention_days: parsed
                .retention
                .and_then(|r| r.max_trace_age_days)
                .unwrap_or(defaults.retention_days),
            notifications_enabled: parsed
                .notifications
                .and_then(|n| n.enabled)
                .unwrap_or(defaults.notifications_enabled),
        }
    }
}

/// One config entry to a `PolicyRule`, dropping entries whose command
/// this build cannot issue rather than failing the whole file.
fn to_rule(entry: RuleEntry) -> Option<PolicyRule> {
    let Some(command_type) = parse_command_type(&entry.command_type) else {
        tracing::warn!(
            rule_id = %entry.id,
            command_type = %entry.command_type,
            "unsupported command_type in config rule; skipping"
        );
        return None;
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

    fn load_str(content: &str) -> Config {
        let f = write_temp(content);
        Config::load(f.path())
    }

    /// The defaults are the whole reason this type exists, and they do not
    /// all point the same way. Missing file and broken file must land on
    /// exactly the same values, or a stray character changes behaviour.
    #[test]
    fn a_missing_file_and_an_unparseable_one_give_the_same_defaults() {
        let missing = Config::load(Path::new("/nonexistent/path/config.toml"));
        let broken = load_str("not [ valid toml");
        for c in [&missing, &broken] {
            assert!(c.rules.is_empty());
            assert_eq!(c.privacy_tier, 1, "privacy fails closed");
            assert_eq!(c.retention_days, 30, "not zero, which means keep forever");
            assert!(!c.secrets_block, "blocking is opt-in");
            assert!(!c.notifications_enabled);
            assert_eq!(c.budgets.cap_for("anyone"), None);
        }
    }

    #[test]
    fn a_file_present_but_silent_on_a_setting_still_gets_its_default() {
        let c = load_str("privacy_tier = 2");
        assert_eq!(c.retention_days, 30);
        assert!(!c.secrets_block);
        assert!(!c.notifications_enabled);
        assert!(c.rules.is_empty());
    }

    #[test]
    fn every_setting_reads_back_from_one_parse() {
        let c = load_str(
            r#"
privacy_tier = 2
[notifications]
enabled = true
[secrets]
block = true
[retention]
max_trace_age_days = 7
[budgets]
default_daily = 5.0
"#,
        );
        assert_eq!(c.privacy_tier, 2);
        assert!(c.notifications_enabled);
        assert!(c.secrets_block);
        assert_eq!(c.retention_days, 7);
        assert_eq!(c.budgets.cap_for("anyone"), Some(5.0));
    }

    #[test]
    fn privacy_tier_zero_clamps_to_one() {
        assert_eq!(load_str("privacy_tier = 0").privacy_tier, 1);
    }

    #[test]
    fn retention_zero_means_keep_forever_and_is_not_the_default() {
        assert_eq!(
            load_str("[retention]\nmax_trace_age_days = 0").retention_days,
            0
        );
    }

    #[test]
    fn a_per_agent_cap_overrides_the_default() {
        let c = load_str(
            r#"
[budgets]
default_daily = 5.0
[budgets.per_agent]
"claude-cli:proxy" = 20.0
"#,
        );
        assert_eq!(c.budgets.cap_for("claude-cli:proxy"), Some(20.0));
        assert_eq!(c.budgets.cap_for("someone-else"), Some(5.0));
    }

    #[test]
    fn a_zero_cap_is_unbudgeted_not_a_wall() {
        let c = load_str("[budgets]\ndefault_daily = 0.0");
        assert_eq!(
            c.budgets.cap_for("anyone"),
            None,
            "a stray zero must not stop every request"
        );
    }

    #[test]
    fn a_valid_rule_parses() {
        let c = load_str(
            r#"
[[rules]]
id = "my_rule"
name = "My rule"
trigger_condition = "health_score < 40"
command_type = "pause"
"#,
        );
        assert_eq!(c.rules.len(), 1);
        assert_eq!(c.rules[0].id.as_str(), "my_rule");
        assert_eq!(c.rules[0].command_type, CommandType::Pause);
        assert!(c.rules[0].enabled, "enabled defaults on");
        assert!(c.rules[0].requires_confirmation, "confirmation defaults on");
    }

    #[test]
    fn a_rule_with_an_unissuable_command_is_dropped_not_fatal() {
        let c = load_str(
            r#"
[[rules]]
id = "bad"
name = "Bad"
trigger_condition = "health_score < 40"
command_type = "teleport"

[[rules]]
id = "good"
name = "Good"
trigger_condition = "health_score < 40"
command_type = "kill"
"#,
        );
        assert_eq!(
            c.rules.len(),
            1,
            "the unknown command drops its own rule only"
        );
        assert_eq!(c.rules[0].id.as_str(), "good");
    }

    #[test]
    fn agent_scope_parses() {
        let c = load_str(
            r#"
[[rules]]
id = "scoped"
name = "Scoped"
trigger_condition = "health_score < 40"
command_type = "pause"
scope = "agent:claude-cli:proxy"
"#,
        );
        assert_eq!(
            c.rules[0].scope,
            RuleScope::Agent("claude-cli:proxy".to_string())
        );
    }

    #[test]
    fn a_privacy_tier_coexists_with_rules_in_one_file() {
        let c = load_str(
            r#"
privacy_tier = 2
[[rules]]
id = "r"
name = "R"
trigger_condition = "cost_usd > 1.0"
command_type = "kill"
"#,
        );
        assert_eq!(c.privacy_tier, 2);
        assert_eq!(c.rules.len(), 1);
    }
}
