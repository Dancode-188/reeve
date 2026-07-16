# Getting started

From install to watching a real agent. The install compiles for a few
minutes; everything after it took nine seconds on a machine that had
never seen Reeve. Two terminals: Reeve owns one, your agent gets the
other.

## Install

```bash
cargo install --git https://github.com/Dancode-188/reeve reeve-cockpit
```

One binary, `reeve`, lands in your cargo bin. No config file, no
account, no daemon. The compile is the only slow step; get coffee.

## Terminal 1: the cockpit

```bash
reeve
```

The cockpit fills the terminal and waits. It is listening on three
loopback ports: 4317 for OTel spans, 4318 for the HTTP proxy, 4316 for
the control channel. If any of them is taken, Reeve says so and names
the process holding it instead of starting blind.

## Terminal 2: your agent

```bash
eval "$(reeve env)"
```

That sets the two exports an agent needs, straight from the binary
that owns the ports. Then launch whatever you want to watch:

```bash
claude                  # Claude Code, through the proxy
python your_agent.py    # anything instrumented with the SDK
```

For Claude Code, give it a task and watch the cockpit: the turn
appears as a tree, tool calls attach to the chat that made them, and
token counts and cost tick up live. For SDK agents, spans appear as
your code emits them, and the pause and kill commands work at your
agent's checkpoints.

That is the whole setup. The [cockpit guide](cockpit.md) covers the
keys from here.

## Optional: quality scoring

If [Ollama](https://ollama.com) is running with `phi4-mini` pulled,
Reeve scores faithfulness, hallucination, and tool selection in the
background at zero cost. Without it, a banner says Tier 2 is
unavailable and the heuristic scoring carries on. Start Ollama any
time and press `r` on the banner; there is no need to restart Reeve.

## When nothing shows up

**The cockpit is running but your agent never appears.** Three causes,
in the order to check them:

1. The exports are not set in the terminal the agent launched from.
   `eval "$(reeve env)"` only affects the shell you ran it in, so run
   `env | grep -E 'ANTHROPIC_BASE_URL|OTEL'` in the agent's terminal
   and rerun the eval if they are missing.
2. The agent has not actually done anything yet. Claude Code sitting
   at its prompt makes no API calls, and no traffic means no spans.
   Give it a task.
3. SDK agents only: the exporter batches spans and flushes every
   second, so a script that exits immediately can die before its
   spans leave. Keep the process alive past its last span or flush
   the provider on shutdown.

**Reeve refuses to start and names a port.** Another process holds
it, most often an earlier Reeve you forgot about. Close the other
one; the fatal card offers a retry.

**The yellow Tier 2 banner.** Ollama is not reachable. That is a
degraded mode, not an error: scoring continues on heuristics alone.
Start Ollama and press `r`.

**Windows.** Reeve runs on Linux and macOS natively and on Windows
through WSL, where all of the above applies unchanged inside the WSL
shell. Native Windows is not supported.
