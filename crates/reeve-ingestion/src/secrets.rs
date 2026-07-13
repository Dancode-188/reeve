//! Outbound secret detection for the proxy path. The scan runs in
//! memory on bytes already passing through; what survives a finding is
//! the kind, a redacted hint, and a hash fingerprint for dedup. The
//! secret itself is never stored, logged, or put on a span.
//!
//! Detection is deliberately conservative: an alert that cries wolf on
//! every base64 blob trains the operator to ignore the one alert that
//! matters. Known prefixes and structure carry the detection; entropy
//! is a supporting signal gated to assignment-shaped candidates only.

use regex::Regex;
use std::collections::HashSet;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::LazyLock;

/// One detected secret, already redacted. `fingerprint` identifies the
/// secret for dedup without being reversible to it.
#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    /// Human-readable kind, e.g. "anthropic api key".
    pub kind: &'static str,
    /// Redacted display form: prefix plus last four characters.
    pub hint: String,
    /// Hash of the full match, for speaking once per secret.
    pub fingerprint: u64,
}

/// (kind, pattern) for shapes with a known prefix or structure. Word
/// boundaries keep a key embedded in a longer token from matching; the
/// bodies these run over are JSON, so secrets sit inside string values.
static PATTERNS: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    [
        ("anthropic api key", r"\bsk-ant-[A-Za-z0-9_-]{20,}"),
        // OpenAI keys share the sk- prefix; require the longer modern
        // shape so `sk-ant-` (matched above) and short identifiers miss.
        (
            "openai api key",
            r"\bsk-[A-Za-z0-9]{20}T3BlbkFJ[A-Za-z0-9]{20}\b|\bsk-proj-[A-Za-z0-9_-]{40,}",
        ),
        ("aws access key id", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"),
        (
            "github token",
            r"\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{36,}\b|\bgithub_pat_[A-Za-z0-9_]{22,}\b",
        ),
        ("slack token", r"\bxox[baprs]-[A-Za-z0-9-]{10,}\b"),
        ("google api key", r"\bAIza[0-9A-Za-z_-]{35}\b"),
        ("stripe key", r"\b[sr]k_live_[A-Za-z0-9]{20,}\b"),
        (
            "private key",
            r"-----BEGIN (?:RSA |EC |DSA |OPENSSH |PGP )?PRIVATE KEY( BLOCK)?-----",
        ),
        (
            "jwt",
            r"\beyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\b",
        ),
    ]
    .into_iter()
    .map(|(kind, pat)| (kind, Regex::new(pat).expect("static pattern compiles")))
    .collect()
});

/// Assignment-shaped candidate: something named like a credential being
/// given a quoted value. The name gates the entropy check; without it,
/// span ids and content hashes would fire constantly.
static ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)\b([A-Z0-9_.-]*(?:api_?key|apikey|secret|token|passwd|password|credential)[A-Z0-9_.-]*)\\?["']?\s*[:=]\s*\\?["']([A-Za-z0-9+/_=-]{16,})\\?["']"#,
    )
    .expect("static pattern compiles")
});

/// Compiles the patterns now instead of on the first scanned request,
/// which would otherwise pay the one-time cost (about 20ms measured)
/// inside its forwarding overhead.
pub fn warm() {
    LazyLock::force(&PATTERNS);
    LazyLock::force(&ASSIGNMENT);
}

/// Scans a request body and returns redacted findings, deduplicated
/// within the body. Prefixed shapes always count; assignment-shaped
/// candidates additionally need high entropy.
pub fn scan(body: &str) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut seen: HashSet<u64> = HashSet::new();

    for (kind, pattern) in PATTERNS.iter() {
        for m in pattern.find_iter(body) {
            let fp = fingerprint_of(m.as_str());
            if seen.insert(fp) {
                findings.push(Finding {
                    kind,
                    hint: redact(m.as_str()),
                    fingerprint: fp,
                });
            }
        }
    }

    for cap in ASSIGNMENT.captures_iter(body) {
        let value = &cap[2];
        // The prefix patterns already claimed their shapes; entropy
        // decides only the anonymous remainder.
        let fp = fingerprint_of(value);
        if !seen.contains(&fp) && shannon_entropy(value) >= 3.5 && seen.insert(fp) {
            findings.push(Finding {
                kind: "credential assignment",
                hint: redact(value),
                fingerprint: fp,
            });
        }
    }

    findings
}

/// Prefix and last four characters: enough for an operator to recognize
/// which credential leaked, not enough to reconstruct it.
fn redact(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    if chars.len() <= 12 {
        return format!("{}\u{2026}", chars.iter().take(4).collect::<String>());
    }
    let head: String = chars.iter().take(8).collect();
    let tail: String = chars.iter().rev().take(4).rev().collect();
    format!("{head}\u{2026}{tail}")
}

