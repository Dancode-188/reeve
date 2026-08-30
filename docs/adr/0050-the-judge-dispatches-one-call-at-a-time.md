# 0050: The Judge Dispatches One Call at a Time

**Status:** Accepted
**Date:** 2026-08-31
**Amends:** [0049](./0049-a-timed-out-judge-call-is-not-retried.md)

## Context

ADR-0049 raised the deadline for one judge call to fifteen minutes and
left a cap on concurrent dispatches out of that change on purpose. Its
reasoning was that the two answer different questions, and that sizing
a cap before a single dispatch had completed would have shipped a
guessed bound.

It did not leave the failure unnamed. Under what gets harder it
recorded a call starved by other calls rather than by its own size,
and it had already seen one: four dispatches holding connections at
once, and a 254 character `tool_selection` prompt worth about five
seconds of work dropped at the full ceiling without ever being served.
What that record predicted rather than observed was that bursts would
make this the common case, because a trace scoring at the tier 1 floor
samples at 1.0 and so dispatches every time.

That prediction is the one that held. Since the context fix in #347
landed, `tool_selection` prompts of 477, 506, 507 and 548 characters
have each burned the full ceiling.

Sizing the cap needed three questions answered, and all three now have
answers off this backend rather than off a guess.

**What does the backend serve at once? One.** It says so in every load
request, which carries `Parallel:1`. Nothing in the unit environment
sets that. The backend picks it itself at load time, choosing between
one slot and four according to the memory it has to spend on context,
and on this deployment it has picked one every time. So the bound is a
property of this deployment rather than of the software: a host with
more room to spend would serve four, and the cap below is then the
thing to revisit. The context budget comment in `llm_judge.rs` had
already been written as though a single runner slot were the case, so
the server confirms that reading instead of overturning it.

**Does the observed timing agree? In magnitude, yes.** Completion
latency regressed on the number of calls already in flight at start
puts each waiting call at about 94 seconds. That is the stable part of
the fit and the only part quoted here: it moved by two seconds when
the window doubled, while the intercept moved by fifty. It is a floor
rather than an estimate, for the reason in the limits below.

**Where does failure begin? At the second concurrent call, and for a
different reason below it.** Of the 98 calls since the context fix,
counted with the dead included rather than the survivors alone, 4 of
the 34 that began with at most one other call in flight ran out of
time, against 39 of the 64 that began with two or more. A permutation
test over 200,000 relabellings puts the split at p = 0.00001. The four
low depth failures are not a milder version of the same thing: every
one of them was a prompt between 4,306 and 8,392 characters, where the
starved calls that motivate this record were under 550. Below two in
flight the deadline is spent on inference. Above it the deadline is
spent waiting.

Those three answers size the bound. One limit on them, and one warning
about how to read them, are worth recording alongside.

The regression sees only calls that came back. Everything slower than
the ceiling is censored out of the fit, and the censored observations
are precisely the ones that would steepen it, so 94 seconds a queued
call understates what queueing costs. Extrapolated, the fit offers
headroom for several more concurrent calls. Its own inputs refute
that: at six or more in flight, 32 of 40 calls died, and all eight
survivors came back between 629 and 891 seconds against a 900 second
ceiling. The backend has been asked to hold twelve calls at once. This
record takes its bound from the observed split and treats the fitted
headroom as an artifact.

One warning for anyone who rechecks this later. Prompt size and queue
depth are both real predictors here and in this window each hides the
other, so a correlation taken against either one alone will read as
though it has gone away. Condition before concluding anything from it.

None of this is the backend failing. All 43 calls that ended without a
verdict ended at 900.0 seconds, to the tenth of a second, which is
this crate's own ceiling and not a number a backend would arrive at on
its own. Ollama has never given up on a request here. It has only ever
been hung up on.

What the missing cap has cost is most of the grading. Of the 29 traces
that reached a dispatch, 18 finished with no score at all, having
spent 41 attempts to get there. `tool_selection`, the cheap metric,
survived 7 of its 15 attempts, against 8 of 29 for
`hallucination_detection` and 4 of 29 for `faithfulness`. The gap
between those last two is inside the noise at this sample size and is
not an ordering; the gap between the cheap metric and the expensive
pair is not noise, and `faithfulness` carries the heaviest weight in
the table. Continuing to collect in this state adds rows without
adding evidence, because what goes missing is missing for reasons
bound up with what the grading was meant to measure.

## Decision

The judge holds one dispatch slot and takes it before issuing a
request, so at most one call is ever at the backend. This matches what
the backend is configured to serve. Concurrency here was never buying
anything: a second call in flight only ever occupied a socket until
the first one finished.

Waiting for the slot is bounded at five minutes, and that number comes
off the distribution this change creates rather than the one it
replaces. Once the slot exists, every call that reaches the backend
runs with nothing else in flight, and the calls that ran that way in
this window returned at a median of 143 seconds and a ninety fifth
percentile of 272. Five minutes is a little over one full service
time, so a call arriving while another is being served will usually
get the slot, and a call arriving second in line usually will not.
That is the intended shape rather than a regrettable one. A call that
cannot get the slot inside the window is dropped without being sent,
and is recorded as `NoVerdict` with a reason saying it was never
dispatched. It is not retried, because the queue that turned it away
is the queue a retry would rejoin.

