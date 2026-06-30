# 0022: evalexpr for Policy Rule Conditions

**Status:** Accepted
**Date:** 2026-06-30

## Context

The policy engine evaluates rules against trace state after each Tier 1
evaluation pass. A rule fires when a condition is met, issues an
`InterventionCommand` to the warm store, and emits a `PolicyAlert` event
so the renderer can surface it.

The central design question is how conditions are expressed. The simplest
option is hardcoded Rust predicates with no dynamic surface at all. The
ceiling is a full logic programming language. Between those is a
family of expression evaluators that accept a string condition at runtime
and return a boolean.

Hardcoded predicates are good for the three default rules but collapse the
moment a developer wants to adjust a threshold or add a rule without
recompiling. Reeve's long-term trajectory includes user-defined rules stored
in the warm tier. The condition evaluation mechanism has to be capable of
handling an arbitrary string expression that was not known at compile time.

The condition language needs to be:
1. Safe to evaluate on untrusted or user-supplied strings.
2. Fast enough to run per-trace without measurable overhead.
3. Accessible to developers who are not familiar with formal logic.

## Decision

Use `evalexpr` as the policy DSL. `evalexpr` is a pure-Rust arithmetic and
boolean expression evaluator with no FFI and no external runtime. It accepts
standard infix notation (`health_score < 30 && cost_usd > 5.0`) which is
immediately readable to anyone familiar with C-style syntax.

A `PolicyContext` struct wraps an `evalexpr::HashMapContext` populated from
Tier 1 evaluation results. Keys available in every condition:

- `health_score`: composite score on [0, 100]
- `cost_usd`: trace cost in US dollars
- `span_count`: number of spans as a float
- `tier2_pending`: boolean, true when Tier 2 metrics have not arrived
- `weight_coverage`: sum of active metric weights in [0.0, 1.0]
- `<metric_name>`: individual metric score in [0.0, 1.0] for each Tier 1
  metric that produced a result on this trace

An invalid or malformed condition evaluates to `false` and logs a warning.
No panic, no error propagation. The default rules are never user-supplied
and are known valid. User-defined rules that produce warnings will be
surfaced in a future validation step before they are persisted.

**Policy fires on Tier 1 results only.** Tier 2 evaluation runs
asynchronously after policy has already executed. Re-triggering after Tier 2
would fire duplicate `PolicyAlert` events for the same trace. The Tier 1
health score is the authoritative trigger input; Tier 2 refines it for
display purposes.

**Three hardcoded default rules:**

`builtin_low_health` fires when `health_score < 30`. The threshold of 30
represents a composite score where the weighted combination of available
metrics falls below a level consistent with reliable agent behavior. It
issues a `Pause` command requiring human confirmation.

`builtin_high_cost` fires when `cost_usd > 5.0`. A single trace exceeding
five dollars is anomalous for most development workloads and warrants
attention before the agent continues. It issues a `Pause` command requiring
human confirmation.

`builtin_loop_detected` fires when `loop_detection < 0.5`. The loop
detection heuristic is designed so that a score below 0.5 represents a
trace with meaningful repetition. It issues a `Pause` command requiring
human confirmation.

All three default rules carry a 300-second per-agent cooldown to prevent
alert flooding when an agent is consistently producing problematic traces.
Cooldown state lives in memory and resets on restart. Persistent cooldown
state would require a storage migration and is deferred.

**Command identity.** Each fired rule produces one `InterventionCommand`
with an ID of the form `{rule_id}:{trace_id}`. This makes commands
idempotent within a trace: re-evaluating the same rule on the same trace
produces the same command ID, which the warm store's `INSERT` will reject
as a duplicate rather than creating a second pending command.

## Consequences

**What gets easier:**
- Policy conditions are human-readable strings, not compiled code.
- The evaluation path from `TraceCompleted` to `PolicyAlert` is under a
  millisecond when Ollama is not involved.
- User-defined rules stored in the warm tier can be evaluated without
  any changes to the engine. The context shape is the stable interface.
- The `evalexpr` sandbox makes it safe to evaluate strings from the
  database without sandboxing at the OS level.

**What gets harder:**
- Conditions that reference a metric not present in the current context
  evaluate to `false` silently (the metric may simply not have been
  available on this trace, or the condition is wrong). A validation step
  before persisting user-defined rules would help. Deferred to when
  user-defined rules are introduced.
- Cooldown state is in-memory. A process restart clears all cooldowns.
  During startup, a burst of problematic traces from the same agent can
  fire rules on every trace before the 300-second window refills. This is
  acceptable for v0.2.0.

## Alternatives considered

**Lua scripting (rejected):** Lua is the standard embedded scripting
choice for similar use cases. It offers loops, functions, and state. For
boolean conditions on a flat key-value context, that power is unnecessary.
Lua requires an FFI layer (`mlua`, `rlua`) which introduces build
complexity and a C runtime dependency.

**Custom DSL (rejected):** Building a purpose-written parser for
conditions like `health_score < 30` is straightforward but amounts to
re-implementing evalexpr without its test coverage or community maintenance.
Any custom DSL starts with a parsing bug backlog.

**Datalog / Prolog-style rules (deferred):** Formal rule engines enable
reasoning about rule interactions and can detect conflicts between rules.
For three default rules operating on independent thresholds, that power
is not needed. A future version with user-defined rules and rule priorities
may revisit a formal logic approach.
