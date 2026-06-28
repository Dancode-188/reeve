# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[0.1.0]: https://github.com/Dancode-188/reeve/releases/tag/v0.1.0
