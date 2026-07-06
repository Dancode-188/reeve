# 0029: Engine-to-Dispatcher Channel to Preserve the `reeve-engine` Dependency Boundary

**Status:** Accepted
**Date:** 2026-07-06

## Context

When a policy rule fires and does not require confirmation, `reeve-engine`
needs to deliver the resulting `InterventionCommand` to `reeve-intervention`'s
`Dispatcher`. The most direct way to do this is to add `reeve-intervention`
as a dependency of `reeve-engine` and call the dispatcher directly.

The problem is what comes with that dependency. `reeve-intervention` compiles
a protobuf schema, links tonic, and generates gRPC stubs at build time. Pulling
it into `reeve-engine` would add all of that to every test run for the engine
and to every build of any crate that depends on it. The engine's tests currently
run in under 100ms. That matters for iteration speed.

There is also a conceptual issue. `reeve-engine` is responsible for evaluation
and policy decisions. It produces `InterventionCommand` records as output; it
has no business knowing how those commands are transmitted to agents. The
transport layer belongs to `reeve-intervention`. Keeping that boundary clean
is the same rationale that produced ADR-0013 and ADR-0019.

## Decision

`reeve-engine` exposes a public type alias for an `mpsc::Sender` carrying
`(AgentId, InterventionCommand)` pairs. The engine's `run()` function accepts
this sender as an `Option`, allowing callers to wire it up or leave it absent.

When a rule fires without requiring confirmation, the engine sends the agent ID
and command on the channel. `main.rs` creates the channel, passes the sender to
the engine, and spawns a receiver task that calls `dispatcher.dispatch()` for
each arriving pair. The engine never imports or references the dispatcher type
directly.

Rules with `requires_confirmation: true` bypass the channel entirely. They are
written to the warm store as `PendingConfirmation` and surfaced to the renderer
via a `PolicyAlert` event. The renderer handles dispatch after the operator
confirms or the auto-confirm countdown expires.

The `Option` wrapper keeps the engine functional in test contexts where no
dispatcher is wired up. Existing engine tests required no changes.

## Consequences

**What gets easier:**
- Engine tests stay fast. No tonic, no protobuf codegen, no gRPC stubs.
- The engine's contract is clear: it produces commands; it does not transmit
  them. A future refactor of the transport layer does not touch the engine.
- The channel provides natural backpressure. If the dispatcher falls behind,
  the engine's send blocks rather than silently dropping commands.

**What gets harder:**
- Two steps are needed to complete an auto-dispatch: the engine puts the
  command on the channel, then the receiver task calls the dispatcher. A panic
  or shutdown in the receiver task causes the channel to close and the engine
  logs a warning rather than delivering the command. This is visible in the
  log but not in the audit trail.
- The `AgentId` must be available at the policy evaluation site. Both the
  `TraceCompleted` and `SpanCompleted` event handlers already have `agent_id`
  in scope, so this constraint is currently met.

## Alternatives considered

**Add `reeve-intervention` as a direct dependency of `reeve-engine` (rejected):**
The direct approach works but carries tonic and prost into every engine
build and test run. The dependency boundary between evaluation logic and
transport is worth keeping explicit.

**Define a `CommandDispatcher` trait in `reeve-model` (rejected):** A trait
object would also keep the engine free of `reeve-intervention`. But it adds
an abstraction that has exactly one implementation and is never expected to
have a second. A channel is simpler and requires no trait definition, no
dynamic dispatch, and no `reeve-model` changes.

**Route through the engine event bus (rejected):** Emitting a new
`EngineEvent::DispatchCommand` variant and having the renderer pick it up
would work, but it puts transport responsibility on the renderer. The renderer
is the wrong place: it is already responsible for UI state, and it has no
guarantee of processing engine events before the command expires.
