# 0036: Proxy Agent Identity Derived from the User-Agent Product Token

**Status:** Accepted
**Date:** 2026-07-08

## Context

Agents on the OTel path declare who they are: `service.name` and
`service.instance.id` arrive as resource attributes, and the agent id is
derived from both. Traffic through the HTTP proxy carries no such
declaration. The proxy must name the agent from what an unmodified HTTP
client actually sends, because requiring configuration would defeat the
path's promise of zero instrumentation.

## Decision

The agent name is the User-Agent header's product token: the first
whitespace-separated word, truncated at the first slash. `claude-cli/2.1.0
(external, cli)` names the agent `claude-cli`; `curl/8.5.0` names it
`curl`. A missing or empty User-Agent falls back to `proxy-agent`. The
`REEVE_PROXY_AGENT_NAME` environment variable overrides derivation
entirely, for clients with unhelpful User-Agents or developers who want
two instances of the same tool distinguished.

The instance id is the constant `proxy`, so all traffic from one client
tool aggregates into one agent. The framework field is `proxy`, and the
synthesized agent carries `IntegrationPath::Proxy`, which is what lets the
cockpit state the path's reduced capabilities instead of guessing.

## Consequences

**What gets easier:**
- Pointing a tool at the proxy is genuinely zero-configuration: the tool
  appears in the fleet under a recognizable name with nothing declared.
- The cockpit can distinguish proxy agents structurally (the integration
  field) rather than by naming conventions.

**What gets harder:**
- Two different tools with the same product token collapse into one
  agent, as do two concurrent instances of the same tool. The override
  variable is the escape hatch until real usage shows whether finer
  identity (port, header fingerprint) is worth its complexity.
- Identity is only as good as the client's User-Agent discipline. A
  client that sends none becomes `proxy-agent`, and several such clients
  become one indistinguishable agent.

## Alternatives considered

- **Require a configured name.** Honest identity, but the proxy's entire
  premise is that an uninstrumented tool appears with no setup; a
  mandatory config step reintroduces the friction the path exists to
  remove.
- **Derive from the API key.** The key is the one stable per-caller
  value, but using credential material as an identity input contradicts
  the hygiene rule that the key is never inspected, logged, or persisted;
  even a hash invites questions no monitoring tool should raise.
- **Client address and port.** Ephemeral ports change per connection,
  producing agent churn; the address is nearly always localhost.
