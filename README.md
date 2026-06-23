# Reeve

[![CI](https://github.com/Dancode-188/reeve/actions/workflows/ci.yml/badge.svg)](https://github.com/Dancode-188/reeve/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.78%2B-orange.svg)](https://www.rust-lang.org)

<!-- Demo GIF goes here before v0.3.0 ships.
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
┌─ REEVE v0.1.0 ──────────────────────── ● research-bot  ◆72 CAUTION  $0.047 ──┐
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
 [j/k] nav  [i] intervene  [p] pause/resume  [n] note  [?] help  [q] quit
```

The trace tree grows as spans arrive. The LLM response appears token by token with
a blinking cursor. The health score tells you whether the agent is doing well. Press
`i` to intervene.

---

## Quick start

```bash
cargo install reeve
reeve
```

Reeve listens on `:4317` for OTel and `:4318` as an HTTP proxy. Connect your agent
and it shows up. If Ollama is running with phi4-mini available, quality scoring starts
immediately at zero cost. No account, no API key, no setup wizard.

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
context, or kill the trace. When the agent continues, Reeve measures whether the
intervention improved quality. If you redirect a drifting agent and the health score
goes from 0.31 to 0.89 over the next four spans, that shows up inline in the trace
tree. You know it worked.

---

## Connecting your agent

### SDK (full capability)

**LangChain**
```python
from reeve import ReeveCallbacks

agent = create_agent(
    llm=llm, tools=tools,
    callbacks=[ReeveCallbacks(endpoint="http://localhost:4317")]
)
```

**OpenAI Agents SDK**
```python
from reeve.adapters.openai import ReeveHooks

agent = Agent(name="...", model="gpt-4o", tools=[...],
              hooks=ReeveHooks(endpoint="http://localhost:4317"))
```

**Custom Python**
```python
from reeve import ReeveSdk

reeve = ReeveSdk(endpoint="http://localhost:4317", agent_name="my-agent")

@reeve.trace()
async def run():
    while not done:
        await reeve.checkpoint()  # pauses here if you command it to
        with reeve.llm_span() as span:
            response = await llm.invoke(messages)
            span.record_usage(response.usage)
        await reeve.checkpoint()
```

Any OTel-instrumented agent can point directly at Reeve:
```bash
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317 python your_agent.py
```

### HTTP proxy (no SDK required)

For Claude Code and similar tools that call the API directly:
```bash
ANTHROPIC_BASE_URL=http://localhost:4318 claude
```

Redirect and inject context work fully. Clean pause and kill do not. See
[proxy path docs](docs/guides/proxy-path.md) for what works and what does not.

---

## A few things worth saying upfront

**Nothing leaves your machine by default.** Quality evaluation runs locally via
Ollama. If Ollama is available, the status bar shows `quality evaluation: local
(phi4-mini)`. Cloud evaluation is opt-in with a cost cap you set. Reeve will not
send your agent's data anywhere without you explicitly configuring it. This matters
if your agents handle anything sensitive. Also just on principle.

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
| OpenAI Agents SDK | SDK | Full | Yes | Yes |
| Claude Agent SDK | SDK | Full | Yes | Yes |
| Custom Python | SDK | Full | Yes | Yes |
| Rust agents | SDK | Full | Yes | Yes |
| Claude Code | Proxy | Full | Limited | Yes |
| Any OTel agent | OTel | Full | No | No |

---

## Install

```bash
cargo install reeve
```

Or from source:
```bash
git clone https://github.com/Dancode-188/reeve
cd reeve
cargo build --release
```

Requires Rust 1.78+. Linux and macOS. On Windows, Windows Terminal works fine. Use
`--ascii` if the Unicode characters do not render correctly.

---

## Configuration

Works without any config file. When you want to change something:

```toml
# ~/.config/reeve/config.toml

[evaluation.tier2]
backend = "auto"       # auto | local | cloud | off
sample_rate = 0.20

[evaluation.tier2.local]
model = "phi4-mini"

[evaluation.tier2.cloud]
provider = "anthropic"
model = "claude-haiku-4-5"
api_key_env = "ANTHROPIC_API_KEY"
max_cost_per_session_usd = 1.00   # hard cap. it will not go over this.

[[intervention.templates]]
name = "summarize and stop"
type = "Redirect"
instruction = "Please summarize what you have accomplished so far and stop."
```

Full reference at [docs/guides/configuration.md](docs/guides/configuration.md).

---

## Documentation

- [Architecture](docs/ARCHITECTURE.md): how the five layers fit together
- [Architecture Decision Records](docs/adr/README.md): 30+ design decisions
  documented with context, alternatives considered, and consequences. Why a
  2-second straggler window and not 30 seconds. Why phi4-mini. Why the control
  channel is separate from the OTel channel. Most projects lose this reasoning
  the moment a decision is made. It is all in here.
- [Getting started](docs/guides/getting-started.md)
- [Framework integration guides](docs/guides/)
- [Configuration](docs/guides/configuration.md)
- [Roadmap](ROADMAP.md)

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
