# 0002: Local-First LLM Judge

**Status:** Accepted
**Date:** 2026-06-23

## Context

Reeve's Tier 2 evaluation uses an LLM to judge agent behavior for things that
deterministic heuristics cannot catch: subtle goal drift, reasoning quality,
whether an agent's stated plan matches its actual actions.

This requires running inference somewhere. The options are a cloud API
(OpenAI, Anthropic, etc.) or a local model via something like Ollama.

The people most likely to run Reeve are developers building agents that
themselves call cloud LLM APIs. Those agents' conversations (tool calls,
reasoning chains, full context windows) flow through Reeve's observation
channel. That data is sensitive. Sending it to a second cloud service for
evaluation creates a real privacy surface: now the conversation data is leaving
the developer's machine twice, to two different vendors, with two different
retention policies.

There is also a cost dimension. Reeve is a developer tool, not a managed
service. Developers do not want to pay per-evaluation on top of whatever their
agents are already spending on inference.

## Decision

Default to local inference via Ollama, using phi4-mini as the default model.
On startup, Reeve auto-detects whether Ollama is running and which models are
available. If Ollama is not available, Tier 2 evaluation is skipped and the
health score runs on Tier 1 heuristics alone (weight renormalization handles
this; see ADR 0007).

Cloud API support will be added later as an opt-in, for developers who
explicitly want to trade privacy for model quality.

## Consequences

**What gets easier:**
- Zero cost per evaluation for the default configuration.
- Agent conversation data stays on the developer's machine.
- No API keys required for basic Reeve installation.
- Works offline.

**What gets harder:**
- phi4-mini is less capable than frontier models. Some subtle behaviors it
  will miss.
- Users must have Ollama installed to get Tier 2 evaluation at all.
- Inference latency on CPU is higher than cloud APIs, which affects evaluation
  turnaround for complex traces.

**Acceptable tradeoffs:**
- Tier 2 is the "deep" evaluation layer. Tier 1 heuristics catch the common
  cases (loops, cost spikes, latency anomalies) fast and cheap. Tier 2 is for
  nuanced judgment on already-suspicious traces, where a few hundred
  milliseconds of inference latency is not a problem.

## Alternatives considered

**OpenAI API as default (rejected):** Requires API key setup, costs money per
call, sends agent data to a third party without explicit opt-in. Not acceptable
as a default for a privacy-sensitive tool.

**No Tier 2 at all (rejected):** Heuristics alone miss goal drift and reasoning
quality issues. The LLM judge is what makes Reeve qualitatively different from
a metrics dashboard.

**Embed a local model directly, no Ollama dependency (rejected):** Significantly
increases binary size and complexity. Ollama is a widely adopted local inference
server that developers building with local models likely already have. Treating
it as an optional dependency with graceful degradation when absent is the right
tradeoff.
