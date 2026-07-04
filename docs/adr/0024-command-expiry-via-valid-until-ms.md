# 0024: Command Expiry via `valid_until_ms`

**Status:** Accepted
**Date:** 2026-07-04

## Context

Intervention commands issued by the policy engine can sit in the
dispatcher's pending queue for meaningful periods before an ack arrives.
A Pause command issued during a cost spike is not useful thirty minutes
later when the agent has already finished. The dispatcher needs a
principled way to stop tracking commands that were never acknowledged
in time, and agents need to know whether to act on a command they
receive late.

## Decision

Every `InterventionCommand` carries a `valid_until_ms` field: a Unix
millisecond timestamp past which the command should be discarded.

The dispatcher checks this field in two places:

**At dispatch time.** If `valid_until_ms` is already in the past when
`dispatch()` is called, the command is logged as EXPIRED and discarded
without sending. No proto message reaches the agent. This covers the
case where the policy engine queues a command during a burst and the
dispatcher processes it late.

**During the ack timeout scan.** Every five seconds, the dispatcher
scans pending commands. Any command that has been pending for thirty
seconds without an ack gets one retry. If `valid_until_ms` has already
passed by the time the retry check runs, the retry is skipped and the
command is settled as EXPIRED immediately.

The field is also sent on the wire inside `InterventionCommand`. The
agent SDK must check it before applying any command it receives. This
is a defense-in-depth measure: network delay or a slow agent loop
could deliver a command after it was supposed to expire.

**Default expiry.** The policy engine sets `valid_until_ms` to
`now + 300_000` (five minutes) for all built-in rules. This matches
the per-agent cooldown window: a command that has not been acknowledged
within the cooldown period is not worth retrying.

## Consequences

**What gets easier:**
- The dispatcher's pending map is bounded in practice. Commands expire
  and are removed within five minutes even if an agent never acks.
- Agents with a slow loop do not apply commands that were relevant
  thirty seconds ago but no longer are.
- The audit log has a clear EXPIRED record for every command that
  timed out, making post-incident analysis straightforward.

**What gets harder:**
- The policy engine must set `valid_until_ms` on every command it
  creates. Forgetting to set it (or setting it to 0) expires the
  command immediately on dispatch.
- Human-issued commands from the UI also need a `valid_until_ms`.
  The intervention overlay (#61) must set a sensible default at the
  point of dispatch. A reasonable default for UI-issued commands is
  `now + 60_000` (one minute): if the agent has not acked in a minute
  the operator can re-issue.

## Alternatives considered

**TTL in seconds on the domain struct (rejected):** A countdown value
requires knowing when the timer starts to compute the deadline. An
absolute millisecond timestamp makes the deadline unambiguous regardless
of when the struct was created or how long it sat in a queue.

**No expiry, rely on manual cancellation (rejected):** Commands that
never ack would accumulate in the pending map indefinitely. An agent
that disconnects without sending a final ack would leave its commands
stuck forever.
