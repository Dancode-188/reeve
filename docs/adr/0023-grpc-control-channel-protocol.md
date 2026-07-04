# 0023: gRPC Control Channel Protocol

**Status:** Accepted
**Date:** 2026-07-04

## Context

Reeve needs a persistent channel from each connected agent back to the
terminal. The ingestion pipeline (port 4317) is one-way: agents push
spans and Reeve never writes back. The control layer is the opposite
problem. Reeve needs to push commands to agents (Pause, Resume, Kill,
Redirect, InjectContext), and agents need to push acknowledgements and
heartbeats back. Neither direction is optional.

The protocol design also needs a stable wire contract that SDK authors
can implement against. Port assignment, message ordering guarantees,
and connection lifecycle all become SDK-visible from the moment the
first implementation ships.

## Decision

Run the control channel as a separate gRPC service on **port 4316**.

4317 is already reserved for ingestion (the OTel OTLP gRPC standard).
4316 is adjacent, unregistered with IANA for conflicting uses, and
easy to remember as "one below ingestion." Separating the two services
avoids routing logic inside a shared tonic server and keeps ingestion
untouched if the control layer changes.

The service exposes a single RPC: `ControlStream`, a bidirectional
streaming call. Agents hold the stream open for the duration of their
run. Reeve pushes `ControlMessage` frames downstream; agents push
`AgentMessage` frames upstream. Unary calls and server-streaming were
considered and rejected: both require polling on at least one side,
which is incompatible with low-latency command delivery.

**The first message on every stream must be `AgentHandshake`.** The
server returns `INVALID_ARGUMENT` for any other first message and
drops the connection. This keeps the connection state machine trivial:
once an entry exists in the connected-agents map, identity is
established. There is no agent ID field on subsequent messages.

`AgentHandshake` carries `t1_ms`, the agent's send timestamp in
milliseconds since the Unix epoch. The server records `t2_ms`
(receive) and `t3_ms` (send) immediately and returns them in
`HandshakeAck`. The agent follows up with `t4_ms` in a separate
`NtpFollowup` message. These four timestamps feed the NTP offset
formula in issue #7. Capturing T2 and T3 at handshake time means no
additional round-trip is needed when NTP offset computation lands.

`ControlServer::run()` returns `Arc<ControlServer>`. The dispatcher
(#57) holds this handle and calls `send_to_agent` to route commands to
specific agents. The server struct is `Clone` because tonic requires it
for the service trait; the `Arc<Mutex<HashMap>>` inside means all
clones share the same connection table.

## Consequences

**What gets easier:**
- SDK implementations have a single, unambiguous entry point: connect
  to 4316, send a handshake, hold the stream open.
- The dispatcher never touches gRPC directly. It calls `send_to_agent`
  with a typed `ControlMessage` and gets a boolean back.
- Ingestion and control evolve independently. A breaking protocol
  change does not touch port 4317 or the ingestion crate.

**What gets harder:**
- 4316 is now a reserved port. Running multiple Reeve instances on the
  same host requires explicit port overrides, which are not yet
  configurable.
- The handshake-first invariant means SDK authors cannot probe
  connectivity with a heartbeat before the handshake completes.
  Intentional, but worth calling out in SDK documentation.

## Alternatives considered

**Single port with service multiplexing (rejected):** tonic supports
multiple services on one server. Sharing 4317 between ingestion and
control would simplify firewall rules but couples two unrelated
concerns. The ingestion path is on the OTel critical path; a
control-layer bug on the same server can affect span delivery.

**Server-streaming RPC (rejected):** Reeve pushing commands downstream
is covered by a server-streaming call. But agents still need to send
acknowledgements and heartbeats back, which would require a separate
unary call per event. That creates message ordering ambiguities and
doubles the surface SDK authors must implement.

**WebSocket over HTTP (rejected):** Viable for browser-based SDK use
cases. All current target environments (Python asyncio, Rust tokio,
Node.js) have mature gRPC clients. WebSocket would add a custom
framing layer on top of what gRPC already provides for free.
