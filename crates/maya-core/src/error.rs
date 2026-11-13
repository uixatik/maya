use thiserror::Error;

/// Unified result alias for Maya operations.
pub type Result<T> = std::result::Result<T, MayaError>;

/// Top-level error type shared across all Maya crates.
#[derive(Debug, Error)]
pub enum MayaError {
    /// Failed to parse a source URL or magnet URI.
    #[error("invalid URL: {0}")]
    InvalidUrl(#[from] url::ParseError),

    /// Source scheme is not supported by any configured protocol handler.
    #[error("unsupported scheme: {0}")]
    UnsupportedScheme(String),

    /// Operation rejected by the active [`crate::SecurityPolicy`].
    #[error("security policy violation: {0}")]
    SecurityViolation(String),

    /// Transport-level network error.
    #[error("network error: {0}")]
    Network(String),

    /// Protocol framing or unexpected remote response.
    #[error("protocol error: {0}")]
    Protocol(String),

    /// Remote HTTP server returned a non-success status code.
    #[error("http status {status}: {message}")]
    HttpStatus { status: u16, message: String },

    /// Disk persistence or part-file management error.
    #[error("storage error: {0}")]
    Storage(String),

    /// Underlying filesystem or OS I/O failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON state serialization or deserialization failure.
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// Computed file digest did not match the expected checksum.
    #[error("checksum mismatch: expected {expected}, actual {actual}")]
    ChecksumMismatch { expected: String, actual: String },

    /// Existing `.maya.state` metadata is incompatible with the remote resource.
    #[error("resume state mismatch: {0}")]
    ResumeMismatch(String),

    /// Invalid request or engine configuration parameter.
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),

    /// Download was cancelled by the caller or signal.
    #[error("cancelled")]
    Cancelled,

    /// Operation exceeded its configured timeout.
    #[error("timeout: {0}")]
    Timeout(String),

    /// Requested operation is not supported for this source or target.
    #[error("unsupported operation: {0}")]
    Unsupported(String),
}
