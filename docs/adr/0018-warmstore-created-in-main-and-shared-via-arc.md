# 0018: `WarmStore` Created in `main.rs` and Shared via `Arc`

**Status:** Accepted
**Date:** 2026-06-28

## Context

`reeve-ingestion::serve()` originally created the `WarmStore`
internally. It took a `db_path` string and called `WarmStore::open()`
itself. The renderer had no access to warm storage at all.

When the renderer was added, it needed to read from the same SQLite
database: `list_agents()` on startup to populate the initial agent
list, and `list_spans_for_trace()` when a `TraceCompleted` signal
arrives. Two options:

1. Keep `WarmStore` inside `serve()`. Give the renderer a separate
   `WarmStore::open()` call against the same `db_path`. Two distinct
   connections to the same SQLite file.
2. Create one `WarmStore` in `main.rs`. Pass `Arc<WarmStore>` to
   both `serve()` and `run()`.

SQLite in WAL mode handles concurrent readers without issue, so option
1 would work. But two connections to the same file for the same
process is wasteful. It also means `serve()` continues to own DB
initialization, which is the wrong place for it. The binary entry
point should own resource lifecycle.

## Decision

`WarmStore` is opened once in `main.rs`. Both the ingestion pipeline
and the renderer receive `Arc<WarmStore>`. `serve()` now takes
`Arc<WarmStore>` instead of a `db_path`.

`main.rs` also handles the `create_dir_all` for the database parent
directory and reads the `REEVE_DB` environment variable. All startup
resource setup is in one place.

## Consequences

**What gets easier:**
- One SQLite connection for the whole process. No connection overhead
  for a second reader.
- The binary entry point owns DB initialization. `WarmStore::open()`
  fails fast before any network listeners start.
- Adding a third consumer (the engine, a WebSocket feed) follows the
  same pattern: `warm.clone()` at the call site.

**What gets harder:**
- `serve()`'s signature changed. Any caller that previously passed a
  `db_path` string now needs to construct `Arc<WarmStore>` first.
  At v0.1.0 there is exactly one caller (`main.rs`), so this is not
  a real burden.
- `WarmStore` must be `Send + Sync` for `Arc` sharing across tasks.
  It is, because the underlying `rusqlite::Connection` is wrapped in
  a `Mutex` (see ADR-0010).

## Alternatives considered

**Two separate connections (rejected):** Works with SQLite WAL mode
but wastes a connection and spreads DB initialization across two
places. No benefit over the shared `Arc` approach.

**Pass `db_path` to the renderer (rejected):** The renderer would open
its own connection. Same problems as two separate connections plus the
renderer now needs to know about the database path, which belongs in
`main.rs`.
