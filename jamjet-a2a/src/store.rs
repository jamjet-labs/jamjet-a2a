//! TaskStore trait and in-memory implementation for A2A v1.0 servers.

use jamjet_a2a_types::*;
use std::collections::HashMap;
use tokio::sync::{broadcast, Mutex};
use tracing::debug;

// ────────────────────────────────────────────────────────────────────────────
// TaskStore trait
// ────────────────────────────────────────────────────────────────────────────

/// Async storage backend for A2A tasks.
///
/// Implementations must be `Send + Sync` so they can be shared across Axum
/// handlers via `Arc<dyn TaskStore>`.
#[async_trait::async_trait]
pub trait TaskStore: Send + Sync {
    /// Insert a new task into the store.
    async fn insert(&self, task: Task) -> Result<(), A2aError>;

    /// Retrieve a task by ID, returning `None` if it does not exist.
    async fn get(&self, task_id: &str) -> Result<Option<Task>, A2aError>;

    /// Update a task's status and broadcast a [`TaskStatusUpdateEvent`].
    async fn update_status(&self, task_id: &str, status: TaskStatus) -> Result<(), A2aError>;

    /// Append an artifact to a task and broadcast a [`TaskArtifactUpdateEvent`].
    async fn add_artifact(&self, task_id: &str, artifact: Artifact) -> Result<(), A2aError>;

    /// List tasks with cursor-based pagination and optional filters.
    async fn list(&self, req: &ListTasksRequest) -> Result<ListTasksResponse, A2aError>;

    /// Cancel a task. Returns an error if the task is in a terminal state.
    async fn cancel(&self, task_id: &str) -> Result<(), A2aError>;

    /// Subscribe to streaming events for a task. Returns `None` if the task
    /// does not exist or has no broadcast channel.
    async fn subscribe(&self, task_id: &str) -> Option<broadcast::Receiver<StreamResponse>>;
}

// ────────────────────────────────────────────────────────────────────────────
// InMemoryTaskStore
// ────────────────────────────────────────────────────────────────────────────

/// Broadcast channel capacity for per-task event streams.
const CHANNEL_CAPACITY: usize = 64;

struct InMemoryInner {
    tasks: HashMap<String, Task>,
    /// Insertion-order list of task IDs (used for cursor-based pagination).
    order: Vec<String>,
    /// Per-task broadcast channels for streaming events.
    channels: HashMap<String, broadcast::Sender<StreamResponse>>,
}

/// A simple in-memory [`TaskStore`] backed by a `tokio::sync::Mutex`.
///
/// Suitable for development, testing, and single-node deployments. For
/// production use with multiple server instances, swap in a database-backed
/// implementation.
pub struct InMemoryTaskStore {
    inner: Mutex<InMemoryInner>,
}

impl InMemoryTaskStore {
    /// Create a new, empty in-memory store.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(InMemoryInner {
                tasks: HashMap::new(),
                order: Vec::new(),
                channels: HashMap::new(),
            }),
        }
    }
}

