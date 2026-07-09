# 0037: Conversation Threading from Message Prefixes

**Status:** Accepted
**Date:** 2026-07-09

## Context

Without threading, every API call through the proxy is its own
single-span trace, and an agentic session renders as confetti: no task
structure, no tool visibility, no meaningful health or cost per task.
The proxy sees only independent HTTP requests, but an agentic client
resends the full conversation on every call, and that repetition carries
the structure.

Three questions needed answers: how to recognize that two requests
belong to the same conversation, what granularity a trace should have,
and how tool activity becomes visible.

## Decision

**Matching is by message-prefix fingerprints.** Each request's `messages`
array is hashed per message; a request whose fingerprints extend a known
conversation's stored prefix belongs to it, longest match winning.
Conversations are keyed under the agent name first, so identical content
from different tools never threads together. A prefix mismatch, whatever
the cause (context compaction, edited history, client restart), starts a
new conversation, which is the proxy's pre-threading behavior:
degradation is graceful by construction, and the worst case equals the
naive design.

**A trace is one conversation turn, not one session.** All round trips
from a user message until the assistant stops requesting tools
(`stop_reason` other than `tool_use`) form one trace. A session lasting
hours would make single traces useless for history, replay, and health;
a turn is seconds to minutes, matching the task-shaped traces the SDK
path produces.

**The turn root is emitted last.** Chat and tool spans arrive as
children of a synthetic root that is emitted only when the turn ends,
because the assembler starts its completion clock when a root arrives.
SDK agents already emit their task root last, so the pipeline and the
cockpit's awaiting-parent rendering handle this shape without any
changes.

**Tool calls are reconstructed from the request-response seam.** A
`tool_use` block in a response opens a pending tool; the matching
`tool_result` in the conversation's next request closes it as a child
span of the chat span that requested it, spanning the gap between
response and next request, failed when the result says `is_error`. The
gap is exactly the tool's execution time as the client experienced it.

Threading state lives in proxy memory only and idle conversations are
pruned after thirty minutes.

## Consequences

**What gets easier:**
- A Claude Code session's structure (chat turns, tool calls, durations,
  failures) renders as a tree with zero instrumentation, which is the
  milestone's core promise.
- Per-turn context growth is visible from the same data: each chat span
  records the conversation's message count.
- Evaluation and policy see task-shaped traces from the proxy path, so
  health scores and cost rules mean the same thing they mean for SDK
  agents.

**What gets harder:**
- Tool execution time includes client think-time between requests; the
  two are indistinguishable from traffic. Accepted: for agentic clients
  the gap is dominated by the tool.
- A tool running longer than the assembler's idle timeout flickers the
  trace Interrupted until the remaining spans and root arrive, at which
  point storage heals it. Known seam, recorded on the issue.
- A turn abandoned mid-tool-loop (client killed) never emits its root
  and surfaces as Interrupted by idle timeout, which is honest but
  indistinguishable from a slow tool until the prune window passes.
- Restarting Reeve forgets threading state: the next request starts a
  new conversation. Accepted for state this cheap to rebuild.

## Alternatives considered

- **One trace per session.** Multi-hour traces break history granularity,
  replay length, and per-trace health; rejected on trace semantics.
- **One trace per API call (status quo).** No structure at all; the
  reconstruction opportunity is the whole point of sitting in the path.
- **Matching on a conversation id header.** Anthropic requests carry no
  such id, and requiring one would reintroduce instrumentation.
- **Holding conversation state in storage.** Durable threading across
  restarts is not worth schema and lifecycle for state that a single
  request rebuilds organically.
