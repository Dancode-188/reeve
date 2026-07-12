# 0039: Kill on the Proxy Path Is a Circuit Breaker

**Status:** Accepted
**Date:** 2026-07-10

## Context

ADR-0038 established that redirect and inject-context apply on a proxy
agent's next request by modifying its body. Kill does not fit that shape:
there is no body to modify toward "stop," and appending a message asking
the agent to stop trusts an agent that may be in a runaway loop precisely
because it stopped listening.

But the proxy holds something the SDK path never does: every one of the
agent's tokens flows through it. That inverts the usual weakness. SDK
kill is cooperative, the command travels to the agent and waits for a
checkpoint; a proxy agent's kill can be enforced without the agent's
cooperation at all.

## Decision

**Kill on the proxy path is a circuit breaker: the proxy refuses to
forward the agent's subsequent Messages requests.** Killing engages a
breaker in the shared intervention state, and the proxy returns a clean
API error to any Messages request from a killed agent instead of
forwarding it. The agent cannot spend another token no matter what its
loop does.

**Application is reported immediately.** Unlike redirect, which waits for
the next request to modify, the breaker is effective the moment it is
set: enforcement is local to the proxy, not delegated to the agent. So
the dispatcher reports the kill applied at once and folds it into the
same ack handling every other command uses.

**Only Messages requests are refused.** That is where tokens are spent;
other endpoints (model listing, counting) pass through, so the client
degrades toward "this model is unavailable" rather than "the network is
gone."

**The refusal names itself.** The error says an operator killed the agent
via Reeve and that access resumes on Reeve restart, so a developer
reading the agent's logs sees a deliberate action, not a mysterious
outage.

**The breaker clears on restart.** It lives in memory only. A kill is a
stop-now, not a permanent ban; persisting it across restarts would turn
a heat-of-the-moment intervention into a config file nobody remembers
editing.

**Proxy agents are killable in any state.** The SDK killable check
requires Running or Paused, because killing a finished trace is
meaningless there. A proxy agent between requests reports idle, and the
breaker's entire purpose is stopping the *next* request, so the check
exempts proxy agents.

Because the dispatcher routes policy-issued commands through the same
path as human ones, a policy rule firing kill against a proxy agent
engages the breaker with no extra wiring: a predicted-cost rule can stop
a runaway session at the budget line with zero agent instrumentation.

**Addendum (2026-07-12):** the breaker now clears two ways instead of
one. Restart still clears it, and an operator Resume against a killed
agent revives it: the breaker state is visible in the fleet as a
`[killed]` marker, and the intervention overlay offers Revive where
Kill stood. This extends the original decision rather than reversing
it; what was rejected was persistence across restarts, not recovery.
A guard that autonomous policy can fire must be visible and
reversible, or operators will not trust it with a budget.

## Consequences

**What gets easier:**
- Kill actually works on the proxy path, and works better than the SDK
  path: no cooperation required, effective immediately.
- The autonomous budget kill-switch falls out for free: policy plus cost
  prediction plus this breaker stops a runaway before it drains an
  account, with nothing added to the agent.

**What gets harder:**
- A killed agent that keeps retrying sees repeated refusals rather than a
  graceful shutdown; a well-behaved client treats the error as terminal,
  a badly-behaved one busy-loops against a wall. The wall holds either
  way, which is the point.
- The breaker is per Reeve process: kill on one Reeve does not stop an
  agent pointed at a different proxy. Acceptable, since the developer
  killing it is the one running the proxy it uses.
- Restart clears the breaker, so an agent killed just before a Reeve
  restart resumes. A restart is a deliberate act by the same operator;
  re-killing is one keystroke.

## Alternatives considered

- **Inject a "stop now" message like redirect.** Trusts the agent to
  comply, which is exactly the trust a kill exists to withdraw.
- **Return a 500 or drop the connection.** Reads as an outage; the
  client retries and the developer debugs a phantom network problem. A
  named permission error is honest and stops the retry loop faster.
- **Persist the breaker across restarts.** Turns a live intervention
  into durable state that outlives its intent; the SDK kill is not
  durable either, and matching that keeps the mental model one thing.
