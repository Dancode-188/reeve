#[derive(Debug, thiserror::Error)]
pub enum RendererError {
    #[error("terminal error: {0}")]
    Terminal(#[from] std::io::Error),
    #[error("signal channel lagged")]
    SignalLag,
}
