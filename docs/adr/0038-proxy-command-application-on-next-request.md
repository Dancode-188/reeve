# 0038: Proxy Commands Apply on the Agent's Next Request

**Status:** Accepted
**Date:** 2026-07-10

## Context

Agents on the control channel receive commands pushed to them and
acknowledge each step back. Proxy-path agents have no channel to push
through: the proxy only ever sees them when they make a request. Redirect
and inject-context still need to work there, because steering an agent
you can watch is the cockpit's core promise.

Three questions needed answers: how a command reaches a proxy agent, what
"applied" means without an acknowledging SDK, and how the injected
content coexists with conversation threading, which fingerprints request
bodies.

## Decision

**Commands queue until the agent's next request.** The dispatcher, on
finding the target has no control stream and its integration is the
proxy path, queues redirect and inject-context commands in a shared
structure instead of failing them, audited as delivered with the channel
named. When the agent's next Messages request arrives, the proxy drains
its queue and appends one operator message per command to the request
body, most recent last, then forwards.

**Applied means injected into a forwarded request.** The proxy reports
each application through the shared structure, and the dispatcher folds
those reports into the same ack handling the control channel uses, so
the audit trail, pending bookkeeping, and outcome measurement cannot
tell the channels apart. This is a weaker guarantee than an SDK ack: the
model received the instruction, but nothing confirms the agent acted on
it. Outcome measurement exists precisely to answer that second question.

**Expiry still governs.** Application waits for a request that may never
come, so the proxy drops expired commands at drain time and the
dispatcher's expiry loop owns the audit line, exactly as on the control
channel.

**The original body is fingerprinted, the modified body is forwarded.**
Threading hashes the request as the client sent it, before injection.
The client never resends what it never sent, so the next request's
prefix still matches and threading is undisturbed by design rather than
by care.

**Pause stays absent on this path.** Holding requests hostage to
simulate a pause risks client timeouts that read as outages; a
capability that misbehaves is worse than one that is honestly missing.

## Consequences

**What gets easier:**
- Proxy agents are steerable: redirect and inject-context work with zero
  agent-side integration, which is the milestone's promise extended from
  watching to acting.
- Downstream bookkeeping needed no changes: applications enter through
  the existing ack path.

**What gets harder:**
- Latency between issuing and application is unbounded: a command
  applies when the agent next calls, which may be seconds or never.
  Expiry bounds the damage, not the wait.
- The injected message reaches the model as user-role content, so a
  sufficiently instructed model could distinguish it from the real user.
  Accepted: the Messages API offers no operator channel, and user-role
  text is what the path supports.
- An agent that reuses another tool's User-Agent would receive its
  commands; identity remains as good as ADR-0036's derivation.

## Alternatives considered

- **Fail proxy-agent commands as not connected (status quo).** Honest
  but a dead end; the position in the request path is sufficient for
  these two commands, and wasting it makes proxy agents watch-only.
- **A system-prompt modification instead of a user message.** Rewriting
  the agent's own system prompt mid-conversation would invalidate the
  client's prompt cache and risks fighting the tool's own scaffolding;
  an appended user message is visible, cheap, and reversible.
- **Holding the request until a developer confirms application.** Turns
  every intervention into a latency spike the client cannot explain;
  rejected for the same reason as hold-based pause.
