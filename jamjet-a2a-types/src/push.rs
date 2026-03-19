//! Push notification types for A2A v1.0.

use serde::{Deserialize, Serialize};

// ────────────────────────────────────────────────────────────────────────────
// AuthenticationInfo
// ────────────────────────────────────────────────────────────────────────────

/// Credentials for authenticating push notification delivery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthenticationInfo {
    pub scheme: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credentials: Option<String>,
}

// ────────────────────────────────────────────────────────────────────────────
// TaskPushNotificationConfig
// ────────────────────────────────────────────────────────────────────────────

/// Configuration for push notifications on a specific task.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskPushNotificationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub task_id: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authentication: Option<AuthenticationInfo>,
}

// ────────────────────────────────────────────────────────────────────────────
// CreateTaskPushNotificationConfigRequest
// ────────────────────────────────────────────────────────────────────────────

/// Request to create a push notification configuration for a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateTaskPushNotificationConfigRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub task_id: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authentication: Option<AuthenticationInfo>,
}

// ────────────────────────────────────────────────────────────────────────────
// GetTaskPushNotificationConfigRequest
// ────────────────────────────────────────────────────────────────────────────

/// Request to retrieve a specific push notification configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetTaskPushNotificationConfigRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    pub task_id: String,
    pub id: String,
}

// ────────────────────────────────────────────────────────────────────────────
// ListTaskPushNotificationConfigsRequest
// ────────────────────────────────────────────────────────────────────────────

/// Request to list push notification configurations for a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListTaskPushNotificationConfigsRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    pub task_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_size: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_token: Option<String>,
}

// ────────────────────────────────────────────────────────────────────────────
// ListTaskPushNotificationConfigsResponse
// ────────────────────────────────────────────────────────────────────────────

/// Response containing a page of push notification configurations.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListTaskPushNotificationConfigsResponse {
    pub configs: Vec<TaskPushNotificationConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_page_token: Option<String>,
}

// ────────────────────────────────────────────────────────────────────────────
// DeleteTaskPushNotificationConfigRequest
// ────────────────────────────────────────────────────────────────────────────

/// Request to delete a push notification configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteTaskPushNotificationConfigRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    pub task_id: String,
    pub id: String,
}