fn fingerprint_of(secret: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    secret.hash(&mut hasher);
    hasher.finish()
}

/// Shannon entropy in bits per character. Real keys sit near 4.5+;
/// English words and paths sit near 3. The 3.5 threshold splits them
/// with room, and only assignment-shaped candidates are ever measured.
fn shannon_entropy(s: &str) -> f64 {
    let len = s.chars().count() as f64;
    if len == 0.0 {
        return 0.0;
    }
    let mut counts: std::collections::HashMap<char, u32> = std::collections::HashMap::new();
    for c in s.chars() {
        *counts.entry(c).or_insert(0) += 1;
    }
    counts
        .values()
        .map(|&n| {
            let p = n as f64 / len;
            -p * p.log2()
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every token-shaped fixture is assembled at runtime: a literal in
    /// source trips secret scanners (GitHub push protection refused the
    /// first commit of this file, then GitGuardian flagged what it let
    /// through). Being caught by the class of tool under construction
    /// is a fine compliment, once.
    fn fake_anthropic_key() -> String {
        format!("sk-ant-{}-{}", "api03", "abcdefghijklmnopqrstuvwx")
    }

    fn fake_aws_key() -> String {
        format!("AKIA{}", "IOSFODNN7EXAMPLE")
    }

    #[test]
    fn known_prefixes_are_detected_and_redacted() {
        let body = format!(
            r#"{{"messages":[{{"role":"user","content":"my key is {} and {}"}}]}}"#,
            fake_anthropic_key(),
            fake_aws_key()
        );
        let findings = scan(&body);
        let kinds: Vec<&str> = findings.iter().map(|f| f.kind).collect();
        assert!(kinds.contains(&"anthropic api key"), "{kinds:?}");
        assert!(kinds.contains(&"aws access key id"), "{kinds:?}");
        // Redaction shows enough to recognize, never the secret.
        for f in &findings {
            assert!(f.hint.len() < 20, "hint too revealing: {}", f.hint);
            assert!(f.hint.contains('\u{2026}'));
        }
    }

    #[test]
    fn github_and_slack_and_pem_shapes_match() {
        let body = format!(
            "ghp_{} xoxb-{}-{} -----BEGIN OPENSSH PRIVATE KEY-----",
            "a".repeat(36),
            "123456789012",
            "abcdefghijklmnop"
        );
        let kinds: Vec<&str> = scan(&body).iter().map(|f| f.kind).collect();
        assert!(kinds.contains(&"github token"), "{kinds:?}");
        assert!(kinds.contains(&"slack token"), "{kinds:?}");
        assert!(kinds.contains(&"private key"), "{kinds:?}");
    }

    #[test]
    fn ordinary_agent_traffic_stays_quiet() {
        // The false-positive corpus: hashes, span ids, base64 content,
        // paths, a git SHA. None of these are secrets; any hit here is
        // the alert that teaches an operator to stop reading alerts.
        let body = r#"{
            "trace_id": "4bf92f3577b34da6a3ce929d0e0e4736",
            "commit": "f83bc48c9e2a1b7d3f56a8e0c4d21e9b7a654321",
            "content": "SGVsbG8gd29ybGQgdGhpcyBpcyBiYXNlNjQ=",
            "path": "/home/user/projects/reeve/crates/reeve-ingestion",
            "id": "toolu_01A2B3C4D5E6F7G8H9J0K1L2"
        }"#;
        assert_eq!(scan(body), vec![], "clean traffic must scan clean");
    }

    #[test]
    fn assignment_with_high_entropy_fires_without_a_prefix() {
        let body = format!(
            r#"{{"content":"DATABASE_PASSWORD=\"{}{}\""}}"#,
            "xK9mP2vQ7wR4tY8u", "Z3aB5cD1eF6gH0jL"
        );
        let findings = scan(&body);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].kind, "credential assignment");
    }

    #[test]
    fn assignment_with_low_entropy_is_ignored() {
        // A named credential with a boring value: placeholder, not leak.
        let body = r#"{"content":"api_key = \"your_api_key_goes_here_ok\""}"#;
        assert_eq!(scan(body), vec![], "placeholders are not findings");
    }

    #[test]
    fn the_same_secret_twice_reports_once() {
        let key = fake_anthropic_key();
        let body = format!("{key} appears twice {key}");
        assert_eq!(scan(&body).len(), 1);
    }

    #[test]
    fn jwt_shape_matches() {
        let body = format!(
            "Bearer {}.{}.{}",
            "eyJhbGciOiJIUzI1NiJ9",
            "eyJzdWIiOiIxMjM0NTY3ODkwIn0",
            "dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U"
        );
        let kinds: Vec<&str> = scan(&body).iter().map(|f| f.kind).collect();
        assert!(kinds.contains(&"jwt"), "{kinds:?}");
    }
}