Recording it is a deliberate widening of what the attempts table
covers. That table was introduced as a record of the causes that reach
a dispatch. Being turned away by a full slot stops one step short of
that line, but the judge still chose the metric, still meant to send
it, and still ended up with nothing to show, which is precisely the
silence the table was built to break.

The deadline for one call comes down from fifteen minutes to ten,
which is the part of ADR-0049 this record amends. Fifteen was sized
against the worst prompt the context budget permits, and that
reasoning was sound for a call being served. It is far wider than a
served call needs once nothing is queued ahead of it.

The price of ten minutes belongs against the population that will
actually pay it, which is calls that run alone. Twelve such calls
returned in this window and one of them took 855 seconds, so this cuts
about one served call in twelve rather than one in thirty. What it
buys for that is a bound on how long a wedged call can hold the only
slot there is.

The slot cap and the shorter deadline ship together and neither is
correct alone. A cap without a shorter deadline converts a throughput
problem into a stall, because the queue is now explicit and a single
wedged call blocks everything behind it for its full ceiling. This
also means queue depth was never purely a cause of timeouts: a timed
out call holds the backend for its whole deadline and inflates the
depth measured for everything behind it, so depth is partly a
consequence of the failure it predicts.

There is a further change after this one, and this record is not a
complete fix without it. With the queue gone, prompt size is the only
failure mode left standing, and the answer to that is a deadline that
scales with the prompt. It cannot be fitted until calls are being
served uncontended, which is what this change produces.

Requests now log `prompt_eval_count` and `eval_count`. The context
budgets in `llm_judge.rs` are written in characters, converted from a
token figure at a fixed ratio that nothing has ever verified against
the tokenizer actually reading the prompt, and the first count turns
that ratio from an assertion into something anyone can check. The
second is there because the ratio is not the only unverified term.
The size scaled deadline above has to be fitted against how long a
call takes, and generation length is the half of that the prompt
cannot report.

Everything else in ADR-0049 stands: a timed out call is still not
retried, other failures still retry, liveness keeps its own three
second deadline against `/api/tags`, and `keep_alive` still holds the
model between the calls of a dispatch.

The wait bound is the one guessed quantity in this record, and the
attempts table now says enough to check it. If it is right, give up
reasons move from the deadline to the wait: most `NoVerdict` rows
should say the call was never dispatched, and the ones that still name
the deadline should be large prompts. Small prompts reaching the
backend and still running out of time would mean ten minutes was cut
too far. Almost nothing dropped at the wait, with dispatches simply
taking much longer end to end, would mean five minutes is too generous
and that the sample rate rather than the bound is the thing to change.

## Consequences

**What gets easier:**
- A small prompt is no longer starved by a large one. The metric that
  costs seconds is not made to wait behind the metric that costs
  minutes and then charged the full ceiling for the privilege.
- Give-up reasons separate. A drop now says either that the backend
  could not answer in time or that this crate never sent the call, and
  those are different problems with different fixes.
- The corpus stops selecting on load. Missingness driven by how busy
  the backend happened to be is the harder of the two biases to reason
  about after the fact, because it correlates with burst structure
  rather than with anything about the trace.

**What gets harder:**
- Throughput is now explicitly bounded. Six calls to a dispatch at the
  observed service median is a long serial run, and traces arriving
  faster than that will lose metrics to the wait bound. Bursts are not
  the exception here: a trace scoring at the tier 1 floor samples at
  1.0, which is what fills the queue in the first place. The loss is
  visible in the attempts table rather than silent, which is the
  improvement, but it is still loss.
- A dispatch takes longer end to end. ADR-0020's two-tier update
  already assumes a late Tier 2 result and ADR-0049 widened that gap;
  this widens it again.
- The corpus gains another boundary. Rates measured before this change
  and after it are not comparable, and the numbers in this record are
  themselves conditional on a grader that was dropping work non
  randomly.
- Nothing here shrinks a prompt. The drops that remain should be the
  expensive metrics failing on their own account, which is a narrower
  problem than the one this record closes but not a smaller one.

## Alternatives considered

- **Raise the backend's parallelism instead of capping the client.**
  The backend runs one slot, and more slots on one machine divide the
  same compute into smaller shares while multiplying the memory the
  model context costs. It would convert one slow call into several
  slower ones and leave the client with no idea how many were
  outstanding. The cap belongs where the dispatches are created.
- **Queue without a bound and let every call wait its turn.** This
  makes the failure invisible instead of absent. A backlog would grow
  through a burst, calls would return against traces long finished,
  and nothing in the attempts table would distinguish a metric that
  waited an hour from one that was answered promptly.
- **Scale the deadline to the prompt rather than fixing it.**
  Deferred rather than rejected, and it is the sequel this record
  expects, for the reason given under what gets harder. It has to be
  fitted on calls that are actually being served, and until this
  record ships there are almost none of those, so a fit made now would
  inherit the censoring described above.
- **Shrink the prompts and leave concurrency alone.** Worth doing on
  its own merits and not a substitute: the starved calls that made
  this record necessary were 477 to 548 characters, which no budget
  cut reaches.
- **Cap dispatches per trace rather than calls at the backend.**
  Simpler to write and aimed at the wrong unit. A trace dispatches six
  calls, so a per-trace cap of one still puts six calls at a backend
  that serves one, and the queue it is supposed to prevent forms
  inside a single dispatch.
