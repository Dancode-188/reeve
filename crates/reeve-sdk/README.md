# reeve-sdk

The Rust SDK for agents that want to be watched. Opens the control
channel, registers the agent, and provides `checkpoint()`, the call
that makes pause, redirect, and kill enforceable at the agent's own
safe yield points. The Python SDK (`reeve-sdk` on PyPI) is the same
contract for Python agents.

Part of [Reeve](https://github.com/Dancode-188/reeve), the terminal
cockpit for AI agents.
