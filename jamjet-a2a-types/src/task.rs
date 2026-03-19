//! Core A2A v1.0 types: Task, Message, Part, Artifact.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

// ────────────────────────────────────────────────────────────────────────────
// TaskState
// ────────────────────────────────────────────────────────────────────────────

/// Terminal and non-terminal states a task can occupy.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum TaskState {
    Submitted,
    Working,
    Completed,
    Failed,
    Canceled,
    InputRequired,
    Rejected,
    AuthRequired,
}

// ────────────────────────────────────────────────────────────────────────────
// TaskStatus
// ────────────────────────────────────────────────────────────────────────────

/// Snapshot of a task's current state, optionally carrying a status message.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TaskStatus {
    pub state: TaskState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
}

// ────────────────────────────────────────────────────────────────────────────
// Role
// ────────────────────────────────────────────────────────────────────────────

/// Who authored a message.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Role {
    User,
    Agent,
}

// ────────────────────────────────────────────────────────────────────────────
// Message
// ────────────────────────────────────────────────────────────────────────────

/// A single conversational message exchanged between user and agent.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub message_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub role: Role,
    pub parts: Vec<Part>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, Value>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reference_task_ids: Vec<String>,
}

// ────────────────────────────────────────────────────────────────────────────
// PartContent — custom serde for key-based discrimination
// ────────────────────────────────────────────────────────────────────────────

/// The payload of a [`Part`].
///
/// A2A v1.0 uses key-based discrimination on the wire:
/// - `{"text": "hello"}`
/// - `{"raw": "<base64>"}`
/// - `{"url": "https://..."}`
/// - `{"data": {...}}`
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum PartContent {
    Text(String),
    Raw(Vec<u8>),
    Url(String),
    Data(Value),
}

impl Serialize for PartContent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(Some(1))?;
        match self {
            PartContent::Text(s) => map.serialize_entry("text", s)?,
            PartContent::Raw(bytes) => {
                use base64::engine::{general_purpose::STANDARD, Engine};
                let encoded = STANDARD.encode(bytes);
                map.serialize_entry("raw", &encoded)?;
            }
            PartContent::Url(u) => map.serialize_entry("url", u)?,
            PartContent::Data(v) => map.serialize_entry("data", v)?,
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for PartContent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Deserialize into a generic map first, then inspect keys.
        let map: HashMap<String, Value> =
            HashMap::deserialize(deserializer)?;

        if let Some(v) = map.get("text") {
            let s = v
                .as_str()
                .ok_or_else(|| serde::de::Error::custom("\"text\" must be a string"))?;
            Ok(PartContent::Text(s.to_owned()))
        } else if let Some(v) = map.get("raw") {
            let encoded = v
                .as_str()
                .ok_or_else(|| serde::de::Error::custom("\"raw\" must be a base64 string"))?;
            use base64::engine::{general_purpose::STANDARD, Engine};
            let bytes = STANDARD
                .decode(encoded)
                .map_err(|e| serde::de::Error::custom(format!("base64 decode error: {e}")))?;
            Ok(PartContent::Raw(bytes))
        } else if let Some(v) = map.get("url") {
            let s = v
                .as_str()
                .ok_or_else(|| serde::de::Error::custom("\"url\" must be a string"))?;
            Ok(PartContent::Url(s.to_owned()))
        } else if let Some(v) = map.get("data") {
            Ok(PartContent::Data(v.clone()))
        } else {
            Err(serde::de::Error::custom(
                "PartContent must contain one of: text, raw, url, data",
            ))
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Part
// ────────────────────────────────────────────────────────────────────────────

/// A single content part within a [`Message`] or [`Artifact`].
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Part {
    #[serde(flatten)]
    pub content: PartContent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
}

// ────────────────────────────────────────────────────────────────────────────
// Artifact
// ────────────────────────────────────────────────────────────────────────────

/// An output artifact produced by an agent while executing a task.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Artifact {
    pub artifact_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parts: Vec<Part>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, Value>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<String>,
}

// ────────────────────────────────────────────────────────────────────────────
// Task
// ────────────────────────────────────────────────────────────────────────────

/// Top-level task object in the A2A protocol.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<Artifact>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history: Option<Vec<Message>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, Value>>,
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_state_serializes_to_screaming_snake_case() {
        let json = serde_json::to_string(&TaskState::InputRequired).unwrap();
        assert_eq!(json, "\"INPUT_REQUIRED\"");
    }

    #[test]
    fn role_serializes_to_lowercase() {
        assert_eq!(serde_json::to_string(&Role::User).unwrap(), "\"user\"");
        assert_eq!(serde_json::to_string(&Role::Agent).unwrap(), "\"agent\"");
    }

    #[test]
    fn task_state_round_trip() {
        for state in [
            TaskState::Submitted,
            TaskState::Working,
            TaskState::Completed,
            TaskState::Failed,
            TaskState::Canceled,
            TaskState::InputRequired,
            TaskState::Rejected,
            TaskState::AuthRequired,
        ] {
            let json = serde_json::to_value(&state).unwrap();
            let back: TaskState = serde_json::from_value(json).unwrap();
            assert_eq!(back, state);
        }
    }

    #[test]
    fn part_text_serializes_with_text_key() {
        let part = Part {
            content: PartContent::Text("hello".into()),
            metadata: None,
            filename: None,
            media_type: None,
        };
        let json = serde_json::to_value(&part).unwrap();
        assert_eq!(json["text"], "hello");
        assert!(json.get("raw").is_none());
        assert!(json.get("url").is_none());
        assert!(json.get("data").is_none());
    }

    #[test]
    fn part_data_serializes_with_data_key() {
        let part = Part {
            content: PartContent::Data(serde_json::json!({"key": "val"})),
            metadata: None,
            filename: None,
            media_type: None,
        };
        let json = serde_json::to_value(&part).unwrap();
        assert_eq!(json["data"]["key"], "val");
    }

    #[test]
    fn part_text_round_trip() {
        let part = Part {
            content: PartContent::Text("hello".into()),
            metadata: None,
            filename: None,
            media_type: Some("text/plain".into()),
        };
        let json = serde_json::to_value(&part).unwrap();
        let back: Part = serde_json::from_value(json).unwrap();
        assert_eq!(back.media_type, Some("text/plain".into()));
        match &back.content {
            PartContent::Text(s) => assert_eq!(s, "hello"),
            _ => panic!("expected Text variant"),
        }
    }

    #[test]
    fn task_full_round_trip() {
        let task = Task {
            id: "task-1".into(),
            context_id: Some("ctx-1".into()),
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: Some("2026-03-19T00:00:00Z".into()),
            },
            artifacts: vec![],
            history: None,
            metadata: None,
        };
        let json = serde_json::to_string(&task).unwrap();
        let back: Task = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "task-1");
        assert_eq!(back.status.state, TaskState::Working);
    }
}
