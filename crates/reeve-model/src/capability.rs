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
    fn an_sdk_agent_has_no_fallback_list() {
        // Not an empty list. Offering every row to an agent that has not
        // handshaked would be as wrong as offering none to a proxy.
        assert!(path_capabilities(IntegrationPath::Sdk).is_none());
    }
}
