# 0027: `checkpoint()` as a Pull-Based Polling Primitive

**Status:** Accepted
**Date:** 2026-07-05

## Context

Reeve needs to deliver commands to a running agent at safe points in
its execution. Pause, Redirect, Kill, and InjectContext all carry the
same requirement: the command must not interrupt the agent mid-operation.
A Kill that fires while the agent is mid-database-write, or a Redirect
that fires between two steps of a multi-part tool call, can leave
external systems in an inconsistent state.

The SDK also needs to be simple enough for developers to drop into an
existing agent loop in a few lines, and testable without a live Reeve
instance.

## Decision

`ReeveSdk::checkpoint()` is a function the agent calls explicitly at
safe yield points. It is not a background task, a signal handler, or a
callback. The agent decides when it is interruptible.

The signature is `async fn checkpoint(&self) -> Result<CheckpointResult,
AgentError>`. Pending commands are stored in `Arc<Mutex<Option<
PendingCommand>>>`. One command is held at a time; a newer command from
the control stream overwrites the previous one before the agent reaches
its next checkpoint.

`CheckpointResult` has three variants:

- `Continue` — nothing pending, keep going.
- `Redirect(String)` — a redirect instruction arrived; the agent should
  use it to alter the next step.
- `Context(String)` — context JSON arrived; the agent should merge it
  into the next prompt.

Kill is modeled as `Err(AgentError::Killed)` rather than an `Ok`
variant. This forces the caller to handle Kill through the error path,
which in Rust means the `?` operator propagates it up the call stack
automatically. An agent that forgets to check `CheckpointResult` after
a successful return will still be stopped by a Kill because the error
forces acknowledgement.

Pause blocks inside `checkpoint()` on a `tokio::sync::Notify`. The
function loops after Resume returns from `notified()` because another
command may have arrived while the agent was paused. Resume itself is
not surfaced as a `CheckpointResult` variant; it is handled internally
and the loop continues.

## Consequences

**What gets easier:**
- The agent controls exactly when it is interruptible. No external
  entity can interrupt a mid-operation sequence.
- Tests inject commands by setting the `pending` mutex directly, with no
  live gRPC connection needed. The existing test suite covers Continue,
  Redirect, InjectContext, Kill, and ack delivery without a server.
- SDK authors in other languages can implement the same pattern: a
  polling function, a pending slot, and a notify primitive.

**What gets harder:**
- A stalled or sleeping agent will not receive commands until it calls
  checkpoint again. Reeve can detect this via the heartbeat gap but
  cannot force delivery.
- Only one command is held pending at a time. If the policy engine fires
  two commands in quick succession before the agent checks, only the
  second survives. This is intentional: stacked commands from an
  automated policy are more likely to cause confusion than to help.

## Alternatives considered

**Signal-based interrupts (rejected):** OS signals (SIGURG or SIGUSR1)
can interrupt a running thread. But Rust async runtimes run agent code
on a thread pool, and signal delivery to a specific task requires unsafe
code and OS-specific machinery. The complexity is out of proportion with
the benefit.

**Callback registered at connect time (rejected):** The agent registers
a function that fires when a command arrives on the control stream.
Callbacks fire at unpredictable points relative to the agent's own
execution state, creating re-entrancy hazards. The agent can be in the
middle of modifying shared state when the callback lands.

**Separate tokio task with a channel (rejected):** A background task
receives commands and sends them over an `mpsc::channel`. The agent
reads from the channel at yield points. This is structurally similar to
the chosen design but adds a task and a channel with no benefit, since
the control stream background task already holds the pending command and
the Mutex read is cheaper than a channel receive.
