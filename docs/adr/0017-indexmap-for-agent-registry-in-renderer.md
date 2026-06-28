# 0017: `IndexMap` for the Agent Registry in the Renderer

**Status:** Accepted
**Date:** 2026-06-28

## Context

The renderer maintains a live registry of known agents in `AppState`.
It needs two things from this registry simultaneously:

1. O(1) lookup by `AgentId` to apply incoming signals (status changes,
   cost updates) to the right agent.
2. A stable display order so the agent list does not jump around as
   new agents connect or statuses change.

The obvious starting point is `HashMap<AgentId, AgentState>`. Lookup
is O(1). But `HashMap` does not preserve insertion order. The display
order would be arbitrary and would shift as the map re-hashes.

A `Vec<AgentState>` with a companion `HashMap<AgentId, usize>` for
index lookup gives stable order and O(1) lookup. But updating an
agent requires two writes (the map and the vec), and removing one
requires patching the index map for all elements that shift. Keeping
them in sync is error-prone.

`BTreeMap<AgentId, AgentState>` gives sorted order. But the sort key
is the agent ID, which is a UUID. Sorted-by-UUID is not meaningful
for a user watching a list of running agents.

## Decision

`IndexMap<AgentId, AgentState>` from the `indexmap` crate is used
for the agent registry. `IndexMap` combines hash-map lookup (O(1))
with insertion-order iteration. Agents appear in the order they first
connected. The selected-agent cursor is a `usize` index, which maps
directly to `IndexMap::get_index()`.

`indexmap` is already in the workspace `Cargo.toml` with the `serde`
feature enabled for potential future serialization.

## Consequences

**What gets easier:**
- Applying a signal to an agent is `agents.get_mut(&agent_id)`, O(1).
- Iterating for display gives agents in connection order, stable
  across signal updates.
- The selection cursor (`selected_agent: Option<usize>`) is a direct
  index into the map. No ID-to-index translation needed.

**What gets harder:**
- `IndexMap` is an external crate. It is widely used and stable, but
  it is a dependency that `HashMap` would not add.
- Insertion order is first-seen order, not any user-configurable sort.
  Sorting by name or cost would require a separate index or a
  temporary sorted copy for rendering.

## Alternatives considered

**`HashMap` (rejected):** Lookup is O(1) but iteration order is
arbitrary. The agent list would reorder unpredictably as the map
grows.

**`Vec` + `HashMap` index (rejected):** Achieves the same properties
as `IndexMap` but requires manual synchronization of two data
structures. The `indexmap` crate solves exactly this problem.

**`BTreeMap` (rejected):** Sorted by key. Keys are `AgentId` (UUID).
Sorted-by-UUID order is not useful for display.
