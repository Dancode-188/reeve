# 0042: Per-Agent Daily Budgets Wire Cost to the Breaker

**Status:** Accepted
**Date:** 2026-07-12

## Context

Four pieces existed and none connected. The engine predicts a trace's
final cost mid-run by extrapolating its cost rate. A built-in policy rule
fires against a hardcoded predicted-cost threshold. The proxy breaker
(ADR-0039) stops a proxy agent's spending unconditionally. And a killed
agent is now visible in the fleet and revivable with one keystroke
(ADR-0039 addendum).

What was missing was the sentence a developer actually wants to say:
this agent may spend five dollars today, and if it will not stay under
that, stop it. A hardcoded threshold is not that sentence, and a rule
that fires on absolute predicted cost per trace says nothing about a
day's accumulated spend across many traces.

## Decision

**A budget is a per-agent daily spend cap, set in config.** The
`[budgets]` section carries a `default_daily` that applies to every agent
and a `per_agent` table that overrides it by agent id. Caps are read once
at startup, the same way rules and the privacy tier are: a budget is a
standing intent, not something that should shift under a running session.

**A zero or negative cap is unbudgeted, not a wall of zero.** A stray
`0.0` resolving to "stop every request immediately" is a footgun; the
only safe reading of an absent or zero cap is no budget. So the resolver
filters non-positive caps to none.

**The day is the local calendar day.** A daily budget a developer sets is
about their day. Resetting at UTC midnight would surprise someone whose
budget refills in the middle of their afternoon. Spend is bucketed by the
local ordinal day and forgotten on the first activity of the next one; no
timer sweeps at midnight, because a budget only matters when spending is
happening and that is exactly when the next tick arrives.

**Predicted spend triggers the stop, not only settled spend.** Waiting
for a trace to finish before counting its cost means the stop always
lands one runaway trace too late. So the mid-trace check folds the
predicted final cost of the in-flight trace into the day's settled total,
and crosses the cap before the money is gone. The prediction carries a
stated 25 to 40 percent error, which is why the warn threshold sits at 80
percent of the cap: the warning gives room, and the cap is approximate by
construction. Settled spend is what the cockpit's bar shows; the
prediction only moves the stop earlier, it does not inflate the number on
screen.

**The stop is the same action on both paths, with an honest label on the
guarantee.** Crossing the cap fires an unconfirmed kill through the exact
policy-to-dispatcher path a rule uses. On the proxy path that engages the
breaker: the budget is a hard ceiling, the agent cannot spend another
token, and it comes back when the operator raises the cap and revives it.
On the SDK path the kill is cooperative: the budget is a best-effort
stop, requested and honored at the agent's next checkpoint. Both fire the
same kill; the cockpit says which guarantee applies rather than dressing
the SDK stop up as a wall it is not. This mirrors how pause is honest
about being absent on the proxy path: same action everywhere, honest
about what it can promise.

**A crossing speaks once, not every tick.** The engine remembers where
each agent last sat against its cap. Entering the warn band raises one
ALERTS entry; crossing the cap raises one more and fires the kill.
Staying over does not re-alert or re-fire against an already-engaged
breaker. Only a transition speaks, so a long over-budget run does not
bury the panel or spam dead-letter kills.

## Consequences

**What gets easier:**
- The kill-switch a developer can reason about ships: a dollar figure in
  a config file, enforced without a line of agent instrumentation on the
  proxy path.
- The budget bar makes the ceiling visible before it is hit, so the stop
  is expected rather than a surprise.

**What gets harder:**
- The cap is approximate on the proxy path by a trace's final cost and on
  both paths by the prediction's error. A budget is a guardrail, not an
  accountant; the warn threshold is where a developer who needs a hard
  number should set their real limit.
- Two cost numbers now live near each other in the COST section: the
  session total and the day's budget spend. They measure different things
  and can differ, which is a small readability cost paid for showing the
  ceiling.

## Alternatives considered

- **Warn only, never stop, on both paths.** Amputates a working lever: the
  breaker already stops a proxy agent cold, and refusing to fire it from a
  budget throws that away to avoid admitting the SDK stop is weaker. The
  honest-label approach keeps the strong guarantee where it exists.
- **Count only settled spend.** Simpler, but the stop always lands after
  one more runaway trace has already spent. Folding in the prediction is
  the whole point of stopping a runaway.
- **Reset at UTC midnight.** One less local-time consideration, at the
  cost of a budget that refills at a time unrelated to the developer's
  day. The daily budget is a human-scale intent and should track a human
  day.
- **Persist the day's spend across restarts.** A restart is a deliberate
  act, and a budget that survives it would let a forgotten cap silently
  throttle a fresh session. In-memory matches the breaker's own
  restart-clears rule and keeps the mental model one thing.
