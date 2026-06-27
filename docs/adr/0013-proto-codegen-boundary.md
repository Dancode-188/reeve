# 0013: Proto Codegen Boundary Between reeve-model and reeve-intervention

**Status:** Accepted
**Date:** 2026-06-27

## Context

The intervention layer uses gRPC. That means protobuf. The question is
where the protobuf toolchain enters the workspace.

`reeve-model` is the foundation crate. Everything depends on it:
`reeve-storage`, `reeve-renderer`, `reeve-ingestion`, and eventually
`reeve-engine`. None of those crates speak gRPC. If `reeve-model` pulls
in `prost` and `tonic` as dependencies, every crate in the workspace
inherits that dependency graph for no reason.

The intervention domain types that map to proto messages are
`CommandType` and `AckStatus`. They need to exist in `reeve-model` so
that `reeve-storage` can persist them and `reeve-renderer` can display
them without depending on the gRPC transport crate.

## Decision

`CommandType` and `AckStatus` are hand-written plain Rust enums in
`reeve-model`, with `Serialize`/`Deserialize` derived as usual. They
mirror the relevant proto values but are defined independently of any
protobuf toolchain.

All actual proto codegen (`prost`/`tonic` build scripts, `.proto`
files, generated type bindings) stays inside `reeve-intervention`,
which owns the gRPC transport. When `reeve-intervention` needs to
convert between wire types and domain types, it does so at the gRPC
boundary via `From` impls.

The domain types also deliberately omit the zero-value sentinel that
protobuf requires for every enum (`Unspecified` or `_UNSPECIFIED = 0`).
That value exists because protobuf uses 0 as the default when a field
is missing on the wire. It is a transport concern, not a domain one. A
`CommandType` in the domain model is never actually unspecified.

## Consequences

**What gets easier:**
- `reeve-storage`, `reeve-renderer`, and `reeve-engine` depend only on
  `reeve-model`. None of them need to know that gRPC exists.
- The `reeve-model` compile time stays fast. No protobuf code generation
  runs when building the foundation crate.
- Adding a new command type means adding a variant to a Rust enum in
  one place. The `From` impl in `reeve-intervention` is the only place
  that needs to know both representations exist.

**What gets harder:**
- Every new proto message type that crosses into domain logic needs a
  manual mirror enum or struct in `reeve-model` and a `From` impl in
  `reeve-intervention`. It is not a lot of code, but it is not zero
  either.
- If the proto definition and the domain enum drift out of sync, the
  `From` impl is the only place the mismatch is caught. Good tests at
  the gRPC boundary are the mitigation.

## Alternatives considered

**Use prost-generated types directly in reeve-model (rejected):**
Simple to start, but it makes the entire workspace depend on the
protobuf toolchain. A developer working only on the renderer or the
storage layer has to wait for proto codegen to compile. The dependency
graph tells a misleading story about what each crate actually does.

**Define domain types in reeve-intervention, re-export to
reeve-model (rejected):** This reverses the dependency direction.
`reeve-model` would depend on `reeve-intervention`, which depends on
`prost` and `tonic`. That is worse than the previous option.
