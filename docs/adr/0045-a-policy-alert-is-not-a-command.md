# 0045: A Policy Alert Is Not a Command

**Status:** Accepted
**Date:** 2026-08-03

## Context

A policy rule carries two things at once. It has a condition worth
knowing about, and it has a command to run when that condition matches.
`builtin_low_health` means both "this run is going badly" and "pause
it". The two travelled together because for most agents they are both
available, and nothing forced them apart.

Two bugs forced them apart. In #274, a rule commanding `pause` matched a
proxy agent, which has no control channel to pause on. The dispatch
failed harmlessly and the cockpit offered the suggestion regardless, so
the intervention modal read `Suggested action: pause` directly above
`this agent does not support pause`. PR #295 fixed that by giving
`reeve-model` one answer to whether a path can carry a command, and
having the policy engine skip any rule it could not honor before that
rule alerted or dispatched.

Then #296 showed the same contradiction from a different direction. Every
span arriving on the OTLP receiver is recorded as `IntegrationPath::Sdk`,
whether or not that agent ever opens a control channel, so an agent
instrumented with plain OpenTelemetry passes the new check and still has
nowhere to receive a command.

Extending the check to know about live control channels would close #296
and leave the real problem in place. Skipping a rule discards a true
condition in order to avoid a false suggestion, and the condition was
never the part that was wrong. The cost is concrete: since #295, a proxy
agent whose health collapses produces no alert entry and no modal, only
a red health gauge, even though that agent can still be killed or
redirected. Something worth acting on stopped being reported because one
of the ways of acting on it was unavailable.

The failure is also invisible from the cockpit. A rule that is skipped
leaves no trace a user can see; someone who writes a `pause` rule for a
proxied agent watches it never fire and has only a debug log line to
explain why. That gets worse as integration paths multiply, because each
new path silently mutes more rules.

## Decision

A policy alert reports that a condition matched. That is the whole of
what it asserts, and it is raised whenever the condition is true.

The suggested command is an attachment to that alert, present only when
the target can actually run it. When it cannot, the alert still appears
and says so, instead of proposing an action the next line contradicts.

Capability is answered in one place and in this order. If the agent
holds a live control stream, the capabilities it declared in its
handshake are the answer. Otherwise `reeve_model::capability` answers
from the integration path. An agent on the SDK path with no live control
stream can be sent nothing at all, because no channel exists to send it
down; this is the case #296 reported, and it is why
`path_capabilities` returns `None` for that path rather than an empty
list. Unknown and incapable are different claims.

Rules are never skipped for capability reasons. Only suggestions are
withheld.

## Consequences

**What gets easier:**
- A rule that cannot be honored explains itself. It fires, the alert
  appears, and the alert says the agent is observe-only, instead of the
  rule vanishing with no visible reason.
- Reeve stays useful to anyone instrumenting with plain OpenTelemetry.
  They get scoring, alerting and history, and lose only the half of the
  product their setup cannot support.
- A new integration path only has to answer the capability question. It
  cannot accidentally mute rules, because muting is no longer something
  the capability answer can do.
- The renderer's overlay, the dispatcher and the policy engine finally
  read the same answer. #295 unified three copies of the path rule; this
  extends that to the live handshake, which the renderer already tracked
  and the engine could not see.

**What gets harder:**
- There are more alerts. Proxy agents raise alerts they have not raised
  since #295 merged, and anyone who read that quiet as an improvement
  gets it back.
- `PolicyAlert` carries an optional command rather than a required one,
  so every consumer has to handle its absence. The renderer grows a
  rendering for an alert that proposes nothing.
- Live handshake capabilities have to reach the engine, which does not
  depend on `reeve-intervention`. That means another piece of shared
  state threaded through `main.rs`, in the shape `ProxyInterventions`
  already established.
- An alert that proposes no action can read as noise. The wording has to
  carry why there is nothing to offer, or the alert becomes the new
  version of the contradiction it replaced.

## Alternatives considered

**Skip the rule entirely (rejected):** what #295 does today. It removes
the contradiction by removing the alert, which also removes a true
signal about an agent the operator can still act on by other means. It
is undiagnosable from the cockpit, and it makes Reeve decide on the
operator's behalf that they do not need to know something. For a tool
whose stance is that a human stays in the loop, choosing silence on
their behalf is the wrong instinct.

**Teach the path check about control channels, keep skipping (rejected):**
the smallest change that closes #296. It fixes the reported symptom and
leaves the structure that produced it, so the next integration path
brings the same class of bug back.

**Give OTLP-only arrivals their own integration path (rejected):**
appealing, because calling them `Sdk` is what let them through. But the
integration path describes how spans arrive, not what can be sent back,
and a new variant would not help an SDK agent whose control channel
dropped mid-run. That agent is equally unreachable and would still be
labelled `Sdk`. The distinction that matters is liveness, not shape.

**Keep the suggestion, mark the agent observe-only in the fleet list
(rejected):** the operator would learn once rather than per alert, and
it is the least code. It also leaves the contradictory modal exactly as
it is, which is the thing both #274 and #296 are actually about.
