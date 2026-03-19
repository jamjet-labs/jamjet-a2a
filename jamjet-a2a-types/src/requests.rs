//! Request and response types for A2A v1.0 RPC methods.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

use crate::push::TaskPushNotificationConfig;
use crate::task::{Message, Task, TaskState};

// ────────────────────────────────────────────────────────────────────────────
// SendMessageConfiguration
// ────────────────────────────────────────────────────────────────────────────

/// Per-request configuration for `SendMessage`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendMessageConfiguration {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accepted_output_modes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_push_notification_config: Option<TaskPushNotificationConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history_length: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub return_immediately: Option<bool>,
}

// ────────────────────────────────────────────────────────────────────────────
// SendMessageRequest
// ────────────────────────────────────────────────────────────────────────────

/// Request body for the `SendMessage` / `StreamMessage` RPC method.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendMessageRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    pub message: Message,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configuration: Option<SendMessageConfiguration>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, Value>>,
}

// ────────────────────────────────────────────────────────────────────────────
// SendMessageResponse
// ────────────────────────────────────────────────────────────────────────────

/// A2A v1.0 wrapped Task response: `{"task": { ... }}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskWrapper {
    pub task: Task,
}

/// A2A v1.0 wrapped Message response: `{"message": { ... }}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageWrapper {
    pub message: Message,
}

/// Response for `SendMessage` — a wrapped Task or Message per A2A v1.0.
///
/// A2A v1.0 returns `{"task": {...}}` or `{"message": {...}}`.
/// For backwards compatibility, also accepts a bare Task or Message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SendMessageResponse {
    /// `{"task": {...}}` — A2A v1.0 wire format.
    WrappedTask(TaskWrapper),
    /// `{"message": {...}}` — A2A v1.0 wire format.
    WrappedMessage(MessageWrapper),
    /// Bare Task (backwards compatibility with pre-v1.0 servers).
    Task(Task),
    /// Bare Message (backwards compatibility with pre-v1.0 servers).
    Message(Message),
}

impl SendMessageResponse {
    /// Extract the [`Task`] from the response, regardless of wrapping.
    pub fn into_task(self) -> Option<Task> {
        match self {
            Self::WrappedTask(w) => Some(w.task),
            Self::Task(t) => Some(t),
            _ => None,
        }
    }

    /// Extract the [`Message`] from the response, regardless of wrapping.
    pub fn into_message(self) -> Option<Message> {
        match self {
            Self::WrappedMessage(w) => Some(w.message),
            Self::Message(m) => Some(m),
            _ => None,
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// GetTaskRequest
// ────────────────────────────────────────────────────────────────────────────

/// Request to retrieve a task by ID.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetTaskRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history_length: Option<i32>,
}

// ────────────────────────────────────────────────────────────────────────────
// ListTasksRequest
// ────────────────────────────────────────────────────────────────────────────

/// Request to list tasks with optional filters and pagination.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListTasksRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<TaskState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_size: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history_length: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_timestamp_after: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_artifacts: Option<bool>,
}

// ────────────────────────────────────────────────────────────────────────────
// ListTasksResponse
// ────────────────────────────────────────────────────────────────────────────

/// Paginated response for `ListTasks`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListTasksResponse {
    pub tasks: Vec<Task>,
    pub next_page_token: String,
    pub page_size: i32,
    pub total_size: i32,
}

// ────────────────────────────────────────────────────────────────────────────
// CancelTaskRequest
// ────────────────────────────────────────────────────────────────────────────

/// Request to cancel a running task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelTaskRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, Value>>,
}

// ────────────────────────────────────────────────────────────────────────────
// SubscribeToTaskRequest
// ────────────────────────────────────────────────────────────────────────────

/// Request to subscribe to streaming updates for a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscribeToTaskRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    pub id: String,
}

// ────────────────────────────────────────────────────────────────────────────
// GetExtendedAgentCardRequest
// ────────────────────────────────────────────────────────────────────────────

/// Request to retrieve the extended (authenticated) Agent Card.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetExtendedAgentCardRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
}
