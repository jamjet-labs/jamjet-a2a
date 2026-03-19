//! Error types for the A2A protocol stack.

use std::time::Duration;
use thiserror::Error;

// ────────────────────────────────────────────────────────────────────────────
// A2aProtocolError
// ────────────────────────────────────────────────────────────────────────────

/// Errors defined by the A2A v1.0 protocol specification.
#[derive(Debug, Clone, Error)]
#[non_exhaustive]
pub enum A2aProtocolError {
    #[error("task not found: {task_id}")]
    TaskNotFound { task_id: String },

    #[error("task not cancelable: {task_id}")]
    TaskNotCancelable { task_id: String },

    #[error("push notifications not supported")]
    PushNotificationNotSupported,

    #[error("unsupported operation: {method}")]
    UnsupportedOperation { method: String },

    #[error("content type not supported: {media_type}")]
    ContentTypeNotSupported { media_type: String },

    #[error("invalid agent response: {details}")]
    InvalidAgentResponse { details: String },

    #[error("extended agent card not configured")]
    ExtendedCardNotConfigured,

    #[error("extension required: {uri}")]
    ExtensionRequired { uri: String },

    #[error("version not supported: {version}")]
    VersionNotSupported { version: String },
}

impl A2aProtocolError {
    /// Return the JSON-RPC error code for this protocol error.
    pub fn json_rpc_code(&self) -> i32 {
        match self {
            Self::TaskNotFound { .. } => -32001,
            Self::TaskNotCancelable { .. } => -32002,
            Self::PushNotificationNotSupported => -32003,
            Self::UnsupportedOperation { .. } => -32004,
            Self::ContentTypeNotSupported { .. } => -32005,
            Self::InvalidAgentResponse { .. } => -32006,
            Self::ExtendedCardNotConfigured => -32007,
            Self::ExtensionRequired { .. } => -32008,
            Self::VersionNotSupported { .. } => -32009,
        }
    }

    /// Return the most appropriate HTTP status code for this protocol error.
    pub fn http_status(&self) -> u16 {
        match self {
            Self::TaskNotFound { .. } => 404,
            Self::TaskNotCancelable { .. } => 409,
            Self::InvalidAgentResponse { .. } => 502,
            Self::ContentTypeNotSupported { .. } => 415,
            _ => 400,
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// A2aTransportError
// ────────────────────────────────────────────────────────────────────────────

/// Transport-level errors (connection, timeout, SSE).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum A2aTransportError {
    #[error("connection to {url} failed: {source}")]
    Connection {
        url: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("request to {url} timed out after {duration:?}")]
    Timeout { url: String, duration: Duration },

    #[error("invalid response: {details}")]
    InvalidResponse { details: String },

    #[error("SSE error: {details}")]
    SseError { details: String },
}

// ────────────────────────────────────────────────────────────────────────────
// A2aError
// ────────────────────────────────────────────────────────────────────────────

/// Top-level error enum combining protocol, transport, and auth errors.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum A2aError {
    #[error(transparent)]
    Protocol(#[from] A2aProtocolError),

    #[error(transparent)]
    Transport(#[from] A2aTransportError),

    #[error("unauthorized: {reason}")]
    Auth { reason: String },
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_correct() {
        assert_eq!(
            A2aProtocolError::TaskNotFound {
                task_id: "x".into()
            }
            .json_rpc_code(),
            -32001
        );
        assert_eq!(
            A2aProtocolError::TaskNotCancelable {
                task_id: "x".into()
            }
            .json_rpc_code(),
            -32002
        );
        assert_eq!(
            A2aProtocolError::PushNotificationNotSupported.json_rpc_code(),
            -32003
        );
    }

    #[test]
    fn http_status_codes_correct() {
        assert_eq!(
            A2aProtocolError::TaskNotFound {
                task_id: "x".into()
            }
            .http_status(),
            404
        );
        assert_eq!(
            A2aProtocolError::TaskNotCancelable {
                task_id: "x".into()
            }
            .http_status(),
            409
        );
        assert_eq!(
            A2aProtocolError::ContentTypeNotSupported {
                media_type: "x".into()
            }
            .http_status(),
            415
        );
    }
}
