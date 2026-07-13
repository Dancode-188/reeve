# 0043: Outbound Secret Scanning Warns First, Blocks by Consent

**Status:** Accepted
**Date:** 2026-07-13

## Context

An agent that reads a `.env` file or a credential-bearing config does
not leak it once: the conversation history replays everything the
agent ever saw on every subsequent request, so one careless read
re-sends the secret to the API for the rest of the session. The proxy
is the last thing that sees those bytes before they leave the machine.

The failure mode that kills features like this is not a missed secret;
it is the false positive. Agent traffic is saturated with high-entropy
strings that are not secrets: trace ids, git SHAs, base64 content,
tool-use ids. A scanner that alerts on those trains the operator to
ignore the one alert that matters, and a scanner that BLOCKS on them
breaks a working session for nothing.

## Decision

**Detection leans on known shapes; entropy is gated, never free-
standing.** Provider-prefixed and structured patterns (Anthropic,
OpenAI, AWS, GitHub, Slack, Google, Stripe keys, PEM private key
headers, JWTs) carry the detection, because prefixes exist precisely
to make keys recognizable and their false-positive rate is near zero.
Shannon entropy fires only on assignment-shaped candidates, where
something named like a key, token, secret, or password is being given
a quoted high-entropy value. A bare high-entropy string is never a
finding.

**What survives a finding is the kind, a redacted hint, and a hash.**
The scan runs in memory on bytes already passing through. The alert
and the span carry the kind and a prefix-plus-last-four hint; dedup
uses a hash fingerprint. The secret itself is never stored, logged,
or put on a span, so tier 1 privacy holds even while scanning.

**A finding speaks once per agent.** The replayed history means one
leaked key appears on every subsequent request of the conversation.
Alerts and span marks fire only for fingerprints not yet seen for
that agent; the first request to carry the secret gets the durable
`reeve.secret.*` mark, and the ALERTS notice fires once.

**Warn is the default; blocking is opt-in and blocks every time.**
With `[secrets] block = true` in the config, a request carrying ANY
finding, new or seen, is refused with a named API error, the same
voice as the circuit breaker's refusal. Blocking on seen secrets is
deliberate: the history re-leaks on every request, so a one-shot
block would be theater. The honest consequence is that a contaminated
conversation stays refused until it is abandoned, which is what a
hard wall means. Warn-first is the default because that consequence
must be chosen, never stumbled into; the operator graduates to
blocking after the warnings have proven themselves quiet.

## Consequences

**What gets easier:**
- The nightmare scenario, an agent quietly shipping a private key to
  a third party on every request, becomes visible in ALERTS and on
  the span the moment it starts, with zero agent instrumentation.
- The redaction discipline means a leaked-secret report can be read,
  shared, and stored without itself being a leak.

**What gets harder:**
- The pattern list is a maintenance surface: new providers mean new
  prefixes. Additions are cheap (one line and a test), but coverage
  is forever partial and honestly so.
- A secret with no recognizable shape and no credential-shaped
  assignment around it passes silently: this scanner catches
  recognizable keys, not all entropy, and claims nothing more.
- In block mode, a contaminated conversation is dead until the client
  starts a fresh one; there is no per-secret allowlist yet. The
  operator's recourse is turning block off and restarting.

## Alternatives considered

- **Block by default.** The first false positive would break a
  working session and burn trust in every alert after it. Warn-first
  earns the right to block.
- **Free-standing entropy detection.** Catches more, drowns the
  operator in span ids and base64. The gated form keeps entropy's
  reach where a name asserts credential intent.
- **Redact the secret from the forwarded request instead of warning
  or blocking.** Tempting, but silently mutating the conversation
  corrupts the agent's context (the model sees a placeholder the
  client never sent) and hides the event instead of surfacing it.
  An intervention should be visible, like every other one.
- **Scan responses too.** The risk direction is outbound: what leaves
  the machine. Inbound content is the developer's own model output,
  already on their screen; scanning it doubles the cost for a threat
  that is not the one this guards against.
