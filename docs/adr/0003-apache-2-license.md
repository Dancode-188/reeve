# 0003: Apache 2.0 License

**Status:** Accepted
**Date:** 2026-06-23

## Context

Reeve needs a license. The choice affects who can use it, how it can be
incorporated into other projects, and whether enterprise teams will engage
with it at all.

The realistic user base for Reeve includes:

1. Individual developers building AI agents who want better observability.
2. Teams at startups and enterprises building agent-based products.
3. Potentially: vendors who want to embed Reeve into their own tooling.

The primary goal is adoption. A license that blocks enterprise usage reduces
the pool of potential users and contributors, which reduces the feedback
surface and the community.

## Decision

Apache 2.0.

## Consequences

**What Apache 2.0 gives us that MIT does not:**
- An explicit patent grant. Contributors grant users a license to any patents
  that read on their contributions. For enterprise legal teams, this matters.
  A project without a patent grant creates legal ambiguity that risk-averse
  teams treat as a blocker.
- MIT is fine for libraries. For a developer tool that enterprises might
  deploy internally or build on top of, the patent grant makes Apache 2.0
  the safer choice.

**What AGPL would have given us that Apache 2.0 does not:**
- Copyleft: anyone who modifies and deploys Reeve must release their changes.
- This was explicitly rejected. Enterprise legal teams routinely block AGPL
  software because the "if you run it as a service" trigger is poorly
  understood and potentially broad. Blocking enterprise usage at the license
  level is the opposite of what Reeve needs.

**What gets harder:**
- Nothing meaningful. Apache 2.0 is a well-understood permissive license.
  The only practical constraint on users is attribution and the patent grant
  notice.

## Alternatives considered

**MIT (rejected):** No patent grant. Fine for libraries where this rarely
matters, but Reeve is infrastructure that enterprises will run. The missing
patent grant is a real gap for some users and there is no benefit to MIT
over Apache 2.0 that outweighs it.

**AGPL (rejected):** Blocks enterprise adoption. Enterprise legal teams treat
AGPL as a liability. The open-source community benefits that AGPL is meant
to create are outweighed by the lost adoption surface.

**BSL (Business Source License) (rejected):** Adds a time-delay to commercial
use that creates uncertainty and requires tracking when the conversion date
is. Unnecessary complexity for a tool that benefits from maximum adoption.
