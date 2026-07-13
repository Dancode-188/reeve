# The proxy path

Reeve's proxy path puts a cockpit on any tool that speaks the Anthropic
API, with zero integration work. One environment variable:

```bash
ANTHROPIC_BASE_URL=http://localhost:4318 claude
```

Start Reeve, run your tool with that variable, and the tool appears in
the fleet with a live trace tree, health scoring, cost tracking, and
working interventions. This guide covers what that buys you, exactly
which guarantees each feature carries on this path, and the honest list
of what a proxy seat cannot do.

## How it works

The proxy listens on `4318`, forwards every request to
`api.anthropic.com`, and reads the traffic as it passes. Streaming
responses are forwarded chunk by chunk with the parse happening on a
copy, so the client sees the same bytes at the same time either way.
The forwarding overhead is measured per request and stamped on the
span; you can read it in SPAN DETAIL rather than take my word for it.

Everything in the cockpit is reconstructed from the traffic itself.
Agentic clients resend the conversation on every call, so consecutive
requests thread into turns. A `tool_use` block in one response plus its
`tool_result` in the next request becomes a tool span with a real
duration, parented to the round trip that requested it. Token usage,
cache reads, thinking tokens, and cost estimates come from the `usage`
the API reports. When the API applies a context edit, the span that
carried it gets a `compacted` marker, because the next request will
legitimately start a new trace and that should not read as a mystery.

Your API key passes through in memory only. It is never logged, never
persisted, and never attached to any span; a test pins this.

If you route through a gateway instead of hitting the API directly,
`REEVE_PROXY_UPSTREAM` overrides where the proxy forwards.

## Interventions, and what each one promises

The proxy path does not pretend its interventions are the SDK's. Same
actions, honestly different guarantees:

**Kill is a hard wall.** Killing a proxy agent engages a circuit
breaker: the proxy refuses to forward its Messages requests, returning
a named permission error instead. The agent cannot spend another token
no matter what its loop does, which makes kill on this path stronger
than the SDK's cooperative version, not weaker. A killed agent shows
`killed` in the fleet and comes back one of two ways: a Resume from the
intervention overlay (the Revive option), or restarting Reeve.

**Redirect and inject context apply on the next request.** There is no
control channel to a proxied tool, so the command waits until the agent
next calls the API and rides in as an appended operator message, marked
as such. The agent is told plainly that a human operator changed the
priorities and that its prior work is not in question, because early
wording that read like a correction made agents apologize for decisions
that were mine. Latency is therefore unbounded: an agent that never
calls again never receives the redirect, and commands expire rather
than apply late.

**Pause does not exist here.** Holding a request open reads as an
outage to the client, and there is no way to hold one safely. Proxy
agents show reduced capabilities instead of offering a pause that lies.

## Budgets

A daily spend cap per agent, in `~/.config/reeve/config.toml`:

```toml
[budgets]
default_daily = 10.0

[budgets.per_agent]
"claude-cli:proxy" = 5.0
```

The COST section grows a budget bar for capped agents. At 80 percent of
the cap you get an alert; at the cap, settled or predicted, the budget
fires a kill through the same path a policy rule uses, and on the proxy
path that is the breaker, so the cap is a hard ceiling. Raise the cap
and revive the agent to resume. The day boundary is your local
midnight, and a zero cap means unbudgeted, never a wall of zero.

Mid-trace cost prediction carries a stated 25 to 40 percent error, so
treat the cap as a guardrail rather than an accountant: if you need a
number that must never be crossed, set the cap below it.

## Secret scanning

The proxy scans outbound request bodies for credential shapes before
they leave the machine: provider key prefixes, private key headers,
JWTs, and credential-named assignments with high-entropy values. An
agent that reads a `.env` file re-sends it to the API on every
subsequent request, because the conversation history replays everything
the agent ever saw; the first request to carry a leak gets an alert and
a `secret!` row in SPAN DETAIL, once per secret rather than once per
round trip.

The scan runs in memory. What survives is the kind, a redacted hint,
and a fingerprint for dedup; the secret itself is never stored, logged,
or written to a span.

Warning is the default. Blocking is opt-in:

```toml
[secrets]
block = true
```

In block mode any request carrying a detected secret is refused with a
named error, every time, because the history re-leaks on every request
and a one-shot block would be theater. The consequence is that a
contaminated conversation stays refused until the client starts a fresh
one. Turn block on after the warnings have proven themselves quiet on
your traffic.

## What the proxy cannot see or do

- **Pause.** Covered above; it is absent rather than fake.
- **Identity is the User-Agent.** Two tools sending the same User-Agent
  read as one agent and would receive each other's commands. Fine for
  the common case of one tool per machine; wrong if you proxy two
  copies of the same CLI.
- **The breaker and budgets are per Reeve process.** Kill an agent
  here and it stays killed until revive or restart, but a second Reeve
  on another port knows nothing about it. Daily spend also resets on
  restart.
- **Costs are estimates, not billing.** Prices come from a static table
  of known models; unknown models show no cost rather than a wrong one.
- **Only Anthropic-shaped traffic today.** The threading, streaming,
  and usage parsing all speak the Messages API. Other providers are a
  deliberate later step.

## The reasoning, if you want it

The decisions above are recorded where they were made: agent identity
(ADR-0036), conversation threading (ADR-0037), next-request command
application (ADR-0038), the circuit breaker (ADR-0039), budgets
(ADR-0042), and secret scanning (ADR-0043), all under
[`docs/adr/`](../adr/).
