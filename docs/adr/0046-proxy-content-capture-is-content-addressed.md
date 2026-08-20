# 0046: Proxy Content Capture Is a Content-Addressed Store on Disk

**Status:** Accepted
**Date:** 2026-08-18
**Amended by:** [0048](./0048-tier-2-is-one-permission.md)

## Context

ADR-0006 made content capture opt-in and described tier 2 in terms of
one field: `SpanEvent.content` is `None` unless a caller asks for it.
That is an accurate description of the SDK path, where the agent
reports what it did and the translator writes the content into the
database alongside everything else. ADR-0035 then fixed the tier as
read once at startup, with a consent line, failing closed.

Neither says anything about the proxy, because when they were written
the proxy did not capture content. What it actually did was parse
every request body to fingerprint the conversation for threading
(ADR-0037) and then drop it. So an operator who had turned tier 2 on,
signed the consent line, and was running through the proxy got a tier
1 corpus and was never told the difference.

Storing what the proxy sees is not the same problem as storing what an
SDK reports. The SDK reports one step at a time. A proxied request
carries the entire conversation up to that point, and carries it again
on the next turn, and again on the one after: the history is resent in
full every round. Keeping each body whole therefore costs a total that
grows with the square of the turn count, on traffic where 1.34 MB is
an ordinary request size rather than a worst case. The naive version
of this feature fills a disk with copies of the same paragraph.

## Decision

**Captured rounds live next to the database, not in it.**
The directory is `db_path.parent().join("capture")`, gated on
`privacy_tier >= 2`, and opened during startup before anything is
spawned, so a path that cannot be written refuses to start rather than
failing quietly and surfacing weeks later as a collection nobody ever
wrote.

**Deduplication reuses the fingerprints threading already computes.**
A round is written as `capture/rounds/<started_at_ms>-<span_id>.json`,
and the `messages` array inside its request is replaced by a list of
names. Each distinct message is stored once at
`capture/messages/<first two hex>/<sixteen hex>.json`, sharded by the
first byte because a long-running corpus accumulates tens of thousands
of them and one flat directory is unpleasant to list or browse. A
conversation's hundredth turn therefore costs one new message file and
a list of names for everything before it. Measured on live proxy
traffic, 26,856 message references occupy 2,205 files, a shade over 12
to 1. The names hold only where the hashes line up one to one with the
array they came from; where they do not, or where none are supplied,
the messages are stored inline, because pairing a message with the
wrong fingerprint would leave every later round sharing that prefix
pointing at the wrong content.

**Writing is detached, and a failed write is logged and abandoned.**
`Capture::record` hands the round to a blocking pool and returns, so
no live turn ever waits on a write. Each file arrives through a
temporary name and a rename, because nothing revisits one of these
files once it exists: a half-written one would stay half-written for
as long as the collection does. Errors stop at the log line. Dropping
a round leaves a hole in what was collected, while raising it would
cost the operator a working session, and no collection is worth that
trade.

**Nothing in the binary reads the directory back.** Replay continues
to work from span events exactly as it did, `capture/` has no reader,
and deleting it costs no cockpit function. The files are there to be
opened and read by a person. No feature is computed from what they
hold and nothing in them reaches a model, which is where ADR-0006
already drew the line.

## Consequences

**What gets easier:**
- Tier 2 finally means the same thing on both paths. The operator who
  consented to content capture gets content, whichever way traffic
  reaches Reeve.
- The stored corpus is the raw request and response, not Reeve's
  interpretation of them. A later question about what was actually
  sent can be answered from the bytes rather than from whatever fields
  the ingestion path happened to keep.
- Storage grows with the conversation instead of with its turns, which
  is what makes capture affordable enough to leave running for weeks.

**What gets harder:**
- A tier 2 corpus now lives in two places with two shapes: span events
  in SQLite from the SDK path, JSON files on disk from the proxy.
  Anything that wants both has to know both.
- Retention does not reach it. `max_trace_age_days` prunes the
  database; `capture/` grows until someone deletes it, and deleting it
  is currently all or nothing, because messages are shared and no
  round owns the files it references.
- Reading the corpus means writing something to read it. There is no
  view, no export, and no command, so the first question asked of the
  data costs a script.
- The names are the hashes, and the hashes come from threading. A
  change to how threading fingerprints a message re-addresses every
  message written after it, and old and new files sit in the same
  directory looking identical.

## Alternatives considered

- **Write span events, as the SDK path does.** One store, one shape,
  and ADR-0006 already describes it. It fails on the resent history:
  the content the proxy holds is a whole conversation, so every turn
  would write the previous turn's events again, and the database that
  the cockpit reads on every tick would carry that weight.
- **A content-addressed table inside SQLite.** The same deduplication
  with one store instead of two, and no directory layout to document.
  It loses on what the store actually is: written once, never queried,
  with no reader in the binary. A database would buy transactions and
  indexes for none of that, while growing the file the renderer polls
  on every tick.
- **Store whole bodies per round, no deduplication.** Simplest thing
  that works, and it does work for an afternoon. At 1.34 MB a request
  and a history that grows every turn, it does not survive a week of
  real traffic, which is the timescale the store exists for.
- **Compress instead of deduplicating.** Cheaper to implement and
  wrong in an interesting way: compression shrinks each copy, while
  the redundancy here is between copies. Content addressing removes
  the duplicate; compression pays for it at a discount, and pays again
  every turn.
- **Capture nothing on the proxy path, and say so.** The honest
  version of the status quo, and it would have been an improvement on
  the silence. It was rejected because the proxy path is the one that
  needs no agent instrumentation, so it is the path most likely to be
  the only one an operator has.
