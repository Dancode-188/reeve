# 0051: The Judge Refuses Work at Admission, Not at a Timeout

**Status:** Accepted
**Date:** 2026-09-02
**Amends:** [0050](./0050-the-judge-dispatches-one-call-at-a-time.md)

## Context

ADR-0050 capped the judge at one dispatch and bounded the wait for that
slot at five minutes. It said plainly that the bound was the one guessed
quantity in the record, and it wrote its own falsification test: if the
bound is right, give up reasons move from the deadline to the wait, and
the deadline drops that remain are the largest prompts.

**The test passed, and it was the wrong test.** Scoped to the one slot
era, 238 calls over 50 hours: 151 completed, 42 dropped at the wait, 27
found no claims to grade, 17 stood aside, and exactly 1 hit the
deadline. Reasons moved as predicted and the single deadline drop is the
largest prompt in the window. Two windows appear below and they are not
the same: the one slot era is all 50 hours, and a narrower 27 hour
window inside it is where waiting and service are timed apart, which is
where every split figure comes from. But the test only asked whether the
failures had moved. It never asked whether the new failure was refusing
work the backend would have finished. It was.

**The wait bound is shorter than the deadline it waits behind.**
`DISPATCH_WAIT` is five minutes and `EVAL_TIMEOUT` is ten. A call may
legitimately hold the slot for twice as long as a call is permitted to
wait for it. Nothing has to go wrong for a waiter to be refused while
the holder is still inside the budget this crate granted it: the
constants guarantee it. In the split clock window, 6 of the 120 calls
that were served ran past five minutes, so the case is not hypothetical.

**The waits that got served are pressed flat against the limit.** Of the
149 single attempt calls that obtained the slot, 10 waited longer than
270 seconds and the longest waited 298, against a bound of 300. A
distribution that piles up inside the last tenth before its own cap and
then stops is being cut off, not ending on its own. These counts move
with every hour the pilot runs, and so far they have moved one way.

**The slot is idle most of the time.** Over the window where waiting and
service are timed apart, the one slot was held for 4.5 hours out of 27.
Utilisation is 16.7 per cent. A queue that refuses 42 calls while its
server sits idle five sixths of the window is not short of capacity. It
is short of any bound on how much work arrives at once.

**Nothing counts the traces.** Tier 2 spawns a task per sampled trace
and there is no cap on how many run together. Within a trace the metrics
are awaited in turn, so a trace never queues against itself; every call
in the queue belongs to a different trace. Four traces have been grading
at once in this window. ADR-0050 rejected a per trace cap as aimed at
the wrong unit, on the reasoning that one trace fires six calls at the
backend together. It does not. That rejection was argued against a
dispatch shape the code does not have, and it is the reason the missing
bound is still missing.

**The cost lands on completeness, not on volume.** A metric is only
usable when both phrasings score, so a broken half is discarded whole.
Of 69 traces that got as far as a dispatch, 22 ended with no usable
metric pair at all and 3 with all three. Counting a
metric that legitimately had no claims as no loss, 45 of 69 still lost
at least one metric they had attempted. The corpus is not gaining 69
partly graded traces. It is gaining a handful of gradeable ones and a
long tail of records of what the grader could not finish, with the
missingness correlated with load, which is the bias ADR-0050 itself
named as the harder of the two to reason about after the fact.

**Waiting to be refused is not free.** In the split clock window the
calls that were eventually turned away had spent 113 minutes in the
queue first. That is time a trace held an outstanding verdict that was
never going to arrive.

**A retry can throw away work already done.** ADR-0049 stopped retrying
a call that ran out of time, and both the wait bound and the yield rule
mark their refusals as not retryable, with tests asserting that a retry
would only rejoin the queue that refused it. A response that arrives
and does not parse is the one case still marked retryable, and its
retry re-enters the queue as a fresh arrival with no such guard. Twice
in this era a call was served, failed to parse, and was then refused at
the wait bound on its second attempt, discarding slot time already
spent. Two of 60 is not a pattern, but it is the argument in its
sharpest form: the queue charges for the expensive part before it
decides whether to refuse.

**The outside work agrees, and has for a decade.** Controlled Delay,
written for network buffers and since ported to request queues, sheds on
how long work has waited rather than on how much of it is waiting,
precisely so a burst at low utilisation is not read as overload. Uber's
account of leaving static rate limits makes the second point: refuse as
early as possible, because work refused after it has been carried
through the expensive part has cost everything and bought nothing. The
serving literature has converged on the same rule under the name early
rejection, where an arriving request is admitted only if its projected
completion, given what is already queued, still meets the deadline, and
is refused outright otherwise. Reeve's queue does the opposite of all
three. It has a fixed threshold, it fires on transient bursts, and it
fires at the end of a five minute wait.

## Decision

Three changes, and no one of them is sufficient alone. Raising the wait
without an admission rule lets the queue grow until the new bound is hit
in its turn. An admission rule without the raised wait still refuses
callers while the holder is inside its budget. A trace cap without
either leaves both constants wrong.

**A call is never refused while the call ahead of it is inside its own
budget.** `DISPATCH_WAIT` rises to equal `EVAL_TIMEOUT`. The guarantee
this buys is narrow and worth stating exactly: the call at the head of
the queue always outlives the holder. The second call in line gets no
such promise. It is still admitted, because its projected wait fits
inside the raised bound, but nothing guarantees it will be served, and
the distance between admitted and guaranteed is what the next change
governs.

