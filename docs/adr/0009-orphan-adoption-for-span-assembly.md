# 0009: Orphan Adoption for Out-of-Order Span Assembly

**Status:** Accepted
**Date:** 2026-06-26

## Context

OTel exporters send spans in the order they end, not the order they
started. In a typical agent trace, tool call spans end before the LLM
span that triggered them, which ends before the outermost agent
framework span. This means spans arrive at Reeve in leaf-to-root
order: children arrive before their parents.

The assemble stage must reconstruct the parent-child tree from this
stream. A span that arrives before its parent cannot be immediately
placed in the tree because the node it should attach to does not
exist yet. The question is how to hold that span until its parent
arrives, and how to attach it when the parent does arrive.

## Decision

`InFlightTrace` maintains three maps: `spans` (the main tree, indexed
by span ID), `children` (the adjacency list, indexed by parent ID),
and `pending_attachment` (orphans waiting for their parent).

When a span arrives:

1. If the span has no parent, it is the root. It goes directly into
   `spans`.
2. If the span has a parent that is already in `spans`, it goes into
   `spans` and its ID is added to `children[parent_id]`.
3. If the span has a parent that is not yet in `spans`, it goes into
   `pending_attachment`.

After every span insertion, a drain loop runs over
`pending_attachment`. Any span whose parent is now in `spans` is
moved into `spans` and its entry in `children` is created. The loop
repeats until no spans move. One adoption can unblock another: if
span C was waiting for span B, and span B was just adopted from
pending into spans, then span C can now be adopted on the next pass.
A single pass is not sufficient.

Spans that form circular references (span A's parent is B, span B's
parent is A) can never be adopted. They stay in `pending_attachment`
until the idle timeout fires.

When a second span with no parent_span_id arrives for the same trace,
the first root wins. The duplicate is inserted into `spans` without
overriding `root_span_id` or resetting the completion timer. A
warning is logged. This prevents a misbehaving agent from holding a
trace open indefinitely by emitting multiple no-parent spans.

## Consequences

**What gets easier:**
- Span arrival order does not matter. Leaves, middle nodes, and the
  root can arrive in any order and the tree assembles correctly.
- The drain loop is bounded: it terminates when no spans move in a
  pass. In the worst case (a fully reversed arrival order on a linear
  chain of N spans), it runs N times and adopts one span per pass.
  In the common case (batch exports where most spans arrive close
  together), it runs once and adopts many.
- Circular references and genuine orphans are handled without special
  casing: they simply stay in `pending_attachment` until timeout.

**What gets harder:**
- Memory usage scales with the number of in-flight traces plus their
  orphan queues. A misbehaving agent that emits spans with randomized
  parent IDs will accumulate orphans in `pending_attachment` until
  the idle timeout. A memory ceiling per trace is the planned
  mitigation (not yet implemented in v0.1.0).
- The `pending_attachment` map is not visible to the caller without
  the `pending_count()` accessor. A caller cannot inspect which
  specific spans are pending without extending the API.

## Alternatives considered

**Flat buffer with post-processing (rejected):** Accumulate all
arriving spans in an unordered list, then run a single tree-building
pass when the trace is finalized. Rejected because it delays any tree
structure from being visible until completion. The assemble stage is
supposed to expose an in-progress tree to the route stage as spans
arrive, not only at the end. Post-processing would make incremental
updates impossible.

**Sort spans by depth before inserting (rejected):** If the full set
of spans were available, they could be topologically sorted by
parent-child relationship before insertion, making a single pass
sufficient. Rejected because the full set is never available upfront.
Spans arrive as a stream. Sorting requires waiting for all of them,
which collapses back into the flat buffer approach.

**Insert all spans into a flat map, resolve parents lazily at query
time (rejected):** Skip the tree structure entirely at assembly time.
Store all spans flat and only compute parent-child relationships when
the route or renderer asks for them. Rejected because the tree
structure is needed by the completion detection logic (root span
detection requires knowing which spans have no parent), and computing
it lazily on every query would be more expensive than maintaining it
incrementally on arrival.
