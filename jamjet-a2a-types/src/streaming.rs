//! Streaming event types for A2A v1.0.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

use crate::task::{Artifact, Message, Task, TaskStatus};

// ────────────────────────────────────────────────────────────────────────────
// TaskStatusUpdateEvent
// ────────────────────────────────────────────────────────────────────────────

/// A streaming event indicating that a task's status has changed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskStatusUpdateEvent {
    pub task_id: String,
    pub context_id: String,
    pub status: TaskStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, Value>>,
}

// ────────────────────────────────────────────────────────────────────────────
// TaskArtifactUpdateEvent
// ────────────────────────────────────────────────────────────────────────────

/// A streaming event delivering an artifact chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskArtifactUpdateEvent {
    pub task_id: String,
    pub context_id: String,
    pub artifact: Artifact,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub append: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_chunk: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, Value>>,
}

// ────────────────────────────────────────────────────────────────────────────
// StreamResponse
// ────────────────────────────────────────────────────────────────────────────

/// A single frame in a streaming response — one of several event types.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StreamResponse {
    Task(Task),
    Message(Message),
    StatusUpdate(TaskStatusUpdateEvent),
    ArtifactUpdate(TaskArtifactUpdateEvent),
}
