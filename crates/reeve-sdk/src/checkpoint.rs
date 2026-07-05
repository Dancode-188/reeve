/// Returned by [`crate::ReeveSdk::checkpoint`] when no command is pending or
/// when a command has been picked up and is ready for the caller to act on.
#[derive(Debug)]
pub enum CheckpointResult {
    /// No command pending. The agent should continue its current work.
    Continue,
    /// A Redirect command arrived. The agent should alter its next step to
    /// follow the given instruction.
    Redirect(String),
    /// An InjectContext command arrived. The context JSON should be merged
    /// into the agent's next prompt.
    Context(String),
}

/// Fatal signal from [`crate::ReeveSdk::checkpoint`]. The agent must not
/// continue execution after receiving this.
#[derive(thiserror::Error, Debug)]
pub enum AgentError {
    #[error("agent was terminated by reeve")]
    Killed,
}
