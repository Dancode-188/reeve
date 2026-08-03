//! Which commands an agent can actually be sent, given how it is wired up.
//!
//! This used to be three separate answers. The renderer dimmed the rows its
//! overlay could not offer, the dispatcher refused what it could not deliver,
//! and the policy engine knew nothing at all, so it would alert "suggested
//! action: pause" directly above "this agent does not support pause". One
//! answer here means the three cannot drift apart again.

use crate::entity::agent::IntegrationPath;
use crate::entity::intervention::CommandType;

/// Whether an integration path can carry this command at all.
///
/// This is the static half of the answer, fixed by how the agent reaches
/// Reeve. SDK agents narrow it further at runtime: their control handshake
/// declares what the framework actually implements, and where that list
/// exists it wins. Nothing widens this.
pub fn path_supports(path: IntegrationPath, command: &CommandType) -> bool {
    match path {
        // No handshake has been seen yet, or the caller is asking about the
        // path rather than a specific agent. The path rules nothing out;
        // the handshake is the real answer.
        IntegrationPath::Sdk => true,
        // The proxy sits on the request path, so it can rewrite the next
        // request or refuse to forward it. It cannot reach inside a turn
        // already in flight, which is what pause means, so pause is absent
        // rather than present-but-broken. Resume is not listed because the
        // proxy's resume is the breaker revive, handled before any
        // capability question is asked.
        IntegrationPath::Proxy => matches!(
            command,
            CommandType::Redirect { .. } | CommandType::InjectContext { .. } | CommandType::Kill
        ),
        // Log agents are read from a file after the fact. There is no
        // channel to send anything down.
        IntegrationPath::Log => false,
    }
}

/// The capability names a path fixes on its own, in the string form the
/// control handshake and the renderer's overlay both use.
///
/// `None` for SDK agents, and that is the point: the path allows everything,
/// so the handshake is the only real answer and there is nothing to fall back
/// on until it arrives. Callers must not read `None` as "no capabilities" —
/// an agent that has not declared yet is unknown, not incapable.
pub fn path_capabilities(path: IntegrationPath) -> Option<Vec<String>> {
    if path == IntegrationPath::Sdk {
        return None;
    }
    Some(
        [
            CommandType::Pause,
            CommandType::Redirect {
                instruction: String::new(),
            },
            CommandType::InjectContext {
                context: String::new(),
            },
            CommandType::Kill,
        ]
        .iter()
        .filter(|c| path_supports(path, c))
        .map(|c| capability_name(c).to_string())
        .collect(),
    )
}

/// How an agent can be reached right now: the path its spans arrive on,
/// plus whatever it declared over a live control stream, if it holds one.
///
/// The two travel together because neither answers the question alone. The
/// path knows what a proxy agent can take without any handshake; the
/// handshake knows what one particular SDK agent implements. `live: None`
/// means no stream exists, which is why an SDK agent can end up reachable
/// by nothing at all. ADR-0045.
#[derive(Debug, Clone, Copy)]
pub struct AgentReach<'a> {
    pub path: IntegrationPath,
    pub live: Option<&'a [String]>,
}

impl<'a> AgentReach<'a> {
    pub fn new(path: IntegrationPath, live: Option<&'a [String]>) -> Self {
        Self { path, live }
    }
}

/// Whether this agent can be sent this command right now.
///
/// A live control stream is authoritative in both directions: what it
/// declared is what the agent can do, and what it left out the agent
/// cannot. With no live stream the answer comes from the path, and on the
/// SDK path that means nothing at all, because the channel that would
/// carry the command is exactly the thing that is missing.
///
/// This never decides whether a rule fires, only whether its command is
/// worth offering. ADR-0045.
pub fn can_command(reach: AgentReach<'_>, command: &CommandType) -> bool {
    let needed = required_capability(command);
    match reach.live {
        Some(declared) => declared.iter().any(|c| c == needed),
        None => path_capabilities(reach.path).is_some_and(|caps| caps.iter().any(|c| c == needed)),
    }
}

/// The capability a command requires, which is not always its own name.
/// An adapter declaring `pause` is declaring that it can stop and start,
/// so `resume` rides on the same declaration rather than needing its own.
fn required_capability(command: &CommandType) -> &'static str {
    match command {
        CommandType::Resume => "pause",
        other => capability_name(other),
    }
}

