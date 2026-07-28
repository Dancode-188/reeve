// The bindings this produces are committed at src/generated/reeve.rs, so an
// ordinary build needs no protoc on the machine. An agent author adding the
// SDK should not have to install a protobuf compiler to get a dependency to
// compile.
//
// Regenerate deliberately after changing the proto:
//
//     REEVE_REGENERATE_PROTO=1 cargo build -p reeve-sdk
//
// CI regenerates and diffs, so a stale committed file fails the build instead
// of shipping quietly.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-env-changed=REEVE_REGENERATE_PROTO");
    if std::env::var_os("REEVE_REGENERATE_PROTO").is_none() {
        return Ok(());
    }

    println!("cargo:rerun-if-changed=proto/reeve.proto");
    tonic_prost_build::configure()
        .build_server(false)
        .build_client(true)
        .out_dir("src/generated")
        .compile_protos(&["proto/reeve.proto"], &["proto"])?;
    Ok(())
}
