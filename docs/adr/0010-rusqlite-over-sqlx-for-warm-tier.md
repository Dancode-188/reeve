# 0010: rusqlite over sqlx for the Warm Tier

**Status:** Accepted
**Date:** 2026-06-26

## Context

`reeve-storage`'s warm tier persists completed traces to a local
SQLite database. The choice of SQLite driver matters because
`reeve-ingestion` runs an async Tokio runtime, and the warm tier
is called from within that runtime on every trace finalization event.

Two Rust SQLite drivers are in common use: `rusqlite`, which exposes
SQLite's native synchronous API directly, and `sqlx`, which wraps
SQLite in an async interface. The question is which to use, and what
the async integration story looks like under each.

## Decision

`reeve-storage` uses `rusqlite` with the `bundled` feature. Every
method on `WarmStore` that touches the database wraps its `rusqlite`
call in `tokio::task::spawn_blocking` through a single shared
`with_conn` helper. The sync-in-async boundary is explicit and
visible in one place rather than hidden behind a trait.

Migrations run through a small hand-rolled runner: a
`_schema_migrations` table records which SQL files have been applied,
and a loop over compile-time-embedded SQL files applies any that have
not. No migration crate is used.

The `bundled` feature compiles SQLite directly into the binary. There
is no runtime dependency on a system SQLite installation and no
`apt install libsqlite3-dev` step in setup documentation.

## Consequences

**What gets easier:**
- The sync-in-async bridge is visible exactly once, in `with_conn`.
  A reader can see immediately where blocking work is handed off to
  the thread pool without tracing through multiple layers of trait
  implementations.
- No system SQLite required. The binary is self-contained. This
  matters for single-binary distribution, which is a goal for v1.0.0.
- `rusqlite`'s API is stable and close to SQLite's own documentation.
  Queries written against SQLite docs work without translation.

**What gets harder:**
- `tokio::task::spawn_blocking` has a non-zero cost per call. For
  Reeve's access pattern (one or a few writes per finalized trace,
  not thousands per second), this cost is negligible.
- There is no built-in migration framework. The hand-rolled runner
  must be maintained. It is simple enough (under 50 lines) that this
  is not a meaningful burden.

## Alternatives considered

**sqlx (rejected):** `sqlx`'s async SQLite interface is synchronous
SQLite wrapped in an async facade. SQLite itself has no async I/O:
it reads and writes a local file. `sqlx` achieves the async interface
by running SQLite on a dedicated thread pool internally, which is
exactly what `with_conn` does explicitly with `spawn_blocking`. The
difference is that `sqlx` hides the thread pool behind a connection
pool abstraction and a query macro system, adding complexity (the
`sqlx` compile-time query checking requires a live database at build
time or an offline cache file) without buying any real concurrency
benefit for a tool writing sequential trace flushes to a single local
file. The explicit `spawn_blocking` approach in `rusqlite` is the
honest version of what `sqlx` does internally.

**sled (rejected):** `sled` is an embedded key-value store with a
native async API. Rejected because the warm tier needs SQL: spans are
queried by trace ID, by time range, by agent, and eventually by
health score. A key-value store would require Reeve to implement
secondary indexing manually. SQLite provides that for free.

**An ORM (Diesel, SeaORM) (rejected):** ORMs add a schema abstraction
layer that is useful when the application model and the database
schema diverge significantly, or when the same codebase targets
multiple databases. Reeve targets exactly one database (SQLite) and
the schema is simple and append-heavy. An ORM would add build
complexity (Diesel requires a CLI and a schema file; SeaORM requires
code generation) for no benefit over writing SQL directly.
