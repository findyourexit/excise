use thiserror::Error;

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("model memory limit exhausted: required {required} bytes, limit {limit} bytes")]
    MemoryExhausted { required: usize, limit: usize },
    #[error("identity accounting failed: {0}")]
    Identity(String),
    #[error("invalid model path: {0}")]
    InvalidPath(String),
    #[error("model invariant failed: {0}")]
    Invariant(String),
}
