# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.0] - 2026-07-08

### Added

- Intervention outcome measurement: every applied command gets a measured
  before/after health delta across the agent's next three completed traces
  (the cross-trace window, ADR-0032). Outcome annotations render inline in
  the trace tree under the span the command touched.
- Effectiveness memory: when a policy rule fires, its alert carries the
  track record of past interventions for that rule (`redirect: +0.42 avg ·
  5 tries`), aggregated by rule identity with a same-framework fallback
  below three samples (ADR-0034), and the suggested intervention is the
  one that has historically worked.
- View modes on `1`/`2`/`3`/`4`: Fleet (the default cockpit), Focus (one
  agent, trace-history strip, full-width tree), History (persisted traces
  from the warm store with cost aggregation and `d` delete), and Cost
  (per-agent and per-model spend charts).
- Replay mode (`R` on a History trace): DVR controls with play/pause,
  step, four speeds, a scrubber with intervention markers, `I`/`i` marker
  jumps, and the health gauge re-animating exactly as scores arrived.
  Spans replay in arrival order, evaluations and commands interleave at
  their recorded timestamps.
- Intervention impact view (`W` on a History trace with interventions):
  actual health and cost after the command charted against the
  trajectory the pre-intervention trend projected.
- Privacy tier configuration: `privacy_tier` in
  `~/.config/reeve/config.toml`, default 1 (metadata only), tier 2
  enables span content capture with a `consent.log` line recording the
  decision. Read once at startup, failing closed (ADR-0035).
- Mouse support: click to select agents and spans, click a selected span
  to fold it, scroll any panel, click the replay scrubber to seek. `m`
  toggles capture off for native text selection, with a `[mouse off]`
  header indicator.
- Command palette on `:` with incremental completion: `pause all`,
  `resume all`, `kill all` (confirmed), `pause`/`resume agent <name>`,
  `theme <name>`, `replay last`.
- Live theme switching: `:theme <name>` and `T` cycling through the eight
  bundled themes, no restart.
- Span annotation on `n`: notes persist to the warm store, annotated
  spans carry a diamond in the tree, and the note renders in the right
  panel's NOTE box, in live view and replay alike.
- Trace tree filter bar on `/`: substring match over operation and tool
  names, non-matching rows dim rather than disappear, `Tab` cycles
  matches even after the bar closes.
- The rest of the navigation map: `g`/`G` jump, `Ctrl+d`/`Ctrl+u` half
  page, `a`/`A` expand/collapse all, `z` zoom panel, `P` fleet pause,
  `Backspace` steps back one level.
- Copy and export: `y` copies the selected span's identity line, `Y` the
  trace id (OSC 52, works over SSH and in tmux, ADR-0033), `e` exports
  the loaded trace as JSON including annotations and captured content.
- Ambient integrations: the terminal tab title carries a fleet summary
  with the worst health band, and opt-in desktop notifications
  (`[notifications]` in config, default off) fire for critical band
  crossings and confirmations waiting on input.
- Animation completeness: sustained pulse on unselected agents that
  crossed a health band for the worse, transient bottom-right toasts for
  the cockpit's own confirmations, and a startup wordmark that dissolves
  into the live cockpit when the first agent connects.
- `r` on the degraded banner actually re-probes the evaluation backend,
  so starting Ollama after Reeve no longer requires a restart; recovery
  raises a `tier 2 evaluation resumed` toast.
- Alerts stack most severe first and `x` dismisses the top one.
- `Ctrl+W`/`Ctrl+U` line editing in every text input.
- ADRs 0032 through 0035.

### Fixed

- Killing an idle agent from the intervention overlay now says there is
  nothing to kill instead of walking through a confirmation the SDK
  would refuse.
- The help overlay lists every key the cockpit answers to; it had not
  been updated since before the view modes existed.
- The Python SDK's generated gRPC bindings no longer require a `grpcio`
  version that is not on PyPI.

### Changed

- Health scoring moved to `reeve-model` (`reeve_model::scoring`), so
  replay recomputes the gauge with exactly the arithmetic the live
  engine used; `reeve-engine` re-exports it unchanged.
- `serde_json` is a runtime dependency of `reeve-renderer` (trace
  export).

## [0.3.0] - 2026-07-06

### Added

- gRPC control channel on `127.0.0.1:4316`: a bidirectional `ControlStream`
  carrying commands to agents and acknowledgments back. The first message
  must be an `AgentHandshake` declaring the agent's identity, framework, and
  capabilities; the server refuses anything else.
