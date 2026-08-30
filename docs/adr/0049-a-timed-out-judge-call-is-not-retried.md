# 0049: A Judge Call That Ran Out of Time Is Not Retried

**Status:** Accepted
**Date:** 2026-08-25
**Amends:** [0021](./0021-tier2-llm-judge.md)
**Amended by:** [0050](./0050-the-judge-dispatches-one-call-at-a-time.md)

## Context

ADR-0021 set the retry behaviour for the Tier 2 judge: each call
retries up to three times with exponential backoff, and on exhaustion
the metric is skipped and the score renormalizes per ADR-0007. The
deadline those retries run under was never written down. In code it
was 30 seconds.

On a CPU-only backend that number is not a safety margin, it is the
middle of the distribution. A `tool_selection` prompt is 246
characters at the median and costs about 5 seconds; a faithfulness
prompt is 8,406 and costs about 300. Of the generate requests my
backend retained, 4,781 failures ended at almost exactly 30.0 seconds
against `aborting completion request due to client closing the
connection`. That is Reeve hanging up on work still in progress, not
a backend refusing it.

Retrying made it worse rather than better. Three attempts at a prompt
whose cost is a property of its length is the same abandoned work
billed three times, and every exhaustion warning in my log carried
`phrasing="a"`, so no pair ever reached its second phrasing. The
result is that faithfulness, at `0.30` the largest weight in the
table, has never produced a row.

The deadline was also doing two jobs. Whether the backend is alive is
a question worth answering in milliseconds; how long a large prompt
may take to answer is a different question with a different scale, and
one number cannot be right for both.

## Decision

A call that ran past the deadline is not retried. Retries stay in
place for every other failure, which is what they were for: a refused
connection, a reset, a reply that would not parse. Only the timeout is
treated as final, because asking the same question again cannot make
it cheaper.

The deadline for one evaluation call moves to 15 minutes, and that
number comes off the measured curve rather than off a round figure.
Prompt evaluation on this backend is superlinear: 58 tokens cost 6
seconds, 640 cost 67, 1,268 cost 195, so the seconds each additional
token adds roughly doubles over that range. The context budget caps a
prompt at 12,000 characters. Fitted through the long end and evaluated
there, the largest prompt the budget permits costs 320 seconds if its
text tokenizes as loosely as the probe's filler did and 600 if it
tokenizes at the four characters per token the budget itself assumes.
Writing out a claims list adds another 30 to 105. The worst call the
budget can produce is therefore somewhere between six and twelve
minutes, and 15 is a quarter again over the pessimistic end rather
than double the optimistic one.

That figure bounds the work of one call and nothing else. A dispatch
already runs on a detached task, so a ceiling this generous costs an
idle task and no more. The volume of judging is the Tier 2 sample
rate's business, not this ceiling's.

Liveness keeps its own deadline: three seconds against `/api/tags`,
unchanged. The two questions are separated on purpose.

The requests now carry `keep_alive`, so the model stays resident
between the six calls of a dispatch instead of unloading on the
backend's default idle and charging the next burst for a reload.

Everything else in ADR-0021 stands: the two phrasings, the divergence
thresholds, the exclusion of Low confidence results from the score,
and the renormalization on a dropped metric.

## Consequences

**What gets easier:**
- The metrics that carry the most weight can finish. A judge that
  never returns a row is not a conservative judge, it is an absent
  one, and the health score has been tier 1 renormalized while
  reporting itself as more than that.
- An exhaustion warning now means something specific. It says the
  backend could not answer, rather than saying the answer did not fit
  in a budget chosen without measuring what it had to hold.
- One slow call no longer costs three. A dispatch that gives up gives
  up once.

**What gets harder:**
- A Tier 2 result can now arrive a long time after the trace that
  produced it. The two-tier update in ADR-0020 already assumes this,
  but the gap it has to absorb is much wider than it was.
- A backend that hangs without closing the connection now holds a
  task for 15 minutes rather than 30 seconds. Nothing else waits on
  that task, but it is a real change in how long a stuck dispatch
  stays visible.
- The ceiling is no longer load-bearing, so nothing in the client
  bounds the cost of a prompt. What bounds it is the context budget,
  and that budget is now the only thing standing between a large
  conversation and a very slow call.
- A call can now be starved by other calls instead of by its own
  size. `run_tier2` is spawned per trace with nothing counting how
  many are in flight, and a backend serving one request at a time
  makes the deadline cover queue time as well as inference. The first
  hour under this decision showed it: four dispatches held
  connections at once, and a 254 character `tool_selection` prompt,
  which is about five seconds of work, was dropped at the full 15
  minutes without ever being served. Bursts make this the common
  case rather than the rare one, because 27.3% of agent traces score
  at the tier 1 floor and a score below 45.5 sets the sample rate to
  1.0, so those traces dispatch every time.

## Alternatives considered

- **Keep the 30 second ceiling and shrink the prompt to fit it.**
  This is the change that would have to be large enough to matter:
  8,406 characters to something near 246. The context budget exists
  to spend a bounded amount of the conversation deliberately, and
  cutting it by that much would evaluate a turn on almost none of its
  own context. Shrinking the prompt is worth doing on its own merits
  and is not a substitute for letting a call finish.
- **Raise the ceiling and keep all three retries.** Three attempts at
  15 minutes is 45 minutes of a task waiting on work that failed the
  same way each time. The retries were never wrong, they were aimed
  at the wrong failure.
- **Drop the second consistency call for the expensive metrics.** This
  halves the cost and takes the confidence signal with it, which is
  the part of ADR-0021 that makes a small local judge honest about
  itself. Paying for both calls is the point of that record.
- **Cap concurrent dispatches in the same change.** The measurement
  that used to argue against a cap, a queue depth topping out at 5
  across 1,094 requests, was taken while every expensive call was
  being abandoned at 30 seconds, and a dispatch that gives up in
  ninety seconds cannot build a queue. That number does not survive
  this decision and should not be cited as though it does. The
  starved 254 character prompt says a cap is needed, not that it is
  optional. It is left out here because it answers a different
  question: this record is about how long one call may take, and a
  cap is about how many calls may be outstanding. Doing both at once
  would have shipped a bound guessed before there was a completed
  dispatch to size it from.
