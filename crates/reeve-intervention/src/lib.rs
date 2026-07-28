pub mod dispatcher;
pub mod server;
pub mod types;

pub mod proto {
    // Committed rather than built, so no protoc is needed to compile this
    // crate. See build.rs to regenerate.
    include!("generated/reeve.rs");
}
