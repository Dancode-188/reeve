# 0006: Privacy Tier 1 as the Default

**Status:** Accepted
**Date:** 2026-06-26

## Context

OTel span events on LLM calls carry the actual content of those
calls: user messages, assistant responses, and tool call arguments.
This is exactly the information most useful for debugging an agent,
and also exactly the information most likely to be sensitive. A user
prompt can contain API keys pasted carelessly, proprietary business
logic, personally identifiable information, or confidential
instructions. An assistant response can contain the same.

Reeve stores span events in SQLite on the local machine. It does not
send data anywhere. But "local" does not mean "safe to store without
asking": a developer running Reeve on a shared machine, or a company
deploying it across a team, may not want message content written to
disk at all.

The question is: should Reeve capture message content by default and
let users opt out, or capture nothing by default and let users opt in?

## Decision

Content capture is off by default. `SpanEvent.content` is `None`
unless the caller explicitly enables capture by passing
`capture_content: true` to the translator. This is called privacy
tier 1 behavior throughout the codebase: metadata is fully captured
(span structure, timing, token counts, cost), message content is not.

Opt-in to content capture, not opt-out.

## Consequences

**What gets easier:**
- Reeve can be deployed in privacy-sensitive environments without
  configuration. The safe behavior is the default behavior.
- Users who want content capture know they have made an active
  choice to enable it, which prompts the right level of thought
  about where the data goes.
- The span tree, health score, cost tracking, and policy evaluation
  all work correctly under privacy tier 1. Reeve is fully functional
  without content.

**What gets harder:**
- Users who want content capture for debugging must discover and
  enable the flag. For developers working alone on a personal
  machine, this is a small friction with no benefit.
- Tier 2 LLM-as-judge evaluation requires message content to assess
  faithfulness and hallucination. Under privacy tier 1, those
  specific metrics are unavailable. The health score renormalizes
  around the available Tier 1 metrics in that case.

## Alternatives considered

**Content capture on by default, opt-out to disable (rejected):**
Easier for the debugging use case, which is the primary one. Rejected
because it writes potentially sensitive data to disk without the user
making any explicit decision. The first time a developer points
Reeve at a production agent and walks away, content that was not
meant to be logged is now in a SQLite file. The default should
protect against the case where the user did not think about it.

**Per-event-type opt-in (proposed, deferred):** Capture tool call
arguments by default but not user messages, for example, since tool
arguments are less likely to be sensitive. Reasonable in principle
but overly granular for a first implementation. A single flag that
covers all content is the right starting point. Finer-grained tiers
can be added when there is concrete evidence they are needed.

**No content capture at all, ever (rejected):** Eliminates the
privacy question entirely. Rejected because there are legitimate
uses for content capture: a developer debugging their own agent on
their own machine has every reason to want to see what the model
said. Removing the option entirely would make Reeve less useful for
that case without adding meaningful protection for the sensitive
case, since the sensitive case is already handled by the default-off
behavior.