/// The wire/display name for a command, shared so the overlay's capability
/// strings and the policy engine's alert text cannot disagree.
pub fn capability_name(command: &CommandType) -> &'static str {
    match command {
        CommandType::Pause => "pause",
        CommandType::Resume => "resume",
        CommandType::Kill => "kill",
        CommandType::Redirect { .. } => "redirect",
        CommandType::InjectContext { .. } => "inject_context",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redirect() -> CommandType {
        CommandType::Redirect {
            instruction: "go".to_string(),
        }
    }

    #[test]
    fn a_proxy_agent_cannot_be_paused() {
        // The bug this module exists for: a pause rule matched a proxy
        // agent, the dispatch failed, and the cockpit suggested it anyway.
        assert!(!path_supports(IntegrationPath::Proxy, &CommandType::Pause));
    }

    #[test]
    fn a_proxy_agent_can_be_redirected_and_killed() {
        assert!(path_supports(IntegrationPath::Proxy, &redirect()));
        assert!(path_supports(IntegrationPath::Proxy, &CommandType::Kill));
        assert!(path_supports(
            IntegrationPath::Proxy,
            &CommandType::InjectContext {
                context: "c".to_string()
            }
        ));
    }

    #[test]
    fn a_log_agent_supports_nothing() {
        for c in [CommandType::Pause, CommandType::Kill, redirect()] {
            assert!(
                !path_supports(IntegrationPath::Log, &c),
                "log agents have no control channel, but {c:?} was allowed"
            );
        }
    }

    #[test]
    fn the_sdk_path_rules_nothing_out() {
        // The handshake narrows this per agent; the path itself must not.
        for c in [CommandType::Pause, CommandType::Kill, redirect()] {
            assert!(path_supports(IntegrationPath::Sdk, &c));
        }
    }

    #[test]
    fn proxy_capability_names_match_what_the_overlay_expects() {
        assert_eq!(
            path_capabilities(IntegrationPath::Proxy).unwrap(),
            vec!["redirect", "inject_context", "kill"]
        );
        assert!(path_capabilities(IntegrationPath::Log).unwrap().is_empty());
    }

    #[test]
    fn a_live_handshake_overrides_the_path_in_both_directions() {
        // It grants what the path would not...
        assert!(can_command(
            AgentReach::new(IntegrationPath::Proxy, Some(&["pause".to_string()])),
            &CommandType::Pause
        ));
        // ...and withholds what the path would have allowed.
        assert!(!can_command(
            AgentReach::new(IntegrationPath::Proxy, Some(&["pause".to_string()])),
            &CommandType::Kill
        ));
    }

    #[test]
    fn an_sdk_agent_with_no_live_stream_can_be_sent_nothing() {
        // The #296 case: spans arrive over OTLP so the path reads as Sdk,
        // but no control channel was ever opened. There is no wire.
        for c in [CommandType::Pause, CommandType::Kill, redirect()] {
            assert!(
                !can_command(AgentReach::new(IntegrationPath::Sdk, None), &c),
                "no channel exists, yet {c:?} was offered"
            );
        }
    }

    #[test]
    fn a_proxy_agent_needs_no_handshake_to_be_redirected() {
        // Proxy agents never handshake, so None must not mean "nothing".
        assert!(can_command(
            AgentReach::new(IntegrationPath::Proxy, None),
            &redirect()
        ));
        assert!(can_command(
            AgentReach::new(IntegrationPath::Proxy, None),
            &CommandType::Kill
        ));
        assert!(!can_command(
            AgentReach::new(IntegrationPath::Proxy, None),
            &CommandType::Pause
        ));
    }

    #[test]
    fn resume_rides_on_the_pause_declaration() {
        // An adapter that says it can pause is saying it can stop and
        // start; it does not have to list "resume" separately.
        assert!(can_command(
            AgentReach::new(IntegrationPath::Sdk, Some(&["pause".to_string()])),
            &CommandType::Resume
        ));
        assert!(!can_command(
            AgentReach::new(IntegrationPath::Sdk, Some(&["kill".to_string()])),
            &CommandType::Resume
        ));
    }

    #[test]
    fn an_sdk_agent_has_no_fallback_list() {
        // Not an empty list. Offering every row to an agent that has not
        // handshaked would be as wrong as offering none to a proxy.
        assert!(path_capabilities(IntegrationPath::Sdk).is_none());
    }
}
