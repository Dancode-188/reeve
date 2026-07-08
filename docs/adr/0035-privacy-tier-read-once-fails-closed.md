# 0035: Privacy Tier Read Once at Startup, Failing Closed

**Status:** Accepted
**Date:** 2026-07-08

## Context

The privacy tier decides whether Reeve persists span event content (LLM
prompts and responses) or metadata only. Tier 1 stores no content; tier 2
enables content capture and writes a consent line to `consent.log`
recording when capture was enabled and by which configuration.

Two questions needed answers: what happens when the configuration is
missing or unreadable, and whether the tier participates in the runtime
policy reload path (SIGUSR1 reloads policy rules without a restart).

## Decision

**The tier fails closed.** A missing config file, an unparseable config
file, or an absent `privacy_tier` key all resolve to tier 1. Capturing
someone's LLM traffic because a TOML file had a syntax error is not an
acceptable failure mode; capturing nothing until explicitly told
otherwise always is.

**The tier is read once at startup and never reloaded.** SIGUSR1
deliberately does not re-read it, and the backend re-probe resends the
startup value rather than reloading the file. Content capture is a
consent decision with an audit line attached; a decision that silently
flips mid-session, without the ceremony of a restart, undermines what
the consent log claims to record. Changing the tier requires restarting
Reeve, which re-reads the config and writes a fresh consent line.

## Consequences

**What gets easier:**
- The worst configuration mistake under-captures rather than
  over-captures.
- `consent.log` entries correspond one-to-one with process lifetimes
  that captured content, which keeps the log a truthful record of when
  capture was active.
- Every consumer of the tier reads a value that cannot change under it,
  so there is no mid-session tier-transition handling anywhere.

**What gets harder:**
- Changing the tier requires restarting Reeve, which throws away the
  live session. Accepted as the ceremony a consent change deserves.
- Recordings made at tier 1 replay without content forever; replay shows
  a notice that content was not captured rather than blank space. An
  operator who enables tier 2 gains content only for traces recorded
  afterward.
- Anything else that travels with the tier must follow the read-once
  rule: the evaluation backend re-probe resends the startup tier value
  on its refreshed ready event instead of re-reading configuration.

## Alternatives considered

- **Fail open to tier 2 for a smoother demo.** Rejected without much
  discussion; consent that defaults to yes is not consent.
- **Reload the tier on SIGUSR1 with the policy rules.** Convenient, and
  wrong for the same reason: a consent boundary that changes without a
  restart makes the consent log ambiguous about what was captured when.
- **Per-agent or per-trace tiers.** Real use case (capture only the
  agent being debugged), real complexity (consent bookkeeping per
  stream). Deferred until someone asks for it.
