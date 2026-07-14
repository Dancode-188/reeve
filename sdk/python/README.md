# reeve-sdk

The Python SDK for [Reeve](https://github.com/Dancode-188/reeve), a
terminal cockpit for AI agents: watch a live trace tree, score every
step, and pause, redirect, or stop an agent while it runs.

```python
from reeve_sdk import CheckpointResult, ReeveSdk

sdk = await ReeveSdk.connect("my-agent")

@sdk.trace()
async def run():
    while not done:
        result = await sdk.checkpoint()  # pauses here if you command it to
        if isinstance(result, CheckpointResult.Redirect):
            steer_toward(result.instruction)
        response = await llm.invoke(messages)
```

Adapters ship for LangChain (`ReeveCallbacks`), the OpenAI Agents SDK
(`ReeveHooks`), and the Claude Agent SDK (`ReeveClaudeClient`); each
wires `checkpoint()` and OpenTelemetry spans into the framework's own
lifecycle. The cockpit itself installs with
`cargo install reeve-cockpit`.

Apache-2.0. The full story lives in the
[repository](https://github.com/Dancode-188/reeve).
