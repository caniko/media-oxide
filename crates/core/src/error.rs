use thiserror::Error;

/// Errors that can occur during pipeline operation.
#[derive(Debug, Error)]
pub enum PipelineError {
    #[error("source error: {0}")]
    Source(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("transcoder error: {0}")]
    Transcoder(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("sink error: {0}")]
    Sink(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("codec mismatch: input {input:?} is not supported by {transcoder}")]
    CodecMismatch {
        input: crate::types::MediaCodec,
        transcoder: String,
    },

    /// The transcoder decided the original must be kept unchanged.
    #[error("keep original: {0}")]
    Keep(String),

    #[error("state error: {0}")]
    State(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, PipelineError>;
