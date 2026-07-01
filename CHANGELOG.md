# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-07-01

### Added

- Five Tier 1 heuristic evaluators running in under a millisecond on every
  completed trace: loop detection (counts repeated span operations, threshold
  3), cost efficiency, latency normality, fingerprint deviation (all three
  require 10 completed traces to warm the baseline), and intent-action
  divergence (placeholder, activates in v0.3.0).
- Agent fingerprint: learns mean span count, cost, and duration over the first
  10 traces and uses the rolling baseline for cost and latency evaluation.
- Composite health score (0-100) built from a weighted sum of evaluation
  metrics. Default weights: faithfulness 0.30, tool selection 0.25, loop
  detection 0.20, cost efficiency 0.15, latency normality 0.10. Missing
  metrics renormalize to keep the full 0-100 range interpretable.
- Tier 2 LLM judge via Ollama with self-consistency scoring. Two independent
  passes with phi4-mini produce a score and a confidence level (High, Medium,
  or Low based on inter-pass agreement). Evaluates tool selection on every
  trace. Faithfulness and hallucination detection require privacy tier 2 or
  higher (not the default).
- Policy engine with `evalexpr` DSL. Conditions are plain strings:
  `health_score < 30`, `cost_usd > 5.0`, `loop_detection < 0.5`. Three
  built-in rules fire automatically: `builtin_loop_detected`,
  `builtin_high_cost`, `builtin_low_health`. Each rule has a 60-second
  per-agent cooldown to prevent alert floods.
- QUALITY section in the right panel. Appears once the first evaluation
  result arrives. Each row shows an abbreviated metric name, an 8-cell block
  gauge, a decimal score, and an H/M/L confidence badge for Tier 2 results.
  Footer shows `⋯ tier 2 scoring` while Tier 2 is pending, then switches to
  `N/5 metrics · renormalized` when weight coverage is below 1.0.
- Midline ellipsis on the health gauge label while Tier 2 evaluation is in
  progress. Clears once `run_tier2` completes.
- ALERTS section in the left panel. Appears when the policy engine has fired.
  Shows up to 5 alerts newest-first with a warning icon and the stripped rule
  name. Alerts persist until restart.
- ADRs 0007 and 0020-0022 documenting the evaluation architecture: weight
  renormalization, composite health score design, Tier 2 LLM judge, and the
  evalexpr policy DSL.

### Fixed

- Health gauge midline ellipsis persisted permanently after Tier 2 completed
  under privacy tier 1. `tier2_pending` was derived from `weight_coverage <
  1.0`, but faithfulness (weight 0.30) always returns `None` under privacy
  tier 1, so weight coverage never reached 1.0 even after all Tier 2 work
  finished. Fixed by hardcoding `tier2_pending: false` in the
  `HealthScoreUpdated` event emitted at the end of `run_tier2`.
- Tracing output was written to stderr while Ratatui owned the terminal,
  corrupting panel rows mid-render. Fixed by redirecting all log output to
  `~/.local/share/reeve/reeve.log` before the TUI starts.

## [0.1.0] - 2026-06-28

### Added

- OTel gRPC receiver on port 4317. Accepts `ExportTraceServiceRequest` from
  any OpenTelemetry-instrumented agent. No SDK required on the agent side.
- Four-stage ingestion pipeline: receive (validation, dedup, clock alignment),
  normalize (OTel GenAI semantic convention translation), assemble (orphan
  adoption, 2-second straggler window, root-triggered completion), route
  (fan-out to hot tier, warm tier, and renderer signal channel).
- Hot tier ring buffer for active traces. Configurable span capacity with
  eviction to warm tier on overflow.
- Warm tier SQLite database: completed traces, all spans with `arrived_at`
  timestamps, agent registry. Schema migrations run automatically on startup.
- Three-panel terminal renderer: left panel (agent list with status indicators
  and cost sparkline), center panel (live trace tree), right panel (SPAN detail
  and health gauge).
- Live trace tree with operation names from OTel span fields. Box-drawing tree
  connectors with full ASCII fallback via `--ascii` flag.
- Span selection in the center panel. j/k navigates spans in DFS order.
  Selected span details appear immediately in the SPAN panel.
- SPAN detail panel: operation name (truncated with ellipsis at panel edge),
  status, start time (HH:MM:SS.mmm), duration (ms), cost (USD) when present.
- Cost sparkline per agent: last 60 traces, braille character graph.
- Health score gauge: live placeholder, fully activated in v0.2.0.
- Panel focus cycling with Tab and Shift+Tab.
- Agent status tracking with warm store. Agents loaded on startup are reset
  to Idle so stale status from previous sessions does not mislead.
- 15fps render loop with live signal polling on every tick.
- ADRs 0001-0006, 0008-0018 documenting all major design decisions through the
  renderer. ADR-0007 is reserved for weight renormalization and will be written
  when the evaluation engine ships.
- Eight built-in color themes: Catppuccin Mocha (default), Latte, Frappe,
  Macchiato, Dracula, Nord, Tokyo Night, Gruvbox.
- GitHub Actions CI: fmt check, clippy with `-D warnings`, tests, release build.
- Issue templates, PR template, CONTRIBUTING.md, ROADMAP.md.

[0.2.0]: https://github.com/Dancode-188/reeve/releases/tag/v0.2.0
[0.1.0]: https://github.com/Dancode-188/reeve/releases/tag/v0.1.0