impl Default for InMemoryTaskStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl TaskStore for InMemoryTaskStore {
    async fn insert(&self, task: Task) -> Result<(), A2aError> {
        let mut inner = self.inner.lock().await;
        let task_id = task.id.clone();
        let (tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        inner.channels.insert(task_id.clone(), tx);
        inner.order.push(task_id.clone());
        inner.tasks.insert(task_id, task);
        Ok(())
    }

    async fn get(&self, task_id: &str) -> Result<Option<Task>, A2aError> {
        let inner = self.inner.lock().await;
        Ok(inner.tasks.get(task_id).cloned())
    }

    async fn update_status(&self, task_id: &str, status: TaskStatus) -> Result<(), A2aError> {
        let mut inner = self.inner.lock().await;
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| A2aProtocolError::TaskNotFound {
                task_id: task_id.to_string(),
            })?;
        task.status = status.clone();
        let context_id = task.context_id.clone().unwrap_or_default();
        // Release the mutable borrow on `tasks` before accessing `channels`.
        let _ = task;

        // Broadcast status update event.
        if let Some(tx) = inner.channels.get(task_id) {
            let event = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                task_id: task_id.to_string(),
                context_id,
                status,
                metadata: None,
            });
            // Ignore send errors (no active receivers).
            let _ = tx.send(event);
        }

        debug!(task_id, "status updated");
        Ok(())
    }

    async fn add_artifact(&self, task_id: &str, artifact: Artifact) -> Result<(), A2aError> {
        let mut inner = self.inner.lock().await;
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| A2aProtocolError::TaskNotFound {
                task_id: task_id.to_string(),
            })?;
        task.artifacts.push(artifact.clone());
        let context_id = task.context_id.clone().unwrap_or_default();
        // Release the mutable borrow on `tasks` before accessing `channels`.
        let _ = task;

        // Broadcast artifact update event.
        if let Some(tx) = inner.channels.get(task_id) {
            let event = StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
                task_id: task_id.to_string(),
                context_id,
                artifact,
                append: None,
                last_chunk: None,
                metadata: None,
            });
            let _ = tx.send(event);
        }

        debug!(task_id, "artifact added");
        Ok(())
    }

    async fn list(&self, req: &ListTasksRequest) -> Result<ListTasksResponse, A2aError> {
        let inner = self.inner.lock().await;

        let page_size = req.page_size.unwrap_or(50).max(1).min(100) as usize;
        let history_length = req.history_length;
        let include_artifacts = req.include_artifacts.unwrap_or(false);

        // Parse statusTimestampAfter filter.
        let ts_after = req
            .status_timestamp_after
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok());

        // Step 1: Collect ALL matching tasks (applying filters) in reverse insertion
        // order (descending — most recently inserted first, per A2A spec).
        let mut all_matching: Vec<Task> = Vec::new();
        for id in inner.order.iter().rev() {
            if let Some(task) = inner.tasks.get(id) {
                // Filter by context_id.
                if let Some(ref ctx) = req.context_id {
                    if task.context_id.as_deref() != Some(ctx.as_str()) {
                        continue;
                    }
                }
                // Filter by status.
                if let Some(ref status) = req.status {
                    if task.status.state != *status {
                        continue;
                    }
                }
                // Filter by statusTimestampAfter.
                if let Some(ts_cutoff) = &ts_after {
                    let passes = task
                        .status
                        .timestamp
                        .as_deref()
                        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                        .map(|t| t >= *ts_cutoff)
                        .unwrap_or(false);
                    if !passes {
                        continue;
                    }
                }
                all_matching.push(task.clone());
            }
        }

        let total_size = all_matching.len() as i32;

        // Step 2: Cursor-based pagination on the filtered set.
        let start_idx = if let Some(ref token) = req.page_token {
            if token.is_empty() {
                0
            } else {
                // Find the cursor in the filtered list and start after it.
                all_matching
                    .iter()
                    .position(|t| t.id == *token)
                    .map(|pos| pos + 1)
                    .unwrap_or(all_matching.len())
            }
        } else {
            0
        };

        let page: Vec<Task> = all_matching
            .into_iter()
            .skip(start_idx)
            .take(page_size)
            .map(|mut task| {
                // Apply historyLength limiting.
                if let Some(hl) = history_length {
                    if hl == 0 {
                        task.history = None;
                    } else if let Some(ref mut hist) = task.history {
                        let hl = hl as usize;
                        if hist.len() > hl {
                            let start = hist.len() - hl;
                            *hist = hist.split_off(start);
                        }
                    }
                }
                // Exclude artifacts unless explicitly requested.
                if !include_artifacts {
                    task.artifacts = vec![];
                }
                task
            })
            .collect();

        let actual_count = page.len() as i32;

        // Determine next_page_token: empty string means no more results.
        let next_page_token = if page.len() == page_size
            && start_idx + page_size < total_size as usize
        {
            page.last().map(|t| t.id.clone()).unwrap_or_default()
        } else {
            String::new()
        };

        // pageSize in response = actual number of tasks returned (capped by request).
        Ok(ListTasksResponse {
            tasks: page,
            next_page_token,
            page_size: actual_count,
            total_size,
        })
    }

    async fn cancel(&self, task_id: &str) -> Result<(), A2aError> {
        let mut inner = self.inner.lock().await;
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| A2aProtocolError::TaskNotFound {
                task_id: task_id.to_string(),
            })?;

        // Terminal states cannot be canceled.
        match task.status.state {
            TaskState::Completed | TaskState::Failed | TaskState::Canceled => {
                return Err(A2aProtocolError::TaskNotCancelable {
                    task_id: task_id.to_string(),
                }
                .into());
            }
            _ => {}
        }

        let canceled_status = TaskStatus {
            state: TaskState::Canceled,
            message: None,
            timestamp: Some(chrono::Utc::now().to_rfc3339()),
        };
        task.status = canceled_status.clone();
        let context_id = task.context_id.clone().unwrap_or_default();
        // Release the mutable borrow on `tasks` before accessing `channels`.
        let _ = task;

        // Broadcast cancellation event.
        if let Some(tx) = inner.channels.get(task_id) {
            let event = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                task_id: task_id.to_string(),
                context_id,
                status: canceled_status,
                metadata: None,
            });
            let _ = tx.send(event);
        }

        debug!(task_id, "task canceled");
        Ok(())
    }

    async fn subscribe(&self, task_id: &str) -> Option<broadcast::Receiver<StreamResponse>> {
        let inner = self.inner.lock().await;
        inner.channels.get(task_id).map(|tx| tx.subscribe())
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn test_task(id: &str) -> Task {
        Task {
            id: id.into(),
            context_id: None,
            status: TaskStatus {
                state: TaskState::Submitted,
                message: None,
                timestamp: None,
            },
            artifacts: vec![],
            history: None,
            metadata: None,
        }
    }

    #[tokio::test]
    async fn insert_and_get() {
        let store = InMemoryTaskStore::new();
        store.insert(test_task("t1")).await.unwrap();
        let task = store.get("t1").await.unwrap();
        assert!(task.is_some());
        assert_eq!(task.unwrap().id, "t1");
    }

    #[tokio::test]
    async fn get_missing_returns_none() {
        let store = InMemoryTaskStore::new();
        assert!(store.get("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn cancel_terminal_task_fails() {
        let store = InMemoryTaskStore::new();
        store.insert(test_task("t1")).await.unwrap();
        store
            .update_status(
                "t1",
                TaskStatus {
                    state: TaskState::Completed,
                    message: None,
                    timestamp: None,
                },
            )
            .await
            .unwrap();
        let result = store.cancel("t1").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn cancel_working_task_succeeds() {
        let store = InMemoryTaskStore::new();
        store.insert(test_task("t1")).await.unwrap();
        store
            .update_status(
                "t1",
                TaskStatus {
                    state: TaskState::Working,
                    message: None,
                    timestamp: None,
                },
            )
            .await
            .unwrap();
        store.cancel("t1").await.unwrap();
        let task = store.get("t1").await.unwrap().unwrap();
        assert_eq!(task.status.state, TaskState::Canceled);
    }

    #[tokio::test]
    async fn list_with_pagination() {
        let store = InMemoryTaskStore::new();
        for i in 0..5 {
            store.insert(test_task(&format!("t{i}"))).await.unwrap();
        }
        let resp = store
            .list(&ListTasksRequest {
                tenant: None,
                context_id: None,
                status: None,
                page_size: Some(2),
                page_token: None,
                history_length: None,
                status_timestamp_after: None,
                include_artifacts: None,
            })
            .await
            .unwrap();
        assert_eq!(resp.tasks.len(), 2);
        assert_eq!(resp.total_size, 5);

        // Next page using the cursor.
        let resp2 = store
            .list(&ListTasksRequest {
                page_token: Some(resp.next_page_token.clone()),
                page_size: Some(2),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(resp2.tasks.len(), 2);
    }

    #[tokio::test]
    async fn list_filters_by_context_id() {
        let store = InMemoryTaskStore::new();
        let mut task_a = test_task("t1");
        task_a.context_id = Some("ctx-a".into());
        let mut task_b = test_task("t2");
        task_b.context_id = Some("ctx-b".into());
        store.insert(task_a).await.unwrap();
        store.insert(task_b).await.unwrap();

        let resp = store
            .list(&ListTasksRequest {
                context_id: Some("ctx-a".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(resp.tasks.len(), 1);
        assert_eq!(resp.tasks[0].id, "t1");
    }

    #[tokio::test]
    async fn list_filters_by_status() {
        let store = InMemoryTaskStore::new();
        store.insert(test_task("t1")).await.unwrap();
        store.insert(test_task("t2")).await.unwrap();
        store
            .update_status(
                "t2",
                TaskStatus {
                    state: TaskState::Working,
                    message: None,
                    timestamp: None,
                },
            )
            .await
            .unwrap();

        let resp = store
            .list(&ListTasksRequest {
                status: Some(TaskState::Working),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(resp.tasks.len(), 1);
        assert_eq!(resp.tasks[0].id, "t2");
    }

    #[tokio::test]
    async fn update_status_broadcasts_event() {
        let store = InMemoryTaskStore::new();
        store.insert(test_task("t1")).await.unwrap();
        let mut rx = store.subscribe("t1").await.unwrap();

        store
            .update_status(
                "t1",
                TaskStatus {
                    state: TaskState::Working,
                    message: None,
                    timestamp: None,
                },
            )
            .await
            .unwrap();

        let event = rx.recv().await.unwrap();
        match event {
            StreamResponse::StatusUpdate(e) => {
                assert_eq!(e.task_id, "t1");
                assert_eq!(e.status.state, TaskState::Working);
            }
            _ => panic!("expected StatusUpdate event"),
        }
    }

    #[tokio::test]
    async fn add_artifact_broadcasts_event() {
        let store = InMemoryTaskStore::new();
        store.insert(test_task("t1")).await.unwrap();
        let mut rx = store.subscribe("t1").await.unwrap();

        let artifact = Artifact {
            artifact_id: "a1".into(),
            name: Some("test".into()),
            description: None,
            parts: vec![],
            metadata: None,
            extensions: vec![],
        };
        store.add_artifact("t1", artifact).await.unwrap();

        let event = rx.recv().await.unwrap();
        match event {
            StreamResponse::ArtifactUpdate(e) => {
                assert_eq!(e.task_id, "t1");
                assert_eq!(e.artifact.artifact_id, "a1");
            }
            _ => panic!("expected ArtifactUpdate event"),
        }
    }

    #[tokio::test]
    async fn subscribe_missing_task_returns_none() {
        let store = InMemoryTaskStore::new();
        assert!(store.subscribe("nope").await.is_none());
    }

    #[tokio::test]
    async fn cancel_missing_task_returns_error() {
        let store = InMemoryTaskStore::new();
        let result = store.cancel("nope").await;
        assert!(result.is_err());
    }
}
