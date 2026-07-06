# Reeve

[![CI](https://github.com/Dancode-188/reeve/actions/workflows/ci.yml/badge.svg)](https://github.com/Dancode-188/reeve/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.78%2B-orange.svg)](https://www.rust-lang.org)

<!-- Demo GIF goes here before v1.0.0 ships.
     Record: agent connects, trace tree grows in real time, streaming
     cursor appears, health score drops, policy fires, redirect sent,
     quality recovers. Eight seconds. That clip is the whole pitch.    -->

---

Your agent ran for 40 minutes. It called the same search tool 89 times with the
same query because something broke in its context and it got stuck. You found out
this morning when you opened the dashboard and looked at the chart.

The chart looked great, by the way.

Maybe you tried guardrails instead. You wrote rules upfront to catch bad behavior.
Your agent found eight failure modes you hadn't predicted and the guardrail watched
all eight happen and did nothing. Because none of them matched.

Or maybe you're doing the thing where you just tail the logs and grep for errors.
Honest approach. Completely useless when the agent is looping.

The problem is not that existing tools are bad. The problem is that they live in the
wrong moment. Post-hoc evaluation shows you what happened after the run ended.
Pre-defined guardrails catch what you predicted. Neither one helps when something is
going wrong right now and you want to stop it or change direction without killing the
whole process.

Reeve is for that moment.

---

```
┌─ REEVE v0.3.0 ──────────────────────── ● research-bot  ◆72 CAUTION  $0.047 ──┐
│ AGENTS          │ TRACE ── task-0047 ── 12.4s                   │ SPAN DETAIL│
│                 │                                               │            │
│ ● research-bot  │ ▾ agent.execute  ◷ 12.4s  ●                   │ gen_ai.chat│
│   ◆72  $0.047   │ │                                             │ ✓ completed│
│                 │ ├─▾ gen_ai.chat  ◷ 2.1s  ✓  [★0.89]  ♦        │            │
│ ○ code-reviewer │ │   └─ gen_ai.tool:web_search  ◷ 0.8s  ✓      │dur  2,104ms│
│   idle          │ │      ↳ redirect +0.58 quality · 4 spans     │ cost $0.003│
│                 │ └─▾ gen_ai.chat  ●  [STREAMING]               │ ctx  ████ ⚠│
│ HEALTH          │     sonnet-4 · 1,089↑  ctx 78% ⚠              │78% of limit│
│ ███████░░  72   │     ┌─────────────────────────────────┐       │            │
│ CAUTION         │     │ The renewable energy sector has │       │ QUALITY    │
│ ⋯ hallu scoring │     │ seen remarkable growth. Solar   │       │ faith ██.89│
│                 │     │ and wind capacity expanding▌    │       │ tools ██.94│
│ COST            │     └─────────────────────────────────┘       │ hallu ⋯ ...│
│ $0.047 today    │                                               │            │
│ ▁▂▃▄▅▆▇  ↑      │                                               │SCORE   ██72│
│ predicted $0.11 │                                               │ 2/3 metrics│
└─────────────────┴───────────────────────────────────────────────┴────────────┘
 [j/k] nav  [i] intervene  [p] pause/resume  [?] help  [q] quit
```

The trace tree grows as spans arrive. The LLM response appears token by token with
a blinking cursor. The health score tells you whether the agent is doing well. Press
`i` to intervene.

---

## Quick start

```bash
cargo install --git https://github.com/Dancode-188/reeve reeve
reeve
```

Reeve listens on `:4317` for OTel spans and `:4316` for the control channel.
Connect your agent and it shows up. If Ollama is running with phi4-mini
available, quality scoring starts immediately at zero cost. No account, no API
key, no setup wizard.

---

## What it does

Four things. In order.

**Watch.** Connect via OTel SDK integration or HTTP proxy. You get a live trace tree
that builds as your agent works. When a span is streaming, the LLM response
accumulates in the terminal with a blinking cursor. You are watching the model think.
Nothing else does this.

**Score.** Every span gets evaluated. Heuristic checks run in under a millisecond:
loop detection, cost acceleration, latency anomalies, intent vs action mismatch. LLM
judge scoring for faithfulness, hallucination, and tool selection quality runs in the
background via Ollama locally. It all feeds a single health score from 0 to 100 that
changes color as it drops. Green, amber, red. You know at a glance.

**React.** Write policy rules in plain conditions: `health_score < 30`,
`cost_usd > 5.0`, `tool_name == "bash" AND content contains "rm -rf"`. Rules fire
automatically. You can write predicted thresholds that fire before a limit is hit,
not after.

**Intervene.** Press `i`. Pause the agent, redirect it with a new instruction, inject
context, or kill the trace. The agent applies the command at its next safe yield
point and acknowledges every step back to the cockpit, so you watch the command
land instead of hoping it did. Measuring whether the intervention improved quality
and showing the delta inline in the trace tree is next on the
[roadmap](ROADMAP.md) (v0.4.0).

---

## Connecting your agent

### SDK (full capability)

**LangChain**
```python
from reeve import ReeveSdk
from reeve.adapters.langchain import ReeveCallbacks

sdk = await ReeveSdk.connect("research-bot")
agent = create_agent(llm=llm, tools=tools, callbacks=[ReeveCallbacks(sdk)])
```

**Custom Python**
```python
from reeve import CheckpointResult, ReeveSdk

sdk = await ReeveSdk.connect("my-agent")

@sdk.trace()
async def run():
    while not done:
        result = await sdk.checkpoint()  # pauses here if you command it to
        if isinstance(result, CheckpointResult.Redirect):
            steer_toward(result.instruction)
        response = await llm.invoke(messages)
        await sdk.checkpoint()
```

**Rust**
```rust
let reeve = ReeveSdk::connect(SdkConfig::new("my-agent")).await?;
loop {
    reeve.checkpoint().await?;  // returns Err(AgentError::Killed) on kill
    let mut span = reeve.llm_span();
    let response = llm.invoke(&messages).await?;
    span.set_token_usage(response.usage.total_tokens);
}
```

Adapters for the OpenAI Agents SDK and the Claude Agent SDK are planned for
v1.0.0.

Any OTel-instrumented agent can point directly at Reeve:
```bash
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317 python your_agent.py
```

### HTTP proxy (no SDK required, planned)

The proxy path is v0.5.0 on the [roadmap](ROADMAP.md): point
`ANTHROPIC_BASE_URL` at Reeve and watch Claude Code appear in the cockpit with
zero integration work. Redirect and inject context will work cleanly through
the proxy; pause and kill are fragile without an SDK and will say so honestly.

---

## A few things worth saying upfront

**Nothing leaves your machine.** Quality evaluation runs locally via Ollama. If
Ollama is available, the status bar shows the local backend; if not, Tier 2
evaluation turns itself off and says so. There is no cloud path, no telemetry,
no account. This matters if your agents handle anything sensitive. Also just on
principle.

**Steer, don't block.** Most guardrail systems stop execution when something goes
wrong. Reeve biases toward redirecting the agent and letting it self-correct. A good
redirect preserves the work done so far. Kill is there for when you need it. It is
not the first suggestion.

**One number, not ten.** Faithfulness, hallucination, tool selection, loop detection,
cost, latency: it all feeds one health score. You set one threshold, not ten. The
individual metrics are still there when you want to understand why the score dropped.
But the gauge in the header tells you at a glance.

**Reeve is a local developer tool.** Not a production monitoring SaaS. No account to
create, no cloud dashboard, no team seats. It runs on your machine, talks to your
agent on localhost, stores data locally. When that's not what you need, other tools
exist. Datadog and similar platforms handle fleet-level production observability.
Reeve handles the moment when you are building and running agents locally and
something is going wrong right now.

---

## Supported frameworks

| Framework | Integration | Observation | Pause/Resume | Redirect |
|-----------|-------------|-------------|--------------|----------|
| LangChain | SDK | Full | Yes | Yes |
| Custom Python | SDK | Full | Yes | Yes |
| Rust agents | SDK | Full | Yes | Yes |
| OpenAI Agents SDK | SDK | Planned (v1.0.0) | — | — |
| Claude Agent SDK | SDK | Planned (v1.0.0) | — | — |
| Claude Code | Proxy | Planned (v0.5.0) | — | — |
| Any OTel agent | OTel | Full | No | No |

---

## Install

From source:
```bash
git clone https://github.com/Dancode-188/reeve
cd reeve
cargo build --release
./target/release/reeve
```

Or straight from the repository:
```bash
cargo install --git https://github.com/Dancode-188/reeve reeve
```

Requires Rust 1.78+. Linux and macOS. On Windows, Windows Terminal works fine. Use
`--ascii` if the Unicode characters do not render correctly.

---

## Configuration

Works without any config file. When you want your own policy rules:

```toml
# ~/.config/reeve/config.toml

[[rules]]
id = "my_cost_ceiling"
name = "cost ceiling"
description = "Trace crossed two dollars"
trigger_condition = "cost_usd > 2.0"
command_type = "pause"            # pause | resume | kill
requires_confirmation = true      # default true
cooldown_secs = 300               # default 300
auto_confirm_after_secs = 30      # optional: auto-execute countdown
```

Rules load at startup and reload on `SIGUSR1` without a restart. Conditions
use the same primitives as the built-in rules: `health_score`, `cost_usd`,
`span_count`, `predicted_cost`, and any evaluation metric by name.

---

## Documentation

- [Architecture Decision Records](docs/adr/README.md): every significant design
  decision documented with context, alternatives considered, and consequences.
  Why a 2-second straggler window and not 30 seconds. Why phi4-mini. Why the
  control channel is separate from the OTel channel. Most projects lose this
  reasoning the moment a decision is made. It is all in here.
- [Roadmap](ROADMAP.md)
- [Architecture](docs/ARCHITECTURE.md) and getting-started guides land with
  v1.0.0.

---

## Contributing

Issues and PRs are welcome. If you find a bug, open an issue. If you want to add
a framework adapter, read [CONTRIBUTING.md](CONTRIBUTING.md) first.

Before opening a PR for a design-level change, check [docs/adr/](docs/adr/) to see
if the relevant decision is already there. If it is, your PR should explain why you
are departing from it. If it is not, your PR should include a new ADR. This is not
bureaucracy. It is just how the project keeps its reasoning from getting lost.

---

## Credits

[Ratatui](https://ratatui.rs) powers the terminal UI. Without it this would be a
wall of `println!` calls and nobody would use it.

The cockpit layout draws from three things: [btop](https://github.com/aristocratos/btop)
for information density and the braille graph aesthetic,
[lazygit](https://github.com/jesseduffield/lazygit) for the navigation model and
always-visible footer keybindings, and [k9s](https://github.com/derailed/k9s) for
real-time context switching without interrupting focus. All three are worth using
on their own.

The [OpenTelemetry](https://opentelemetry.io) GenAI semantic conventions team for
the unglamorous work of standardizing how AI systems report behavior.

---

## License

Apache 2.0. See [LICENSE](LICENSE).

Use it, modify it, build on it. Attribution required. Works for individuals and
companies alike.

---

*Named after the historical reeve: an overseer who managed workers on behalf of
an authority. That is exactly what this does.*
