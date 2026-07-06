# Roadmap

Each milestone is a complete, demoable slice of the value proposition. Not just
compilable. Actually demoable.

---

## v0.1.0: Observable

Connect an agent. Watch the trace tree grow in real time. See the LLM response
appear token by token with a blinking cursor.

The loop is: agent emits OTel spans, Reeve ingests them, terminal renders them
live. That is the whole thing. No evaluation, no intervention. Just observation.

**What this proves:** The ingestion pipeline works. The renderer renders. The
two-channel architecture is real.

---

## v0.2.0: Evaluable

Every span gets scored. Health score gauge changes color as it drops. Policy
rules fire automatically.

Tier 1 heuristics: loop detection, cost acceleration, latency anomalies. All
sub-millisecond. Health score is live in the header. Tier 2 (Ollama LLM judge)
shows up when Ollama is running.

**What this proves:** The evaluation engine is real and fast. The composite
health score is useful.

---

## v0.3.0: Intervenable

Press `i`. Pause the agent, redirect it, inject context, or kill the trace.

The gRPC control channel is live. SDK adapters working: LangChain, OpenAI
Agents SDK, custom Python. `checkpoint()` in the Rust SDK. The agent actually
pauses when you tell it to.

**What this proves:** The two-channel architecture pays off. The whole loop
works.

---

## v0.4.0: Historical

Replay mode. DVR controls. Intervention impact view.

Load any past trace and scrub through it. See exactly where quality dropped and
what the intervention did. The `W` key shows the before/after view.

**What this proves:** The `arrived_at` field design decision was correct.
Replay actually works.

---

## v0.5.0: Transparent

Point `ANTHROPIC_BASE_URL` at Reeve. Watch Claude Code appear in the cockpit.
No SDK, no instrumentation, no code changes.

The HTTP proxy intercepts request/response pairs and synthesizes spans from
them, streaming SSE included, with zero added latency to the tool being
proxied. Redirect and inject-context work cleanly through the proxy by
modifying the buffered request before forwarding. Pause and kill are fragile
without an SDK, so proxy-connected agents carry a `[proxy]` indicator and
their reduced capabilities are stated plainly rather than papered over.

**What this proves:** Reeve is useful with tools you already run, before you
write a single line of integration code.

---

## v1.0.0: Production

All adapters. Full docs. Stable APIs. `cargo install reeve` actually works.

Claude Agent SDK adapter. Python SDK published to PyPI. ARCHITECTURE.md
written. All 30+ ADRs complete. Demo GIF in README. Getting started guide
works in under 90 seconds.

**What this proves:** The project is ready for other people to use.

---

## Beyond v1.0.0

Nothing committed. Ideas being considered:

- Control protocol spec: the wire contract between Reeve and its agents
  (handshake, capability declaration, command and ack lifecycle, checkpoint
  semantics) written up as a versioned document that anyone could implement
  against without reading Reeve's source. The read path out of agents has a
  standard; the write path into them does not.
- Intervention effectiveness dataset: v0.4.0 starts recording what each
  intervention did to quality. Accumulated, that becomes something nobody
  has: measured evidence of which interventions fix which failure modes.
  Guardrail products block on predictions and never learn whether they were
  right. This would.
- Rewind: scrub back to an earlier checkpoint, edit the context the agent
  had, and re-execute from there. Branches for agent execution. The honest
  caveat is that re-running past tool calls against the real world is only
  safe when the tools are read-only or mocked, so this needs a tool
  isolation story before it needs a scrubber.
- Fleet mode: observe multiple agents across machines from one terminal
- Config server: push policy rule updates to a running Reeve instance
  without restart
- Export: send evaluation results to external observability platforms
- Web UI: for when a terminal is not the right tool (not everyone's situation)
