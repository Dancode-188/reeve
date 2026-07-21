# Contributing

Thanks for considering it.

## Before you start

Read the [Architecture Decision Records](docs/adr/README.md). Not all of them.
The first three at minimum. They explain why the project is structured the way
it is.

A PR that conflicts with an ADR without addressing it will be asked to add a new
one explaining the departure. Not optional.

## What the codebase looks like

Eight Rust crates in a Cargo workspace. Each has a single responsibility:

- `reeve-model`: domain entities. Everything else imports this.
- `reeve-storage`: hot ring buffer plus SQLite warm tier.
- `reeve-ingestion`: four-stage pipeline (receive, normalize, assemble, route).
- `reeve-engine`: Tier 1 heuristics, Tier 2 LLM judge, policy DSL.
- `reeve-renderer`: Ratatui terminal UI.
- `reeve-intervention`: gRPC control channel plus command dispatch.
- `reeve-sdk`: what Rust agents import.
- `reeve`: binary entry point, thin wrapper.

Don't add dependencies without a reason. When you do add one, put it in
`workspace.dependencies` and reference it with `{ workspace = true }`.

## The standard

The CI is the standard. Your PR needs to pass:

```
cargo fmt --check
cargo clippy -- -D warnings
cargo test
cargo build --release
```

Run them locally before pushing. Clippy is set to deny warnings. If it flags
something and you disagree, the PR explains why.

**Tests.** Write them as you go. A `#[test]` that checks one thing is enough.

**Doc comments.** Public functions get `///` doc comments. `cargo doc` should
produce something readable.

## AI-assisted contributions

Welcome, and held to the same bar as any other. You are responsible for
understanding and testing what you submit. The ADR requirement still applies.
If a PR is substantially AI-generated, say so in the description so review can
weight it accordingly.

## Workflow

1. Open an issue first for anything non-trivial. Design is cheaper before code.
2. Fork and branch from `main`.
3. Branch names: `feat/what-it-does`, `fix/what-it-fixes`.
4. Open a PR against `main`. Fill in the template.
5. Design-level changes need a new ADR or a reference to the existing one you
   are departing from.

## Commit messages

Conventional commits. `type: description`. Types: `feat`, `fix`, `docs`,
`refactor`, `test`, `chore`, `perf`.

Subject line at or under 50 chars where possible, hard limit 72. Body answers
why, not what. The diff shows what. Blank line between subject and body,
always.

## Framework adapters

If you want to add a Python SDK adapter, look at the existing adapters in
`sdk/python/reeve_sdk/adapters/`. The pattern is a callbacks or hooks class that
wraps the Reeve SDK and translates framework lifecycle events into OTel spans.
Open an issue before starting so we can agree on the interface.

## The data model

Don't change entities in `reeve-model` without an ADR. The entities are the
shared contract between every crate in the workspace. A field rename ripples
through storage, the renderer, and the evaluation engine all at once. The ADR
process is not bureaucracy. It is insurance against spending three days on a
refactor that turns out to conflict with something nobody remembered to mention.
