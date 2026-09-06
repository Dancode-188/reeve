# 0052: The Wait Bound Held, and the Deadline Is the Next Bound

**Status:** Accepted
**Date:** 2026-09-06
**Amends:** [0051](./0051-the-judge-refuses-work-at-admission.md)

## Context

ADR-0051 raised the wait bound to meet the deadline, added an admission
counter, and said what would falsify it. The soak was declared at a fixed
number of dispatch records before anything was read, that number has been
reached, and this records what it says.

**The refusal at admission has stopped happening.** In the era where the
judge records why a metric was dropped, the wait accounted for 8 of 29
dispatch records before the change and 0 of 90 after it. Every wait bound
drop in the store predates the change, the newest by about two minutes.
One sided Fisher on those counts is far below any threshold worth
quoting, and with no events in 90 the rule of three puts the upper bound
on the current rate near three percent against a prior rate near
twenty-eight.

**The denominator is where this nearly went wrong, and the mistake is
worth more than the result.** The reason a metric was dropped has only
been recorded since the deploy that added the column. Before it, several
hundred dropped metrics carry no reason at all: the drops happened, the
cause was never written. A first reading of this soak used a longer
window on the earlier side to be conservative, and most of that window
sat before the column existed, so most of its denominator could not have
produced the event being counted whatever those calls did. It looked like
the careful choice and it was the broken one. **A denominator has to be
restricted to the rows where the numerator was recordable.**

**Verdicts exist that the old rule would have thrown away.** Nine calls
served since the change waited longer than the old bound, the longest by
more than three minutes. Those are gradings in the corpus that the
previous rule would have refused at admission while the backend was
working on them, which is exactly the failure ADR-0051 described and
could not then count.

**The old bound was truncating the distribution, and the new one is not.**
The ninety-fifth percentile of waits before the change is one millisecond
past the bound itself. That is not a distribution that ends, it is one
that is cut. Since the change the same percentile sits well inside the
new bound and the longest wait recorded is nowhere near it, so waiting is
now ending on its own.

**One sentence in ADR-0051 has to go, and what replaces it is worse
news.** It says the deadline is the only bound a call can hit and that no
served call has ever reached it. ADR-0051 uses served throughout to mean
a call that obtained the slot and ran, and under its own usage the
sentence is false: attempts sit exactly on the deadline, each having held
the slot for the full ten minutes before giving up, and one of them
predates the record that made the claim. Under the narrower reading,
where served means the call came back with a verdict, the sentence is
true by construction, because reaching the deadline is what stops a call
from coming back. A sentence that is false one way and unfalsifiable the
other does not belong in a record written to be checked.

**The replacement matters more than the correction.** The longest attempt
that did come back with a verdict ran about fourteen seconds short of the
deadline, and the four longest sit within twenty seconds of it. That is
the same signature the wait bound showed before this change, one bound
over: a distribution piling up in the last few seconds before its own cap
and then stopping. The difference is that the wait bound was refusing
work the backend would have finished, and the deadline is refusing work
the backend has not finished. Whether that is the right refusal is the
open question, and it is now the only one of these bounds still binding.

**The confound is unchanged and it dominates everything above.** That
boundary deployed three changes together: the raised bound, the admission
counter, and the dispatch record that makes any of this countable. Nothing
here separates them. This is a verdict on the bundle, and the honest
summary is that the wait bound stopped firing, not that it was responsible.

## Decision

The constants stand. `DISPATCH_WAIT` and `EVAL_TIMEOUT` are both ten
minutes, the compile time assertion that the wait is never shorter than
the service it waits behind stays, and `MAX_WAITERS` remains one.

The sentence in ADR-0051 claiming no served call has reached the deadline
is struck. In its place: **calls do reach the deadline, at roughly one in
ninety since the change, and the deadline is now the only bound that
refuses work.**

The next falsification test is the deadline, and it is narrower than the
last one. If ten minutes is too short, the metrics dropped there are the
largest prompts and the count grows with load. If it is right, the count
stays near where it is and drops there stay dominated by transport
failures rather than by slow generation. This record does not choose a
number, because the sojourn distribution under the new admission rule is
still only days old.

## Consequences

- The corpus gains no boundary from this record. Nothing here changes
  behaviour, so rates measured before and after it compare.
- Every rate quoted about why a metric was dropped is now restricted to
  the era where that reason is recorded, which is a much shorter history
  than the pilot. Earlier figures spanning the whole run are not
  comparable to it and should not be quoted beside it.
- The population that can be graded at all grew, because nine calls that
  the old rule would have refused were served instead. Any comparison of
  score distributions across that boundary inherits the change in who got
  graded, not only in how many.
- Three of the four deadline drops share a prompt size at the top of the
  range, so the deadline and the prompt length question are the same
  question and cannot be answered separately.
- The record now depends on a column that did not always exist. Any
  future claim of this shape has to state the era it is scoped to, and
  the tooling that reads these counts asks for a boundary as an instant
  rather than a raw timestamp so the era cannot be typed wrong.

## Alternatives considered

- **Wait for a longer soak before recording anything.** The number was
  declared in advance and reached, and waiting past a declared stopping
  point to see whether a result improves is how a soak stops being a
  test. The short earlier window is stated as the weakness it is.
- **Quote the wider window because it is more conservative.** It is not
  conservative, it is wrong, for the reason given above. A rate whose
  denominator cannot produce its numerator understates nothing, it
  measures nothing.
- **Attribute the result to the raised bound and close the question.**
  Rejected. Three changes shipped together and the counter alone could
  produce the same reading by refusing work earlier and elsewhere. Naming
  the bundle costs nothing and keeps the record true.
- **Lower the deadline now that it is the only bound.** The same trade
  ADR-0051 rejected, and rejected again here for the same reason: the
  calls near it are real gradings of the largest prompts, and cutting
  them removes the slow tail from the corpus rather than speeding
  anything up.