**A call that cannot be served in time is refused when it arrives, not
when it gives up.** The judge admits a call while at most one other is
waiting, and refuses immediately beyond that. The threshold comes from
the one clock that governs waiting rather than from a round number, and
the distinction matters because no budget in this crate spans waiting
and service together: `EVAL_TIMEOUT` bounds the request alone and
`DISPATCH_WAIT` bounds the acquire alone. Once the two are equal, what
has to fit is a call's projected wait against the wait bound it will be
held to: behind `n` waiters a call projects to `n` times the service
time it can expect, and it is admissible only while that product stays
inside the deadline. That service time is not a constant. The ninety
fifth percentile of single attempt service has been measured at 296
seconds and at 321 seconds in two windows a day apart, and the first
admits one waiter inside ten minutes while the second does not. The
threshold is therefore computed from the measurement current when the
code lands, and what this record fixes is the inequality rather than
the number. This needs a counter the crate does not have. The existing
`queued` counts only waiters for metrics that carry weight, because it
was built for the yield rule, and it must not be read as a queue depth.

**Concurrent Tier 2 dispatches are bounded at two traces.** A trace
attempts at most three metrics and awaits them in turn, so two traces
put at most two calls at the queue together, which is exactly what the
admission rule accepts. A trace refused for concurrency is refused
whole, before any call is built, and the refusal is recorded against the
trace next to its inclusion probability. A trace declined for load is
missing for a reason the corpus has to be able to see, which is the same
argument that put `tier2_inclusion_p` in the store.

**This record carries its own falsification test, and one clause of it
is worthless.** That no served call should record a wait at the bound
cannot fail: with the queue capped at two, almost nothing can reach a
ten minute wait, so the clause passes whether or not the reasoning
behind it holds. It is written down only so that it is not mistaken
later for evidence. What can fail is the rest. If the trace cap is
right, the count of traces with every attempted metric intact rises well
clear of 3 in 69. If it is too tight, traces refused whole outnumber the
complete gradings it buys, and what wants adjusting is how often a trace
is sampled rather than how many may grade together. If the admission
threshold errs the other way, refusals at the door land on traces the
old queue would have served, which shows as goodput falling while
refusals rise.

**One reading of that test is forbidden in advance.** Wait drops falling
is not evidence, because this change deletes the wait drop as a
category. The serving literature has a name for the trap and a fix:
measure goodput, meaning only work that finished and was usable. Here
that is complete gradings per hour, and it is the only number that can
say whether this record helped.

## Consequences

**What gets easier:**

- Fragments stop crowding out gradings. Two traces graded to completion
  are worth more to the corpus than five traces that each yield a
  discarded half pair, because a half pair is not a weak verdict, it is
  no verdict.
- A refusal costs a moment instead of five minutes. The 113 minutes
  currently spent queueing for a refusal go back to the calls that can
  be served.
- The missingness becomes legible. A trace declined at the door is
  recorded as declined, with a reason, before any work is done. A trace
  that dies in pieces at the wait bound has to be reconstructed from
  call logs afterwards, and that reconstruction is not possible from the
  store today.

**What gets harder:**

- Fewer traces are graded at all. This is the trade, taken on purpose.
  The rate at which traces enter Tier 2 is now bounded by two things
  rather than one, and the second bound is not visible in the sample
  rate.
- The corpus gains another boundary. Completion rates and drop reasons
  measured before this change do not compare with rates measured after,
  and this is the second such boundary inside a week.
- Two counters exist where the code has one, and they mean different
  things. An earlier attempt at this rule was instrumented against the
  wrong one, and the flag it produced described calls the rule had
  never applied to.
- Nothing here makes a slow call faster. The ten minute deadline is now
  the only bound a call can hit, and no served call has ever reached it.
  Whether it is the right number is a question this record does not
  answer.

## Alternatives considered

- **Lower the deadline to meet the wait bound rather than raising the
  wait.** Symmetry either way, and cheaper to write. Rejected because it
  buys the symmetry by discarding verdicts that arrive. In the split
  clock window 6 of the 120 served calls ran past five minutes and the
  longest ran 551 seconds;
  those are real gradings, and cutting them removes the slow tail of the
  distribution from the corpus for no reason except that it is slow.
- **Leave both bounds and lower the sample rate.** Addresses how much
  work arrives but not how it clumps. The rate is a per trace coin
  flip and does nothing about two traces drawn close together, which is
  the only situation that produces a queue. It also loses the traces
  most worth grading, because the sampler already prefers agents that
  look unhealthy.
- **Queue at the trace and let calls through freely.** The mirror of
  ADR-0050's mistake rather than a correction of it. The backend still
  serves one call at a time, so removing the call level bound puts the
  refusals back at the deadline where they started.
- **Adaptive shedding on measured sojourn time instead of a fixed
  depth.** This is where the outside work points, and the fixed depth
  chosen here is the thing it warns about: a threshold that fires as a
  cliff. Deferred rather than rejected, because the target and interval
  such a scheme needs have to be fitted against a sojourn distribution
  measured under the new admission rule, and that distribution does not
  exist yet. The counter this record adds is what makes it measurable.
- **Grade a trace's metrics concurrently to shorten its span.** It would
  cut the median trace grading window and make the trace cap cheaper.
  Rejected because it puts three calls at a server that runs one, which
  is the thing ADR-0050 removed and this record does not reopen.
