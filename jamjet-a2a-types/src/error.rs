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

    /// The task is in a terminal state (completed, failed, canceled or
    /// rejected) and cannot accept further messages, updates or
    /// subscriptions. A2A v1.0 reports this as `UnsupportedOperationError`.
    #[error("task {task_id} is in a terminal state and cannot be changed")]
    TaskTerminal { task_id: String },
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
            Self::TaskTerminal { .. } => -32004,
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
// A2aStoreError
// ────────────────────────────────────────────────────────────────────────────

/// Errors raised by a task store. These are server-side conditions, not A2A
/// protocol errors, and a server reports them as JSON-RPC internal errors.
#[derive(Debug, Clone, Error)]
#[non_exhaustive]
pub enum A2aStoreError {
    /// A task with this id already exists. Stores must never replace an
    /// existing task on insert.
    #[error("task already exists: {task_id}")]
    TaskAlreadyExists { task_id: String },

    /// The store holds its maximum number of live tasks and none of the
    /// caller's own terminal tasks could be evicted.
    #[error("task store is full ({capacity} tasks)")]
    CapacityExceeded { capacity: usize },

    /// The caller already holds its maximum number of live tasks and none of
    /// them is terminal.
    #[error("too many live tasks for this caller (limit {capacity})")]
    OwnerCapacityExceeded { capacity: usize },

    /// The task's history is at its message or byte limit.
    #[error("history of task {task_id} is full")]
    HistoryFull { task_id: String },

    /// The `contextId` is already used by another caller's tasks.
    #[error("contextId {context_id} is used by another caller")]
    ContextInUse { context_id: String },
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

    #[error(transparent)]
    Store(#[from] A2aStoreError),
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
        // Terminal-state rejections use UnsupportedOperationError's code.
        let terminal = A2aProtocolError::TaskTerminal {
            task_id: "x".into(),
        };
        assert_eq!(terminal.json_rpc_code(), -32004);
        assert_eq!(terminal.http_status(), 400);
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
