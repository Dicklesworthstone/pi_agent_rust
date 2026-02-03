//! Error types for the Pi application.

use thiserror::Error;

/// Result type alias using our error type.
pub type Result<T> = std::result::Result<T, Error>;

/// Main error type for the Pi application.
#[derive(Error, Debug)]
pub enum Error {
    /// Configuration errors
    #[error("Configuration error: {0}")]
    Config(String),

    /// Session errors
    #[error("Session error: {0}")]
    Session(String),

    /// Session not found
    #[error("Session not found: {path}")]
    SessionNotFound { path: String },

    /// Provider/API errors
    #[error("Provider error: {provider}: {message}")]
    Provider { provider: String, message: String },

    /// Authentication errors
    #[error("Authentication error: {0}")]
    Auth(String),

    /// Tool execution errors
    #[error("Tool error: {tool}: {message}")]
    Tool { tool: String, message: String },

    /// Validation errors
    #[error("Validation error: {0}")]
    Validation(String),

    /// Extension errors
    #[error("Extension error: {0}")]
    Extension(String),

    /// IO errors
    #[error("IO error: {0}")]
    Io(#[from] Box<std::io::Error>),

    /// JSON errors
    #[error("JSON error: {0}")]
    Json(#[from] Box<serde_json::Error>),

    /// HTTP errors
    #[error("HTTP error: {0}")]
    Http(#[from] Box<reqwest::Error>),

    /// SQLite errors
    #[error("SQLite error: {0}")]
    Sqlite(#[from] Box<sqlmodel_core::Error>),

    /// User aborted operation
    #[error("Operation aborted")]
    Aborted,

    /// API errors (generic)
    #[error("API error: {0}")]
    Api(String),
}

impl Error {
    /// Create a configuration error.
    pub fn config(message: impl Into<String>) -> Self {
        Self::Config(message.into())
    }

    /// Create a session error.
    pub fn session(message: impl Into<String>) -> Self {
        Self::Session(message.into())
    }

    /// Create a provider error.
    pub fn provider(provider: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Provider {
            provider: provider.into(),
            message: message.into(),
        }
    }

    /// Create an authentication error.
    pub fn auth(message: impl Into<String>) -> Self {
        Self::Auth(message.into())
    }

    /// Create a tool error.
    pub fn tool(tool: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Tool {
            tool: tool.into(),
            message: message.into(),
        }
    }

    /// Create a validation error.
    pub fn validation(message: impl Into<String>) -> Self {
        Self::Validation(message.into())
    }

    /// Create an extension error.
    pub fn extension(message: impl Into<String>) -> Self {
        Self::Extension(message.into())
    }

    /// Create an API error.
    pub fn api(message: impl Into<String>) -> Self {
        Self::Api(message.into())
    }
}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Self::Io(Box::new(value))
    }
}

impl From<serde_json::Error> for Error {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(Box::new(value))
    }
}

impl From<reqwest::Error> for Error {
    fn from(value: reqwest::Error) -> Self {
        Self::Http(Box::new(value))
    }
}

impl From<sqlmodel_core::Error> for Error {
    fn from(value: sqlmodel_core::Error) -> Self {
        Self::Sqlite(Box::new(value))
    }
}