- `checkpoint()` primitive in the Rust SDK (`reeve-sdk`). Agents call it at
  safe yield points; it returns `Continue`, `Redirect(instruction)`, or
  `Context(json)`, blocks in place on Pause until Resume, and surfaces Kill
  as an error so the `?` operator propagates it.
- Python SDK (`sdk/python`) with the same checkpoint contract, an OTel
  exporter wired to port 4317, and adapters for LangChain callbacks, OpenAI
  Agents SDK hooks, and the Claude Agent SDK.
- Command dispatcher with a safety layer: duplicate command IDs are dropped,
  commands past `valid_until_ms` are discarded, an unacknowledged command is
  retried once after 30 seconds and then expired. Every dispatch, ack,
  retry, and expiry is appended to `~/.local/share/reeve/audit.log` with
  `issued_by` attribution (`human`, `policy:<rule_id>`, or
  `policy_auto:<rule_id>`).
- Intervention overlay on `i`: pause/resume, redirect, inject context, and
  kill (with confirmation), gated by the capabilities the agent declared.
  Numbered templates load canned instructions into the input; suggested
  interventions appear per failure mode and dispatch on Enter.
- `p` is a pause/resume toggle. Pause state flips on the agent's applied
  ack, the agents panel shows a paused indicator, and a paused agent's
  in-flight traces are exempt from the idle timeout.
- Confirmation modal for policy rules with `requires_confirmation`. Enter
  dispatches with policy attribution, Esc dismisses, and rules with
  `auto_confirm_after_secs` auto-execute when the countdown expires.
- User-defined policy rules loaded from the `policy_rules` table and
  `~/.config/reeve/config.toml` at startup and on `SIGUSR1`. Conditions get
  a dry-run evaluation before entering the live set; invalid ones are
  skipped with a warning.
- Policy rule cooldowns persist across restarts in a `policy_cooldowns`
  table, so a restart no longer resets every cooldown window.
- Policy commands that need no confirmation dispatch straight to the agent
  through the control channel.
- Canonical agent identity: the handshake carries `service_name` and
  `service_instance_id`, and both channels derive the same
  `name:instance` id from them, so commands aimed at an observed agent
  reach its control stream by construction.
- NTP-style clock offset from the handshake's four-timestamp exchange,
  replacing the connection-time approximation for agents that complete it.
- Mid-trace cost prediction with a `predicted_cost` policy primitive and a
  built-in rule that fires before an expensive trace finishes.
- Adaptive Tier 2 sampling driven by the health score trend.
- Structured chain-of-thought output from the LLM judge, stored per
  evaluation for inspection.
- CTX WINDOW section in the right panel with a saturation gauge; left and
  right panels restructured to the final section layout.
- Fatal error screen (full-screen card with retry/quit) and degraded state
  banner (amber, dismissible) for unrecoverable and reduced-capability
  startup conditions.

### Changed

- Policy rule reload moved from `SIGHUP` to `SIGUSR1`. `SIGHUP` keeps its
  default disposition so closing the terminal terminates Reeve.
- Rust SDK configuration takes an agent name plus an optional instance id
  instead of a single `agent_id`, and sets the OTel resource identity from
  them. Agents that need a stable identity across restarts should pass the
  instance id explicitly.

### Fixed

- Pausing an agent for longer than 30 seconds no longer destroys its
  in-flight trace and orphans the spans that arrive after Resume.
- An unopenable audit log shows the fatal error screen instead of taking
  the process down with a panic.
- The Python SDK acks Pause as applied when the agent reaches the yield
  point rather than at resume time, acks Redirect and InjectContext as
  applied at all, deduplicates retried command IDs instead of re-applying
  them, and processes Kill while paused instead of deadlocking.
- Typing into the redirect and inject-context inputs no longer drops
  characters whose keys double as global bindings.
- Reeve no longer survives terminal close as an invisible process holding
  both ports.
- Confirming a policy alert now dispatches the command; the confirmation
  path matched differently-cased command strings and silently did nothing.
- The release workflow no longer fails when rerun against an existing
  GitHub release.

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

[0.4.0]: https://github.com/Dancode-188/reeve/releases/tag/v0.4.0
[0.3.0]: https://github.com/Dancode-188/reeve/releases/tag/v0.3.0
[0.2.0]: https://github.com/Dancode-188/reeve/releases/tag/v0.2.0
[0.1.0]: https://github.com/Dancode-188/reeve/releases/tag/v0.1.0
