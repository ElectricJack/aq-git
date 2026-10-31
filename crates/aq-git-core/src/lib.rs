//! Repository operations behind aq-git's versioned process protocol.
pub mod artifact;
pub mod candidate;
pub mod git;
pub mod lifecycle;
pub mod limits;
pub mod operations;
pub mod publication;
pub mod repository;
pub mod service;
pub mod state;
pub mod workspace;
pub use service::RepositoryService;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Error {
    pub code: String,
    pub message: String,
    pub details: serde_json::Value,
}
impl Error {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            details: serde_json::Value::Null,
        }
    }
    pub fn details(mut self, details: serde_json::Value) -> Self {
        self.details = details;
        self
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for Error {}
impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::new("io_error", error.to_string())
    }
}
impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::new("invalid_request", error.to_string())
    }
}
pub type Result<T> = std::result::Result<T, Error>;
