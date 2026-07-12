# 0040: Loop Detection Judges Dominance, Not Volume

**Status:** Accepted
**Date:** 2026-07-11

## Context

The original loop detector penalized any operation name repeating more
than a threshold number of times in a trace. That calibration assumed a
trace holds a handful of spans, which was true of every SDK demo agent
the detector was built against.

On the proxy path, one trace is a whole conversation turn. A real
20-minute Claude Code build turn held 46 chat spans and 61 tool calls
spread across six different tools, scored 0.0 on loop detection, and
dragged the composite health score to critical while doing entirely
normal work. The flagship metric cried wolf on the flagship
integration.

## Decision

**Loop detection scores dominance: one action monopolizing the trace,
not the trace being long.**

- **Carrier spans are excluded.** `gen_ai.chat` and `agent.turn.*` are
  structural: on the proxy path there is exactly one chat per round
  trip by construction, so their counts measure turn length, never
  behavior. Tool names are what the agent chose to do.
- **The threshold becomes minimum evidence.** Below the threshold of
  repeats, no judgment is made. This preserves the original meaning
  for small traces: three repeats of one tool that is also all the
  trace did still scores critical.
- **The score falls with share, not count.** Among non-carrier
  actions, the most-repeated action's share of the total drives the
  score: below half, healthy; from half toward nine tenths, the score
  falls linearly to zero.

**Addendum (2026-07-13): an action is the operation plus its input
fingerprint.** Counting bare tool names made eight reads of eight
different files the same evidence as eight identical reads of one
file; a real session doing legitimate exploration scored 17 because
of it. The proxy now hashes each tool's input in memory when
threading closes the call and stamps only the fingerprint on the
span, so tier 1 privacy holds: equal hashes say two calls did the
same thing without saying what. Dominance counts (operation, hash)
pairs where a fingerprint exists and falls back to the bare
operation where none does, keeping the stricter behavior for spans
that cannot carry one. A loop that varies a trivial argument each
round evades the fingerprint; that is the same class of dilution
already noted under what gets harder, and the same future work
covers it.

## Consequences

**What gets easier:**
- Long agentic turns score by what they did, not how much: a healthy
  mix of tools stays healthy at any turn length.
- An actual runaway, one tool hammered with almost nothing else, still
  scores critical, and the policy alert still fires.

**What gets harder:**
- A slow loop diluted by variety can hide: an agent alternating two
  tools in a genuine loop sits at 50% share and scores healthy.
  Dominance by a set of actions rather than one is future work.
- The carrier exclusion list is a judgment: a client that emits some
  other structural span per round trip would need its carrier added.

## Alternatives considered

- **Consecutive-run detection (rejected for now):** score runs of
  identical calls rather than totals. Better at catching tight loops
  inside long turns, but real traffic interleaves task-tracking calls
  between repeated actions, breaking runs that are still loops.
  Share-based dominance is robust to interleaving.
- **Raise the absolute threshold (rejected):** any fixed count is
  wrong at some turn length; the 20-minute turn would need a threshold
  near 50, which would blind the detector for small traces.
- **Normalize the threshold by trace size (rejected):** equivalent to
  share-based scoring but with the threshold semantics muddled;
  scoring share directly says what is meant.
