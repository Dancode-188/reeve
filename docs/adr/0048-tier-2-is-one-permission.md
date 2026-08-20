# 0048: Tier 2 Is One Permission, Not Two Stores

**Status:** Accepted
**Date:** 2026-08-20

## Context

ADR-0006 made privacy tier 1 the default and priced the cost
plainly: under tier 1, faithfulness and hallucination detection are
unavailable, and the health score renormalizes around what is left.
That price only makes sense if tier 2 buys those metrics back.

It never has. `extract_content` in `llm_judge.rs` gates both metrics
on three span attributes, `gen_ai.assistant.message.content`,
`gen_ai.output.content` and `gen_ai.completion`. Nothing in the
workspace writes any of them. The judge has therefore returned
`tool_selection` alone since it was written, and the SDK path is no
better off than the proxy. Faithfulness at `0.30` is the largest
weight in `WEIGHTS` and has never produced a row.

ADR-0046 put proxy content on disk in a content-addressed store and
said, in its decision, that nothing in the binary reads it back.
Twelve lines later, in its own consequences, it promises that tier 2
now carries one meaning across both paths, and that an operator who
turned capture on gets content no matter which route the traffic
took. Both cannot hold. With no reader, tier 2 on the proxy path
buys a directory the cockpit ignores, while tier 2 on the SDK path
was supposed to buy the two metrics 0006 named. 0046 also attributes
the no-model line to ADR-0006, which does not draw it there; 0006
draws the line at capture, not at what a local judge may look at
once capture is consented to.

So this is not a conflict between two records. It is a conflict
inside one, and the question is which half survives.

## Decision

The judge may read the capture store.

`Capture` gains a reader keyed by span id. `extract_content` falls
back to it when the span carries no content attribute, and
`extract_context` resolves the round's messages the same way. The
clause in 0046 that forbids a reader is overridden. Everything else
in 0046 stands: the layout, the hashing, the atomic writes, and the
reason the store exists at all.

The tier gate does not move. `capture/` only exists at tier 2, so a
tier 1 operator's spans still carry nothing and the fallback still
finds nothing. Consent stays the single switch it was.

## Consequences

**What gets easier:** faithfulness and hallucination detection can
score for the first time, which moves the tier 1 weight ceiling off
`0.45` and gives the largest weight in the table something to
measure. Tier 2 means one thing again, so an operator reading 0006
gets what 0006 promised. The judge reads the bytes that were
actually sent upstream rather than the subset the translator kept on
the span.

**What gets harder:** scoring now depends on a file layout. That
dependency is narrower than it looks, because rounds are addressed
by span id and only the messages inside them are hash-addressed, so
the re-addressing 0046 warns about reaches context resolution and
not the assistant reply. The reader returns nothing rather than
failing when a file is missing, which degrades to the behaviour we
have today. Evaluation also picks up disk reads on a path that was
previously memory-only, on a detached task after the trace closes.
And `capture/` stops being a pure sink: deleting it now costs a
metric, where 0046 could say it cost no cockpit function.

## Alternatives considered

- **Write the content onto span attributes instead.** This is what
  the judge already expects, and it needs no reader. It also puts
  the entire resent conversation into the database on every turn,
  which is the quadratic growth 0046 exists to avoid.
- **Delete the promise from 0046 and leave the judge as it is.**
  Honest, and cheap. It also fixes tier 2 at one metric of three
  forever, and 0006 sold tier 2 on the other two.
- **Give the judge a separate capture of its own.** Keeps the
  corpus a sink. Costs two copies of the same bytes for a boundary
  that exists only on paper, since both are written by the same
  process under the same consent.
- **Pass content to the judge in memory and never touch disk.**
  Tier 2 evaluation is spawned from `handle_trace_completed` and
  receives spans, not rounds. Holding rounds in memory until their
  trace closes means holding the resent history of every open
  conversation, which is the same growth as the attribute option
  with none of the durability.
