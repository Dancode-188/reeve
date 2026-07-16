# Instrumenting your own agent

The adapters cover LangChain, OpenAI Agents, and the Claude Agent SDK.
For everything else, the Python SDK instruments any agent loop
directly. Two things make an agent a Reeve agent: it emits spans, and
it calls `checkpoint()` where stopping is safe.

## Connect

```python
from reeve_sdk import ReeveSdk

sdk = await ReeveSdk.connect("research-agent")
```

`connect` opens the control channel, registers the agent in the
cockpit, and wires an OTel exporter at the ingestion port. Optional
arguments: `framework` (shows in the cockpit, default `"custom"`),
`instance_id` (distinguishes two copies of the same agent), and
`capabilities` (which commands the intervention overlay offers for
this agent; the default is all of them).

## Emit spans

Anything OTel works, since Reeve speaks the GenAI conventions. The
SDK ships a decorator for the common case:

```python
@sdk.trace
async def call_model(prompt):
    response = await client.messages.create(...)
    return response
```

That wraps the coroutine in a `gen_ai.chat` span. For more control,
use OTel directly and set the attributes yourself: model on
`gen_ai.request.model`, token counts on `gen_ai.usage.input_tokens`
and `gen_ai.usage.output_tokens`. Spans carrying a model and token
counts get priced automatically; you never send cost.

One convention matters more than the rest: end your root span last.
Reeve completes a trace when its root arrives, so a root that closes
while children still work truncates the trace. Open a task-level span
when work starts, close it after the final child.

## Checkpoints

```python
from reeve_sdk import CheckpointResult, AgentKilled

while not done:
    result = await sdk.checkpoint()
    match result:
        case CheckpointResult.Redirect(instruction=text):
            goal = text            # replace the current objective
        case CheckpointResult.Context(context=text):
            notes.append(text)     # add it, keep the objective
        case CheckpointResult.Continue():
            pass
    await do_next_step(goal)
```

`checkpoint()` is the agent's side of the intervention contract.
Call it between steps, before expensive operations, at the top of
loops: anywhere stopping would not corrupt work. What it does per
command:

**Pause** holds inside the call until the operator resumes. Your code
simply does not return from `checkpoint()`, which is why a paused
agent needs no pause-handling logic at all.

**Redirect** and **inject context** return the operator's text as
`Redirect` or `Context`. What they mean is up to your loop; the usual
reading is that a redirect replaces the goal and context adds to it.

**Kill** raises `AgentKilled` out of the call. Let it propagate to
your shutdown path; catching and continuing makes the kill a lie and
the cockpit will show an agent that reported dying and kept talking.

An agent that never calls `checkpoint()` still shows up and streams
spans, but every command against it sits at `delivered` until it
expires. Observability is free; controllability is these calls.

## The whole thing

```python
import asyncio
from reeve_sdk import AgentKilled, CheckpointResult, ReeveSdk

async def main():
    sdk = await ReeveSdk.connect("research-agent")
    goal = "summarize the papers in ./inbox"
    try:
        while True:
            result = await sdk.checkpoint()
            if isinstance(result, CheckpointResult.Redirect):
                goal = result.instruction
            elif isinstance(result, CheckpointResult.Context):
                goal = f"{goal}\n(operator note: {result.context})"
            done = await work_one_step(sdk, goal)
            if done:
                break
    except AgentKilled:
        await cleanup()

asyncio.run(main())
```

Run it with `eval "$(reeve env)"` in the shell first, and it appears
in the cockpit next to everything else.
