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
| [0008](./0008-root-triggered-straggler-window.md) | Root-Triggered Straggler Window for Trace Completion | Accepted | 2026-06-26 |
| [0009](./0009-orphan-adoption-for-span-assembly.md) | Orphan Adoption for Out-of-Order Span Assembly | Accepted | 2026-06-26 |
| [0010](./0010-rusqlite-over-sqlx-for-warm-tier.md) | rusqlite over sqlx for the Warm Tier | Accepted | 2026-06-26 |
| [0011](./0011-root-span-determines-trace-failure.md) | Root Span Status Determines Trace-Level Failure | Accepted | 2026-06-26 |
| [0012](./0012-lazy-agent-registration-at-trace-finalization.md) | Lazy Agent Registration at Trace Finalization | Accepted | 2026-06-26 |
| [0013](./0013-proto-codegen-boundary.md) | Proto Codegen Boundary Between reeve-model and reeve-intervention | Accepted | 2026-06-27 |
| [0014](./0014-trace-status-seven-state-machine.md) | Seven-State Trace Status Machine | Accepted | 2026-06-27 |

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
