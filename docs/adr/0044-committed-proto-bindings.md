# 0044: Committed Proto Bindings

**Status:** Accepted
**Date:** 2026-07-28

## Context

`reeve-intervention` and `reeve-sdk` both compiled `proto/reeve.proto`
in a build script, which meant every build of the workspace required
`protoc` on the machine.

That is a poor trade for the people it affects most. `cargo install
reeve-cockpit` compiles for several minutes and then stops on a missing
tool that has nothing to do with Rust, and the same is true for an
agent author who adds `reeve-sdk` as a dependency and has never
installed a protobuf compiler. The requirement was documented in the
README after a clean-machine install turned it up, but documenting a
barrier is not removing one.

The Python SDK never had this problem, because its `reeve_pb2.py` and
`reeve_pb2_grpc.py` are generated once and committed, so `pip install
reeve-sdk` needs nothing beyond pip.

## Decision

The generated bindings are committed, one file per crate:

```
crates/reeve-intervention/src/generated/reeve.rs
crates/reeve-sdk/src/generated/reeve.rs
```

Two files rather than one, because the crates do not generate the same
code. `reeve-intervention` builds the server and no client;
`reeve-sdk` builds the client and no server.

Each `lib.rs` includes its committed file directly instead of reaching
into `OUT_DIR`. The build scripts no longer run on an ordinary build.
They regenerate only when asked:

```
REEVE_REGENERATE_PROTO=1 cargo build -p reeve-intervention
```

A CI job regenerates both files and fails if the result differs from
what is checked in. Without that job this decision would be a bad one,
because the failure mode it introduces is silent.

## Consequences

**What gets easier:**
- `cargo install reeve-cockpit` works on a machine that has never seen
  a protobuf compiler, which is the state most machines are in.
- Adding `reeve-sdk` to an agent costs a line in `Cargo.toml` and
  nothing else.
- Builds get faster, since two build scripts stop doing real work.
- The wire format becomes readable in the repository. Anyone can see
  the generated surface without running a toolchain.

**What gets harder:**
- The bindings can go stale. Editing the proto without regenerating
  leaves the crates compiling against the old surface, and nothing in
  the build complains. The CI diff exists precisely because a human
  will forget, and a red build is the only reliable reminder.
- Regeneration is now a deliberate act with a command to remember. The
  command lives in a comment at the top of each build script, which is
  where someone editing the proto will be looking.
- Generated code shows up in diffs and review. This is mild here: the
  proto is additive-only by ADR-0023, so the files move rarely, and
  when they do the diff is the point.

## Alternatives considered

**Keep requiring protoc and document it (rejected):** what the project
did until now. The documentation is honest but the barrier remains, and
it lands after several minutes of compiling rather than at the start.

**Detect protoc and fall back to the committed file (rejected):** the
same source would then produce different builds on different machines
depending on what happened to be installed, and a stale committed file
would be invisible on any machine that had the compiler. Nondeterminism
to avoid an error message is a bad trade.

**Vendor a protoc binary in the repository (rejected):** solves the
missing tool by carrying a platform-specific executable per target, and
turns a build dependency into a distribution problem.

**Publish the bindings as a separate crate (rejected):** a shared
`reeve-proto` crate would remove the duplication between the two
generated files, but the two crates need different code from the same
proto, so the shared crate would have to generate both halves and let
each consumer ignore one. That is more machinery than the duplication
costs, and the duplication is checked by CI.
