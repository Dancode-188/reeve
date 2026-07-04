# 0025: Audit Trail Format and `issued_by` Attribution

**Status:** Accepted
**Date:** 2026-07-04

## Context

Every intervention command that Reeve dispatches to an agent is a
consequential action. A Pause stops the agent mid-task; a Kill
terminates it. There needs to be a permanent record of what was sent,
when, by whom, and what the agent did with it. This matters for
debugging when an intervention misfires, and for accountability when
an autonomous policy rule triggers an action without a human in the
loop.

Two separate questions that look similar but have different answers:

1. **Where does the record live?** The warm store already holds
   `InterventionCommand` rows with status updates on each ack. But the
   warm store is a relational database optimized for querying, not for
   sequential audit reconstruction. Reads and writes compete with the
   normal ingestion and evaluation paths.

2. **How is human vs. automated dispatch distinguished?** The policy
   engine fires commands under rule IDs. The UI fires commands in
   response to a keypress. Both end up in the same dispatcher, and the
   downstream record needs to tell them apart.

## Decision

**Audit trail:** An append-only flat file at
`~/.local/share/reeve/audit.log`, one event per line. The dispatcher
holds the file open (wrapped in `Arc<Mutex<File>>`) and writes to it
synchronously before any async warm store operation. The file is never
rotated by Reeve itself.

Line format: space-separated key=value tokens, epoch milliseconds as
the first field:

```
1751664000000 DISPATCH cmd=abc123 agent=agent-1 type=Pause by=human status=delivered
1751664000012 ACK cmd=abc123 agent=agent-1 status=received
1751664000480 ACK cmd=abc123 agent=agent-1 status=applying
1751664001100 ACK cmd=abc123 agent=agent-1 status=applied
1751664060000 EXPIRED cmd=def456 agent=agent-2 type=Redirect by=policy:builtin_high_cost reason=ack_timeout
```

Event types: `DISPATCH`, `ACK`, `RETRY`, `EXPIRED`.

The file is written even if the warm store write fails. This means the
audit log is always ahead of or equal to the database state. The warm
store holds the last known status for queries; the audit log holds the
full event history for reconstruction.

**Attribution:** The `issued_by` field on every `InterventionCommand`
carries one of two shapes:

- `"human"` — dispatched directly from the intervention overlay by
  the developer pressing a key.
- `"policy:<rule_id>"` — fired automatically by the policy engine. The
  `rule_id` component identifies which rule triggered it (e.g.,
  `"policy:builtin_high_cost"` or `"policy:user_defined_rule_42"`).

The shape is a free string field on the domain struct, not an enum.
This keeps the policy engine and UI decoupled from the dispatcher.
Neither needs to know the dispatcher's internal representation of
attribution; both just set the string before handing the command over.

The audit log writes `by=<issued_by>` verbatim, making human vs.
policy-auto actions immediately distinguishable in a text search.

## Consequences

**What gets easier:**
- Reconstructing the full event sequence for any command requires only
  `grep cmd=<command_id> ~/.local/share/reeve/audit.log`. No SQL.
- A corrupted or missing warm store does not destroy the audit history.
- The attribution model is extensible: a future headless mode could
  use `issued_by=scheduled:<schedule_id>` without changing the
  dispatcher.

**What gets harder:**
- The audit file grows unboundedly. Tools like `logrotate` can manage
  it, but Reeve ships no rotation policy of its own.
- Audit entries are written with the dispatcher's wall clock, not the
  agent's clock. Clock skew between Reeve and the agent is not
  reflected in the audit log. NTP offset (ADR-0026) corrects the
  display clock for spans but not for intervention timestamps.

## Alternatives considered

**Write audit records only to the warm store (rejected):** The warm
store holds the current status of each command, not a time-ordered
event log. Reconstructing "what happened to command X" would require
reading every status transition, which is not how the schema is
structured. A flat file with one line per event is a better fit for
sequential audit reconstruction.

**Structured JSON per line (rejected):** JSON is machine-readable but
harder to grep and read in a terminal. Space-separated key=value pairs
are unambiguous, easy to parse with awk, and readable without tooling.
The format is not a public API. Reeve can change it if a future
structured export is needed.

**Enum for `issued_by` (rejected):** An enum would require the policy
engine and UI to import a dispatcher type just to set the attribution
field. A string field keeps the two callers ignorant of each other and
avoids a dependency on `reeve-intervention` from `reeve-engine`.
