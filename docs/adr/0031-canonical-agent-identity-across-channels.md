# 0031: Canonical Agent Identity Composed by One Function Across Both Channels

**Status:** Accepted
**Date:** 2026-07-06

## Context

Reeve talks to every agent over two channels. The observation channel
receives OTel spans on port 4317; the control channel holds a bidirectional
gRPC stream on port 4316. The two were built at different times and each
grew its own notion of who an agent is. The ingestion pipeline derived agent
identity from OTel resource attributes as `service.name` joined to
`service.instance.id` with a colon. The control server registered each
connection under whatever string arrived in the handshake's `agent_id`
field.

Nothing mapped one identity to the other. The first live end-to-end run of
the intervention loop exposed the consequence: the overlay's capability
lookup missed, dispatch failed with `not_connected`, and no command could
route to any SDK-connected agent. Every layer was individually correct and
the loop was broken between them. Worse, neither SDK set an OTel `Resource`
at all, so every SDK agent displayed as `unknown_service` regardless.

## Decision

The canonical agent identity is the OTel resource identity:
`service.name:service.instance.id`. One function composes it,
`agent_id_from_service` in `reeve-model`, and both channels are required to
derive identity through that function and nowhere else.

The `AgentHandshake` message gains `service_name` and `service_instance_id`
fields. The control server composes its registration key from them, which
means a command dispatched against an agent id observed from spans lands on
the correct control stream by construction rather than by convention.

SDKs own the identity at a single point: the connect call takes an agent
name and an instance id, sets both as resource attributes on the tracer
provider, and sends the same two values in the handshake. The instance id
defaults to a value derived from connect time so concurrent instances of
the same agent stay distinct; agents that need stable identity across
restarts pass it explicitly.

A handshake that carries no service fields falls back to registering under
the raw `agent_id`. Older SDKs keep connecting, but their observed and
control identities will not match and interventions will not route to them.
SDKs also still populate `agent_id` with the composed form, so a server
that predates the new fields registers the same identity a new server
composes.

## Consequences

**What gets easier:**
- The two channels cannot disagree about identity. There is exactly one
  composition site, and both the ingestion normalize stage and the control
  server call it.
- Every identity-keyed structure in the system now lines up for SDK agents:
  capability lookups, dispatch routing, and the paused-agents set shared
  between the dispatcher and the assembler.
- `unknown_service` disappears for SDK agents, since the SDKs now set the
  resource attributes as part of connecting.

**What gets harder:**
- The default instance id changes on every connect, so an agent that
  restarts gets a fresh identity and a fresh fingerprint history unless the
  caller pins the instance id explicitly. This is the honest default:
  distinguishing concurrent instances matters more than preserving history
  for agents that did not ask for it.
- The fallback path keeps old SDKs visibly connected while interventions
  silently fail to route to them. That state is observable in the fleet
  view but not obviously a version mismatch. Acceptable while the SDKs and
  server ship from one repository; revisit when third-party SDKs exist.

## Alternatives considered

**Require SDKs to send the composed form as `agent_id` (rejected):** Works
with no proto change, and the live run used exactly this as a manual
workaround. But it makes the composition rule an implicit contract that
every SDK author must know and honor, and the first one who does not
reintroduces this bug. Explicit fields with server-side composition turn
the convention into structure.

**Drop the instance id from ingestion's derivation (rejected):** Keying
agents by bare `service.name` would make the two channels match without any
proto change, but two concurrent instances of the same agent would collapse
into one identity: their spans would interleave in one trace view and an
intervention aimed at the runaway instance would hit both. The instance
distinction is deliberate and worth the extra field.

**A mapping table between the two identities (rejected):** Registering both
forms and translating at dispatch time preserves both schemes, but a
heuristic mapping between identities that were never designed to correspond
is exactly the kind of quiet complexity that breaks under concurrent
instances and reconnects. Unifying the identity removes the need to map.
