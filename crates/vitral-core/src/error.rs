use thiserror::Error;

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum VitralError {
    #[error("null pointer passed across FFI boundary: {0}")]
    NullPointer(String),
    #[error("invalid UTF-8 string: {0}")]
    InvalidUtf8(String),
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("engine is not initialized")]
    NotInitialized,
    #[error("operation blocked by pure offline mode: {0}")]
    PureOfflineViolation(String),
    #[error("I/O error: {0}")]
    Io(String),
    #[error("parse error: {0}")]
    Parse(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("integrity verification failed: {0}")]
    Integrity(String),
    #[error("model unavailable: {0}")]
    ModelUnavailable(String),
    #[error("internal error: {0}")]
    Internal(String),
}
