/// Unified error type crossing the trait boundaries.
///
/// Adapter crates map their library-specific errors into these variants so the
/// agent and worker can reason about failure classes without knowing about
/// octocrab, sqlx, or reqwest.
#[derive(Debug, thiserror::Error)]
pub enum TriageError {
    /// The model responded, but the payload failed schema validation or
    /// deserialization. Always a hard failure — never guess a decision.
    #[error("model output failed validation: {0}")]
    InvalidModelOutput(String),

    /// The model endpoint itself failed (network, 5xx, rate limit exhausted).
    #[error("model request failed: {0}")]
    Model(String),

    #[error("pull request host error: {0}")]
    Host(String),

    #[error("billing error: {0}")]
    Billing(String),

    #[error("storage error: {0}")]
    Store(String),

    #[error("invalid repo config: {0}")]
    Config(String),
}
