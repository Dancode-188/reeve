# 0030: Reconstituting `Instant` Values from Wall-Clock Timestamps for Cooldown Persistence

**Status:** Accepted
**Date:** 2026-07-06

## Context

`PolicyEngine` tracks per-rule cooldowns using a map from `(AgentId, RuleId)`
to `Instant`. The `Instant` type in Rust is a monotonic clock reading. It is
not serializable, not comparable across processes, and has no defined
relationship to wall-clock time. It exists only to measure elapsed time within
a single process lifetime.

Cooldowns need to survive server restarts. A rule that fires at 14:00 with a
300-second cooldown should not be allowed to fire again at 14:02 just because
the server restarted at 14:01. The only way to persist a cooldown is to record
it in terms of wall-clock time, then convert back to an `Instant` when the
server starts again.

## Decision

The `policy_cooldowns` table stores two unix millisecond timestamps per
`(agent_id, rule_id)` pair: `last_fired_at` and `expires_at`. `expires_at` is
computed at write time as `last_fired_at` plus the rule's cooldown window in
milliseconds. The DB query at startup filters to rows where `expires_at` is
still in the future, so expired cooldowns never enter the reconstitution step.

For each non-expired row, the engine computes how many milliseconds ago the
rule fired in wall-clock terms, then subtracts that duration from the current
monotonic clock reading. This produces a synthetic `Instant` that, when tested
against the cooldown window later in the same process, behaves identically to
the original. The cooldown check in `evaluate()` operates on `Instant` values
without any awareness of the storage layer.

The elapsed duration is clamped to a 24-hour maximum before the subtraction.
This handles two edge cases: a negative elapsed time when `last_fired_at` is
in the future due to clock skew, and a value large enough to underflow the
monotonic clock. The DB filter already ensures legitimate elapsed times are
bounded by the cooldown window, which is at most a few thousand seconds for
any configured rule.

Cooldowns are written to the database on every rule fire rather than on
shutdown. This makes the persistence crash-safe: a kill signal does not create
a window where the cooldown was fired in memory but not persisted.

## Consequences

**What gets easier:**
- The cooldown enforcement logic in `evaluate()` is unchanged. It still works
  entirely with `Instant` values and knows nothing about the database.
- Cross-restart correctness is guaranteed by the DB filter: only non-expired
  rows are loaded, so a cooldown that expired during the restart window is
  simply absent and the rule fires normally on the next trace.
- The reconstitution is deterministic. Given the same `last_fired_at` and the
  same current time, it always produces an `Instant` that behaves identically
  under the elapsed-time check.

**What gets harder:**
- The reconstituted `Instant` is a synthetic value. It does not correspond to
  any real monotonic clock reading from this or any previous process. Debugging
  code that formats `last_fired` as a duration since process start will see a
  value that reflects wall-clock elapsed time, not process uptime.
- The formula assumes the system clock runs forward between restarts. If the
  wall clock is stepped back by NTP or manual adjustment, a cooldown that
  should have expired may appear active. The 24-hour clamp prevents a
  stepped-back clock from causing a panic, but it does not correct the logical
  state.

## Alternatives considered

**Store only `expires_at`, not `last_fired_at` (rejected):** With only
`expires_at` the reconstitution requires knowing how long the original cooldown
window was in order to compute how much of it remains. That forces the query to
join against `policy_rules`, or the cooldown table to store the window duration
as a redundant column. Storing `last_fired_at` directly keeps the query simple
and self-contained.

**Store cooldowns as remaining milliseconds, written at shutdown (rejected):**
Computing the remaining window at shutdown and storing it directly would work
if shutdown were clean and reliable. It fails completely on a crash or kill
signal. The write-on-fire approach is crash-safe by design.

**Replace `Instant` with a wall-clock type throughout `PolicyEngine` (rejected):**
Using `SystemTime` for cooldown tracking would make the state directly
serializable. But `SystemTime` is not monotonic: it can go backwards. A policy
engine that suppresses interventions based on a backwards-going clock is worse
than one that occasionally fires an extra time. The wall-clock-to-monotonic
conversion at the storage boundary is the right place to absorb the impedance
mismatch, not inside the engine itself.
