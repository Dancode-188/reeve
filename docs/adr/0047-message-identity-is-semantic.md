# 0047: Message Identity Is Semantic, and Threading Counts Its Own Misses

**Status:** Accepted
**Date:** 2026-08-18

## Context

ADR-0037 threads proxy conversations by fingerprinting each entry of a
request's `messages` array and matching the result against the
prefixes of known conversations. It also settled in advance what a
mismatch means: a new conversation starts, which is what the proxy did
before threading existed, so "degradation is graceful by construction,
and the worst case equals the naive design."

Two failures have since taken the same shape. A client re-encodes a
message it has already sent, the prefix stops agreeing somewhere near
its end, and requests that continue a conversation open a new one
instead. The first was `cache_control` markers moving forward as a
conversation grows (#178, filed and closed 2026-07-10, the day after
0037 was accepted). The second was Claude Code's own token-budget
system message, sent one way when it first appears and another way on
every replay after that (#308, 2026-08-15). The second was total: on
the traffic threading was built for, it had never worked.

Both were found by hand, reading request bodies. Neither was found by
Reeve, because a miss and a real new conversation produce identical
output, and the record had already said that this was the acceptable
case. What the claim bounds is the damage from one miss. It says
nothing about how often a miss happens, and the missing half went
unnoticed because the half that was there reads like a safety
argument.

The warm store shows the cost. Of 1,039 proxy traces recorded before
the fix, mean span count is 2.37 and 40.3% hold a single span; of the
112 after it, mean is 3.67 and 0.9% hold one. A root plus one chat
span is what a turn that never continued looks like. ADR-0037 also
promised that "evaluation and policy see task-shaped traces from the
proxy path, so health scores and cost rules mean the same thing they
mean for SDK agents." For that whole window they did not.

None of this reverses 0037, which keeps its status and its decisions.
What follows is the rule it left implicit and the instrument it never
had.

## Decision

**A message fingerprint is taken over what the message says, not over
how it was serialized.** `hash_message` puts the value through
`canonical_message` before hashing it, and anything a client is free
to vary while meaning the same thing is stripped there. The two
rewrites that exist are documented at that function with the traffic
that forced them; the decision here is that the list is expected to
grow, and that a new encoding costs one branch in one place rather
than a correction at every site that compares two messages.

This now binds more than threading. ADR-0046 names files in the
capture store by these fingerprints, so a hash sensitive to
serialization would not only split conversations, it would store one
paragraph twice under two names and lose the deduplication that keeps
capture cheap.

**Threading stamps the evidence for each placement on the span, so the
graceful-degradation claim can be checked instead of assumed.** Every
chat span now carries `reeve.threading.new_conversation`,
`reeve.threading.matched_prefix` and `reeve.threading.candidates`
alongside the `reeve.proxy.context_messages` that 0037 already
recorded. `matched_prefix` is how deep the nearest known conversation
went before the two stopped agreeing, and `candidates` is how many
conversations were open for that agent at the time. Together they
separate a first request from an unrelated one, and both of those from
the case where a long history matched almost to its end and then
failed, which is the signature #178 and #308 each wore unread.

The general form of this is the part worth carrying: an argument that
a failure is survivable is incomplete until something emits the rate
at which it fires.

## Consequences

**What gets easier:**
- Whether threading is working is answerable from the corpus, by
  anyone, without reproducing traffic or instrumenting a client.
- The next encoding divergence shows up as misses against a long
  `matched_prefix`, rather than waiting for someone to compare two
  request bodies by eye.
- A placement can be reconstructed after the fact, because the
  evidence sits beside the outcome instead of in proxy memory that a
  restart forgets.

**What gets harder:**
- The counters are written and nothing consumes them. Any question
  put to them means opening the database by hand, so an instrument
  built to catch a silent failure is only as awake as whoever
  remembers to check it.
- Canonicalization is open-ended, and a branch that strips too much
  threads two genuinely different messages together, which is quieter
  than the failure it replaces.
- Traces recorded before the fix cannot be repaired in place. Their
  grouping is already written into stored spans, and capture files
  from that window are named under the fingerprints of the moment, so
  one message sits on disk under two names either side of the
  boundary. Any statistic drawn across it mixes two populations.

## Alternatives considered

- **Mark ADR-0037 superseded.** The index in this directory reserves
  supersession for a decision that was reversed, and 0037's decisions
  all still hold. Marking it would send readers looking for a
  replacement design that does not exist, and force this record to
  restate 0037 unchanged to be usable on its own.
- **Amend ADR-0037 in place.** Cheapest to read, and it destroys the
  evidence of how the reasoning failed. The wrong sentence is the most
  useful thing in that record now.
- **Fix the hash and write nothing.** What happened after #178, which
  is why #308 was possible. Two rewrites of one function in five weeks
  is the pattern worth recording, more than either rewrite.
- **Canonicalize on arrival instead of at the hash.** Rewrite each
  request once at the boundary and let everything downstream compare
  bytes. That puts a lossy transform on the path that captures content
  for an operator to read, and what reaches the disk should be what
  the client actually sent.
- **Alert on a low threading rate rather than record the evidence.**
  An alert needs a threshold, and none was defensible before the
  counters existed to say what normal looks like. An alert would be
  built out of them anyway.
