# Architecture Decision Records

This directory is where reasoning lives. Every significant design choice in Reeve has a
record here: what was decided, why, and what alternatives were considered and rejected.

| # | Title | Status | Date |
|---|-------|--------|------|
| [0001](./0001-two-channel-architecture.md) | Two-Channel Architecture | Accepted | 2026-06-23 |
| [0002](./0002-local-first-llm-judge.md) | Local-First LLM Judge | Accepted | 2026-06-23 |
| [0003](./0003-apache-2-license.md) | Apache 2.0 License | Accepted | 2026-06-23 |
| [0004](./0004-clock-offset-estimation.md) | Connection-Time Clock Offset Estimation | Accepted | 2026-06-25 |
| [0005](./0005-versioned-attribute-translator.md) | Versioned AttributeTranslator Pattern | Accepted | 2026-06-26 |
| [0006](./0006-privacy-tier-1-default.md) | Privacy Tier 1 as the Default | Accepted | 2026-06-26 |
| [0007](./0007-weight-renormalization.md) | Weight Renormalization for Missing Evaluation Metrics | Accepted | 2026-06-30 |
| [0008](./0008-root-triggered-straggler-window.md) | Root-Triggered Straggler Window for Trace Completion | Accepted | 2026-06-26 |
| [0009](./0009-orphan-adoption-for-span-assembly.md) | Orphan Adoption for Out-of-Order Span Assembly | Accepted | 2026-06-26 |
| [0010](./0010-rusqlite-over-sqlx-for-warm-tier.md) | rusqlite over sqlx for the Warm Tier | Accepted | 2026-06-26 |
| [0011](./0011-root-span-determines-trace-failure.md) | Root Span Status Determines Trace-Level Failure | Accepted | 2026-06-26 |
| [0012](./0012-lazy-agent-registration-at-trace-finalization.md) | Lazy Agent Registration at Trace Finalization | Accepted | 2026-06-26 |
| [0013](./0013-proto-codegen-boundary.md) | Proto Codegen Boundary Between reeve-model and reeve-intervention | Accepted | 2026-06-27 |
| [0014](./0014-trace-status-seven-state-machine.md) | Seven-State Trace Status Machine | Accepted | 2026-06-27 |
| [0015](./0015-broadcast-channel-for-pipeline-renderer-signal-bus.md) | `broadcast::channel` for the Pipeline-to-Renderer Signal Bus | Accepted | 2026-06-28 |
| [0016](./0016-renderer-subscribes-to-route-stage-directly.md) | Renderer Subscribes to the Route Stage Directly | Accepted | 2026-06-28 |
| [0017](./0017-indexmap-for-agent-registry-in-renderer.md) | `IndexMap` for the Agent Registry in the Renderer | Accepted | 2026-06-28 |
| [0018](./0018-warmstore-created-in-main-and-shared-via-arc.md) | `WarmStore` Created in `main.rs` and Shared via `Arc` | Accepted | 2026-06-28 |
| [0019](./0019-separate-ingestion-and-engine-event-channels.md) | Separate IngestionEvent and EngineEvent Channels | Accepted | 2026-06-28 |
| [0020](./0020-composite-health-score.md) | Composite Health Score Design | Accepted | 2026-06-30 |
| [0021](./0021-tier2-llm-judge.md) | Tier 2 LLM Judge via Ollama with Self-Consistency Scoring | Accepted | 2026-06-30 |
| [0022](./0022-evalexpr-policy-dsl.md) | evalexpr for Policy Rule Conditions | Accepted | 2026-06-30 |
| [0023](./0023-grpc-control-channel-protocol.md) | gRPC Control Channel Protocol | Accepted | 2026-07-04 |
| [0024](./0024-command-expiry-via-valid-until-ms.md) | Command Expiry via `valid_until_ms` | Accepted | 2026-07-04 |
| [0025](./0025-audit-trail-format-and-attribution.md) | Audit Trail Format and `issued_by` Attribution | Accepted | 2026-07-04 |
| [0026](./0026-ntp-style-clock-sync-via-control-handshake.md) | NTP-Style Clock Synchronization via the Control Handshake | Accepted | 2026-07-06 |
| [0027](./0027-checkpoint-as-polling-primitive.md) | `checkpoint()` as a Pull-Based Polling Primitive | Accepted | 2026-07-05 |
| [0028](./0028-python-sdk-adapter-via-framework-callbacks.md) | Python SDK Adapter via Framework Callback Handlers | Accepted | 2026-07-05 |
| [0029](./0029-engine-to-dispatcher-channel.md) | Engine-to-Dispatcher Channel to Preserve the `reeve-engine` Dependency Boundary | Accepted | 2026-07-06 |
| [0030](./0030-instant-reconstitution-from-wall-clock-timestamps.md) | Reconstituting `Instant` Values from Wall-Clock Timestamps for Cooldown Persistence | Accepted | 2026-07-06 |
| [0031](./0031-canonical-agent-identity-across-channels.md) | Canonical Agent Identity Composed by One Function Across Both Channels | Accepted | 2026-07-06 |
| [0032](./0032-cross-trace-outcome-measurement.md) | Cross-Trace Window for Intervention Outcome Measurement | Accepted | 2026-07-07 |
| [0033](./0033-clipboard-via-osc-52.md) | Clipboard via OSC 52, Not a Native Library | Accepted | 2026-07-08 |
| [0034](./0034-effectiveness-memory-keyed-by-rule-identity.md) | Effectiveness Memory Keyed by Rule Identity, Not Metric | Accepted | 2026-07-08 |
| [0035](./0035-privacy-tier-read-once-fails-closed.md) | Privacy Tier Read Once at Startup, Failing Closed | Accepted | 2026-07-08 |
| [0036](./0036-proxy-agent-identity-from-user-agent.md) | Proxy Agent Identity Derived from the User-Agent Product Token | Accepted | 2026-07-08 |
| [0037](./0037-conversation-threading-from-message-prefixes.md) | Conversation Threading from Message Prefixes | Accepted | 2026-07-09 |

## Format

```markdown
# 000N: [Title]

**Status:** Accepted
**Date:** YYYY-MM-DD

## Context
## Decision
## Consequences
## Alternatives considered
```

Status values: `Accepted`, `Superseded by [NNNN]`, `Deprecated`, `Proposed`
