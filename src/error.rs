//! Error types for Chatstronomy
//!
//! This module defines all custom error types used throughout the application,
//! providing better error handling and debugging information.

use thiserror::Error;

/// Main error type for Chatstronomy operations
#[derive(Error)]
pub enum ChatstronomyError {
    /// Observatory data-source errors
    #[error("Rig source error: {0}")]
    Source(#[from] crate::source::RigSourceError),

    /// Configuration errors
    #[error("Configuration error: {0}")]
    Config(#[from] crate::config::ConfigError),

    /// Chat service errors
    #[error("Chat service error: {0}")]
    Chat(#[from] ChatError),

    /// Service wrapper errors
    #[error("Service error: {0}")]
    Service(#[from] ServiceError),

    /// IO errors
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON parsing errors
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// Network/HTTP errors
    #[error("HTTP error: {0}")]
    Http(#[from] crate::security::SafeHttpError),

    /// URL parsing errors
    #[error("URL error: {0}")]
    Url(#[from] url::ParseError),

    /// Base64 decoding errors
    #[error("Base64 decode error: {0}")]
    Base64(#[from] base64::DecodeError),

    /// Generic error with context
    #[error("Error: {}", crate::security::redact_sensitive(message))]
    Generic { message: String },
}

impl std::fmt::Debug for ChatstronomyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ChatstronomyError")
            .field(&crate::security::redact_sensitive(&self.to_string()))
            .finish()
    }
}

impl From<reqwest::Error> for ChatstronomyError {
    fn from(error: reqwest::Error) -> Self {
        Self::Http(error.into())
    }
}

/// Chat service specific errors
#[derive(Error)]
pub enum ChatError {
    /// Discord webhook errors
    #[error("Discord error: {}", crate::security::redact_sensitive(message))]
    Discord { message: String },

    /// Matrix client errors
    #[error("Matrix error: {0}")]
    Matrix(crate::security::SanitizedError),

    /// Chat service initialization error
    #[error(
        "Failed to initialize chat service: {}: {}",
        crate::security::redact_sensitive(service_name),
        crate::security::redact_sensitive(reason)
    )]
    Initialization {
        service_name: String,
        reason: String,
    },

    /// Chat message sending error
    #[error(
        "Failed to send message to {}: {}",
        crate::security::redact_sensitive(service_name),
        crate::security::redact_sensitive(reason)
    )]
    MessageSend {
        service_name: String,
        reason: String,
    },
}

impl std::fmt::Debug for ChatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ChatError").field(&self.to_string()).finish()
    }
}

impl From<matrix_sdk::Error> for ChatError {
    fn from(error: matrix_sdk::Error) -> Self {
        Self::Matrix(crate::security::SanitizedError::new(error))
    }
}

/// Runtime service-wrapper errors
#[derive(Error)]
pub enum ServiceError {
    /// Service initialization failed
    #[error(
        "Service initialization failed: {}",
        crate::security::redact_sensitive(reason)
    )]
    Initialization { reason: String },

    /// Service runtime error
    #[error("Service runtime error: {}", crate::security::redact_sensitive(reason))]
    Runtime { reason: String },

    /// Service shutdown error
    #[error(
        "Service shutdown error: {}",
        crate::security::redact_sensitive(reason)
    )]
    Shutdown { reason: String },

    /// Tokio runtime errors
    #[error("Tokio runtime error: {0}")]
    TokioRuntime(#[from] tokio::io::Error),
}

impl std::fmt::Debug for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ServiceError")
            .field(&self.to_string())
            .finish()
    }
}

/// Result type alias for Service operations
pub type ServiceResult<T> = std::result::Result<T, ServiceError>;

impl From<String> for ChatstronomyError {
    fn from(message: String) -> Self {
        Self::Generic { message }
    }
}

impl From<&str> for ChatstronomyError {
    fn from(message: &str) -> Self {
        Self::Generic {
            message: message.to_string(),
        }
    }
}

impl From<String> for ChatError {
    fn from(message: String) -> Self {
        Self::Discord { message }
    }
}

impl From<&str> for ChatError {
    fn from(message: &str) -> Self {
        Self::Discord {
            message: message.to_string(),
        }
    }
}

impl From<String> for ServiceError {
    fn from(reason: String) -> Self {
        Self::Runtime { reason }
    }
}

impl From<&str> for ServiceError {
    fn from(reason: &str) -> Self {
        Self::Runtime {
            reason: reason.to_string(),
        }
    }
}
