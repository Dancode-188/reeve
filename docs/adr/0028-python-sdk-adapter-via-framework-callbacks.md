# 0028: Python SDK Adapter via Framework Callback Handlers

**Status:** Accepted
**Date:** 2026-07-05

## Context

Python agents commonly run on frameworks such as LangChain that manage
the LLM call lifecycle internally. The agent developer does not write an
explicit loop; they compose a chain and run it. There is no obvious
place to insert `checkpoint()` calls manually, and asking developers to
find one puts the burden of safe interrupt handling on every user of the
SDK.

LangChain has a native extension point: `BaseCallbackHandler` and
`AsyncCallbackHandler`. Callbacks fire at the start and end of every
LLM call, tool call, chain run, and agent step. These are the natural
yield points where the agent is between operations.

## Decision

Each supported framework gets an adapter class that subclasses the
framework's own callback handler. For LangChain, `ReeveCallbacks`
subclasses `AsyncCallbackHandler`.

`checkpoint()` is called at `on_llm_start`, `on_tool_start`, and
`on_tool_end`. These are the three points where the agent is not
mid-operation: before an LLM call begins, before a tool begins, and
immediately after a tool returns. A Pause at any of these points holds
the agent at a stable boundary.

OTel spans are opened and closed inside the same callbacks. `on_llm_start`
opens an `llm.call` span with model metadata; `on_llm_end` closes it
with token usage. `on_tool_start` opens a span named after the tool;
`on_tool_end` closes it. This keeps telemetry synchronized with the
framework's own event timeline without requiring separate instrumentation.

The adapter pattern extends to other frameworks on the same principle.
OpenAI Agents SDK and Claude's agent SDK each have their own event hook
systems; adapters for those follow the same structure.

## Consequences

**What gets easier:**
- A developer using LangChain adds one line: pass `ReeveCallbacks(sdk)`
  to the chain's `callbacks` argument. No manual checkpoint() calls,
  no changes to chain logic.
- The adapter survives LangChain version bumps as long as the callback
  interface is stable. It does not depend on internal LangChain classes
  or private methods.
- Adding a new framework adapter is a self-contained task: subclass the
  framework's callback base, call checkpoint() at the right hooks, open
  and close spans. No changes to the core SDK.

**What gets harder:**
- checkpoint() only fires at the framework's callback boundaries. Custom
  Python code that runs inside a tool function, between two tool calls
  in a chain step, is not covered. Developers who need finer-grained
  interrupt points must call checkpoint() manually inside those
  functions.
- The adapter assumes the framework fires callbacks synchronously with
  execution. A framework that batches callbacks asynchronously would
  require a different approach.

## Alternatives considered

**Monkey-patching the LLM client (rejected):** Wrap or replace the
LLM client's call method at import time to inject checkpoint() before
each call. This is fragile: the patch breaks when the client changes its
internal call path, when the framework uses a different client variant,
or when multiple patches stack. Debugging patched code is harder than
debugging a clean subclass.

**Wrapping the LLM client class (rejected):** Subclass the client and
override the `__call__` or `invoke` method. This is version-coupled:
every new client class (ChatOpenAI, ChatAnthropic, ChatOllama) needs a
separate wrapper. It also does not cover tool calls, which are managed
by the framework, not the client.

**Requiring manual checkpoint() calls (rejected):** Document checkpoint()
and ask developers to place calls at appropriate points in their agent
code. This works for custom agent loops but defeats the purpose of an
adapter for framework-based agents. The entire point of the adapter
layer is that most agents should not need to know checkpoint() exists.
