//! TaskStore trait and in-memory implementation for A2A v1.0 servers.

use jamjet_a2a_types::*;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::{broadcast, Mutex};
use tokio::time::Instant;
use tracing::debug;

// ────────────────────────────────────────────────────────────────────────────
// TaskStore trait
// ────────────────────────────────────────────────────────────────────────────

/// A task together with the principal it is bound to.
#[derive(Debug, Clone)]
pub struct TaskRecord {
    /// The task.
    pub task: Task,
    /// The [`Principal::id`](crate::auth::Principal::id) of the caller that
    /// created the task, or `None` if it was created without authentication.
    pub owner: Option<String>,
}

/// Async storage backend for A2A tasks.
///
/// Implementations must be `Send + Sync` so they can be shared across Axum
/// handlers via `Arc<dyn TaskStore>`.
///
/// The server relies on these rules, so every implementation must keep them:
///
/// - `insert` never replaces an existing task. If the id is taken it returns
///   [`A2aStoreError::TaskAlreadyExists`] and leaves the stored task alone.
/// - The owner recorded at insert never changes.
/// - Terminal states ([`TaskState::is_terminal`]) are final. Once a task is
///   terminal, `update_status`, `add_artifact` and `append_message` return
///   [`A2aProtocolError::TaskTerminal`] and change nothing. The check and the
///   write must be atomic.
/// - `list` returns only the tasks whose owner equals the `owner` argument.
#[async_trait::async_trait]
pub trait TaskStore: Send + Sync {
    /// Insert a new task bound to `owner`.
    async fn insert(&self, task: Task, owner: Option<String>) -> Result<(), A2aError>;

    /// Retrieve a task and its owner, returning `None` if it does not exist.
    async fn get_record(&self, task_id: &str) -> Result<Option<TaskRecord>, A2aError>;

    /// Retrieve a task by ID, returning `None` if it does not exist.
    ///
    /// This ignores ownership. It is meant for task handlers and other
    /// server-side code, not for answering client requests.
    async fn get(&self, task_id: &str) -> Result<Option<Task>, A2aError> {
        Ok(self.get_record(task_id).await?.map(|r| r.task))
    }

    /// Update a task's status and broadcast a [`TaskStatusUpdateEvent`].
    ///
    /// Fails with [`A2aProtocolError::TaskTerminal`] if the task is already
    /// in a terminal state.
    async fn update_status(&self, task_id: &str, status: TaskStatus) -> Result<(), A2aError>;

    /// Append an artifact to a task and broadcast a [`TaskArtifactUpdateEvent`].
    ///
    /// Fails with [`A2aProtocolError::TaskTerminal`] if the task is already
    /// in a terminal state.
    async fn add_artifact(&self, task_id: &str, artifact: Artifact) -> Result<(), A2aError>;

    /// List the tasks bound to `owner`, with cursor-based pagination and
    /// optional filters.
    async fn list(
        &self,
        req: &ListTasksRequest,
        owner: Option<&str>,
    ) -> Result<ListTasksResponse, A2aError>;

    /// Append a message to an existing task's history.
    ///
    /// Fails with [`A2aProtocolError::TaskTerminal`] if the task is already
    /// in a terminal state.
    async fn append_message(&self, task_id: &str, message: Message) -> Result<(), A2aError>;

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

/// Size and age limits for [`InMemoryTaskStore`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreLimits {
    /// Maximum number of tasks held at once. When the store is full, the
    /// least recently updated terminal task is evicted to make room. If every
    /// task is still active, the insert fails with
    /// [`A2aStoreError::CapacityExceeded`] rather than dropping live work.
    pub max_tasks: usize,
    /// Tasks not updated for this long are evicted, whatever their state.
    /// `None` keeps tasks until capacity eviction removes them.
    pub ttl: Option<Duration>,
}

impl StoreLimits {
    /// Default maximum number of tasks: 10,000.
    pub const DEFAULT_MAX_TASKS: usize = 10_000;
    /// Default idle time before a task is evicted: one hour.
    pub const DEFAULT_TTL: Duration = Duration::from_secs(60 * 60);
}

impl Default for StoreLimits {
    fn default() -> Self {
        Self {
            max_tasks: Self::DEFAULT_MAX_TASKS,
            ttl: Some(Self::DEFAULT_TTL),
        }
    }
}

struct Entry {
    task: Task,
    owner: Option<String>,
    /// Broadcast channel for this task's stream events. Dropping it (on
    /// eviction) ends every open stream for the task.
    tx: broadcast::Sender<StreamResponse>,
    last_update: Instant,
}

struct InMemoryInner {
    tasks: HashMap<String, Entry>,
    /// Insertion-order list of task IDs (used for cursor-based pagination).
    order: Vec<String>,
}

impl InMemoryInner {
    fn is_expired(entry: &Entry, ttl: Option<Duration>, now: Instant) -> bool {
        match ttl {
            Some(ttl) => now.saturating_duration_since(entry.last_update) >= ttl,
            None => false,
        }
    }

    /// Remove every task whose TTL has run out.
    fn evict_expired(&mut self, ttl: Option<Duration>, now: Instant) {
        if ttl.is_none() {
            return;
        }
        let before = self.tasks.len();
        self.tasks
            .retain(|_, entry| !Self::is_expired(entry, ttl, now));
        if self.tasks.len() != before {
            let tasks = &self.tasks;
            self.order.retain(|id| tasks.contains_key(id));
            debug!(evicted = before - self.tasks.len(), "evicted expired tasks");
        }
    }

    /// Look up a live task, evicting it first if its TTL has run out.
    fn live_mut(
        &mut self,
        task_id: &str,
        ttl: Option<Duration>,
        now: Instant,
    ) -> Option<&mut Entry> {
        if Self::is_expired(self.tasks.get(task_id)?, ttl, now) {
            self.tasks.remove(task_id);
            self.order.retain(|id| id != task_id);
            return None;
        }
        self.tasks.get_mut(task_id)
    }

    /// Like [`Self::live_mut`], but refuses tasks in a terminal state.
    fn live_open_mut(
        &mut self,
        task_id: &str,
        ttl: Option<Duration>,
        now: Instant,
    ) -> Result<&mut Entry, A2aError> {
        let entry =
            self.live_mut(task_id, ttl, now)
                .ok_or_else(|| A2aProtocolError::TaskNotFound {
                    task_id: task_id.to_string(),
                })?;
        if entry.task.status.state.is_terminal() {
            return Err(A2aProtocolError::TaskTerminal {
                task_id: task_id.to_string(),
            }
            .into());
        }
        Ok(entry)
    }

    /// Make room for one more task, evicting the least recently updated
    /// terminal task if the store is full.
    fn make_room(&mut self, max_tasks: usize) -> Result<(), A2aError> {
        while self.tasks.len() >= max_tasks {
            let victim = self
                .tasks
                .iter()
                .filter(|(_, e)| e.task.status.state.is_terminal())
                .min_by_key(|(_, e)| e.last_update)
                .map(|(id, _)| id.clone());
            match victim {
                Some(id) => {
                    self.tasks.remove(&id);
                    self.order.retain(|o| *o != id);
                    debug!(task_id = %id, "evicted terminal task to make room");
                }
                None => {
                    return Err(A2aStoreError::CapacityExceeded {
                        capacity: max_tasks,
                    }
                    .into())
                }
            }
        }
        Ok(())
    }
}

/// An in-memory [`TaskStore`] backed by a `tokio::sync::Mutex`.
///
/// Suitable for development, testing, and single-node deployments. For
/// production use with multiple server instances, swap in a database-backed
/// implementation.
///
/// The store is bounded by [`StoreLimits`]: by default it holds at most
/// 10,000 tasks and evicts tasks that have not been updated for an hour.
/// Evicting a task ends any open stream for it, and later requests for it
/// get `TaskNotFoundError`.
pub struct InMemoryTaskStore {
    inner: Mutex<InMemoryInner>,
    limits: StoreLimits,
}

impl InMemoryTaskStore {
    /// Create a new, empty in-memory store with the default [`StoreLimits`].
    pub fn new() -> Self {
        Self::with_limits(StoreLimits::default())
    }

    /// Create a new, empty in-memory store with the given limits.
    pub fn with_limits(limits: StoreLimits) -> Self {
        Self {
            inner: Mutex::new(InMemoryInner {
                tasks: HashMap::new(),
                order: Vec::new(),
            }),
            limits,
        }
    }

    /// The limits this store enforces.
    pub fn limits(&self) -> &StoreLimits {
        &self.limits
    }

    /// Number of tasks currently held, including expired tasks that have not
    /// been swept yet.
    pub async fn len(&self) -> usize {
        self.inner.lock().await.tasks.len()
    }

    /// True if the store holds no tasks.
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}

impl Default for InMemoryTaskStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl TaskStore for InMemoryTaskStore {
    async fn insert(&self, task: Task, owner: Option<String>) -> Result<(), A2aError> {
        let now = Instant::now();
        let mut inner = self.inner.lock().await;
        inner.evict_expired(self.limits.ttl, now);
        let task_id = task.id.clone();
        if inner.tasks.contains_key(&task_id) {
            return Err(A2aStoreError::TaskAlreadyExists { task_id }.into());
        }
        inner.make_room(self.limits.max_tasks)?;
        let (tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        inner.order.push(task_id.clone());
        inner.tasks.insert(
            task_id,
            Entry {
                task,
                owner,
                tx,
                last_update: now,
            },
        );
        Ok(())
    }

    async fn get_record(&self, task_id: &str) -> Result<Option<TaskRecord>, A2aError> {
        let now = Instant::now();
        let mut inner = self.inner.lock().await;
        Ok(inner
            .live_mut(task_id, self.limits.ttl, now)
            .map(|e| TaskRecord {
                task: e.task.clone(),
                owner: e.owner.clone(),
            }))
    }

    async fn update_status(&self, task_id: &str, status: TaskStatus) -> Result<(), A2aError> {
        let now = Instant::now();
        let mut inner = self.inner.lock().await;
        let entry = inner.live_open_mut(task_id, self.limits.ttl, now)?;
        entry.task.status = status.clone();
        entry.last_update = now;

        let event = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: task_id.to_string(),
            context_id: entry.task.context_id.clone().unwrap_or_default(),
            status,
            metadata: None,
        });
        // Ignore send errors (no active receivers).
        let _ = entry.tx.send(event);

        debug!(task_id, "status updated");
        Ok(())
    }

    async fn add_artifact(&self, task_id: &str, artifact: Artifact) -> Result<(), A2aError> {
        let now = Instant::now();
        let mut inner = self.inner.lock().await;
        let entry = inner.live_open_mut(task_id, self.limits.ttl, now)?;
        entry.task.artifacts.push(artifact.clone());
        entry.last_update = now;

        let event = StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
            task_id: task_id.to_string(),
            context_id: entry.task.context_id.clone().unwrap_or_default(),
            artifact,
            append: None,
            last_chunk: None,
            metadata: None,
        });
        let _ = entry.tx.send(event);

        debug!(task_id, "artifact added");
        Ok(())
    }

    async fn append_message(&self, task_id: &str, message: Message) -> Result<(), A2aError> {
        let now = Instant::now();
        let mut inner = self.inner.lock().await;
        let entry = inner.live_open_mut(task_id, self.limits.ttl, now)?;
        match entry.task.history {
            Some(ref mut hist) => hist.push(message),
            None => entry.task.history = Some(vec![message]),
        }
        entry.last_update = now;
        debug!(task_id, "message appended to history");
        Ok(())
    }

    async fn list(
        &self,
        req: &ListTasksRequest,
        owner: Option<&str>,
    ) -> Result<ListTasksResponse, A2aError> {
        let now = Instant::now();
        let mut inner = self.inner.lock().await;
        inner.evict_expired(self.limits.ttl, now);

        let page_size = req.page_size.unwrap_or(50).clamp(1, 100) as usize;
        let history_length = req.history_length;
        let include_artifacts = req.include_artifacts.unwrap_or(false);

        // Parse statusTimestampAfter filter.
        let ts_after = req
            .status_timestamp_after
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok());

        // Step 1: Collect ALL matching tasks (applying filters) in reverse insertion
        // order (descending — most recently inserted first, per A2A spec).
        // Only tasks bound to `owner` are visible.
        let mut all_matching: Vec<Task> = Vec::new();
        for id in inner.order.iter().rev() {
            if let Some(entry) = inner.tasks.get(id) {
                if entry.owner.as_deref() != owner {
                    continue;
                }
                let task = &entry.task;
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
        let next_page_token =
            if page.len() == page_size && start_idx + page_size < total_size as usize {
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
        let now = Instant::now();
        let mut inner = self.inner.lock().await;
        let entry = inner
            .live_mut(task_id, self.limits.ttl, now)
            .ok_or_else(|| A2aProtocolError::TaskNotFound {
                task_id: task_id.to_string(),
            })?;

        // Terminal states cannot be canceled.
        if entry.task.status.state.is_terminal() {
            return Err(A2aProtocolError::TaskNotCancelable {
                task_id: task_id.to_string(),
            }
            .into());
        }

        let canceled_status = TaskStatus {
            state: TaskState::Canceled,
            message: None,
            timestamp: Some(chrono::Utc::now().to_rfc3339()),
        };
        entry.task.status = canceled_status.clone();
        entry.last_update = now;

        // Broadcast cancellation event.
        let event = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: task_id.to_string(),
            context_id: entry.task.context_id.clone().unwrap_or_default(),
            status: canceled_status,
            metadata: None,
        });
        let _ = entry.tx.send(event);

        debug!(task_id, "task canceled");
        Ok(())
    }

    async fn subscribe(&self, task_id: &str) -> Option<broadcast::Receiver<StreamResponse>> {
        let now = Instant::now();
        let mut inner = self.inner.lock().await;
        inner
            .live_mut(task_id, self.limits.ttl, now)
            .map(|e| e.tx.subscribe())
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
        store.insert(test_task("t1"), None).await.unwrap();
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
        store.insert(test_task("t1"), None).await.unwrap();
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
        store.insert(test_task("t1"), None).await.unwrap();
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
            store
                .insert(test_task(&format!("t{i}")), None)
                .await
                .unwrap();
        }
        let resp = store
            .list(
                &ListTasksRequest {
                    tenant: None,
                    context_id: None,
                    status: None,
                    page_size: Some(2),
                    page_token: None,
                    history_length: None,
                    status_timestamp_after: None,
                    include_artifacts: None,
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(resp.tasks.len(), 2);
        assert_eq!(resp.total_size, 5);

        // Next page using the cursor.
        let resp2 = store
            .list(
                &ListTasksRequest {
                    page_token: Some(resp.next_page_token.clone()),
                    page_size: Some(2),
                    ..Default::default()
                },
                None,
            )
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
        store.insert(task_a, None).await.unwrap();
        store.insert(task_b, None).await.unwrap();

        let resp = store
            .list(
                &ListTasksRequest {
                    context_id: Some("ctx-a".into()),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(resp.tasks.len(), 1);
        assert_eq!(resp.tasks[0].id, "t1");
    }

    #[tokio::test]
    async fn list_filters_by_status() {
        let store = InMemoryTaskStore::new();
        store.insert(test_task("t1"), None).await.unwrap();
        store.insert(test_task("t2"), None).await.unwrap();
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
            .list(
                &ListTasksRequest {
                    status: Some(TaskState::Working),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(resp.tasks.len(), 1);
        assert_eq!(resp.tasks[0].id, "t2");
    }

    #[tokio::test]
    async fn update_status_broadcasts_event() {
        let store = InMemoryTaskStore::new();
        store.insert(test_task("t1"), None).await.unwrap();
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
        store.insert(test_task("t1"), None).await.unwrap();
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

    fn status(state: TaskState) -> TaskStatus {
        TaskStatus {
            state,
            message: None,
            timestamp: None,
        }
    }

    fn is_terminal_err(err: &A2aError) -> bool {
        matches!(
            err,
            A2aError::Protocol(A2aProtocolError::TaskTerminal { .. })
        )
    }

    // ── insert never overwrites ───────────────────────────────────────────

    #[tokio::test]
    async fn insert_refuses_duplicate_id_and_keeps_original() {
        let store = InMemoryTaskStore::new();
        let mut original = test_task("t1");
        original.context_id = Some("victim-ctx".into());
        store.insert(original, Some("alice".into())).await.unwrap();

        let mut imposter = test_task("t1");
        imposter.context_id = Some("attacker-ctx".into());
        let err = store
            .insert(imposter, Some("mallory".into()))
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                A2aError::Store(A2aStoreError::TaskAlreadyExists { .. })
            ),
            "got {err:?}"
        );

        let rec = store.get_record("t1").await.unwrap().unwrap();
        assert_eq!(rec.owner.as_deref(), Some("alice"));
        assert_eq!(rec.task.context_id.as_deref(), Some("victim-ctx"));
    }

    // ── terminal states are final ─────────────────────────────────────────

    #[tokio::test]
    async fn update_status_cannot_leave_terminal_state() {
        let store = InMemoryTaskStore::new();
        store.insert(test_task("t1"), None).await.unwrap();
        store
            .update_status("t1", status(TaskState::Completed))
            .await
            .unwrap();

        let err = store
            .update_status("t1", status(TaskState::Working))
            .await
            .unwrap_err();
        assert!(is_terminal_err(&err), "got {err:?}");
        let task = store.get("t1").await.unwrap().unwrap();
        assert_eq!(task.status.state, TaskState::Completed);
    }

    #[tokio::test]
    async fn terminal_task_refuses_messages_and_artifacts() {
        let store = InMemoryTaskStore::new();
        store.insert(test_task("t1"), None).await.unwrap();
        store
            .update_status("t1", status(TaskState::Failed))
            .await
            .unwrap();

        let msg = Message {
            message_id: "m2".into(),
            context_id: None,
            task_id: Some("t1".into()),
            role: Role::User,
            parts: vec![],
            metadata: None,
            extensions: vec![],
            reference_task_ids: vec![],
        };
        let err = store.append_message("t1", msg).await.unwrap_err();
        assert!(is_terminal_err(&err), "got {err:?}");

        let artifact = Artifact {
            artifact_id: "a1".into(),
            name: None,
            description: None,
            parts: vec![],
            metadata: None,
            extensions: vec![],
        };
        let err = store.add_artifact("t1", artifact).await.unwrap_err();
        assert!(is_terminal_err(&err), "got {err:?}");

        let task = store.get("t1").await.unwrap().unwrap();
        assert!(task.history.is_none());
        assert!(task.artifacts.is_empty());
    }

    #[tokio::test]
    async fn rejected_task_is_not_cancelable() {
        let store = InMemoryTaskStore::new();
        store.insert(test_task("t1"), None).await.unwrap();
        store
            .update_status("t1", status(TaskState::Rejected))
            .await
            .unwrap();
        let err = store.cancel("t1").await.unwrap_err();
        assert!(
            matches!(
                err,
                A2aError::Protocol(A2aProtocolError::TaskNotCancelable { .. })
            ),
            "got {err:?}"
        );
        let task = store.get("t1").await.unwrap().unwrap();
        assert_eq!(task.status.state, TaskState::Rejected);
    }

    // ── ownership ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn list_returns_only_the_owners_tasks() {
        let store = InMemoryTaskStore::new();
        store
            .insert(test_task("a1"), Some("alice".into()))
            .await
            .unwrap();
        store
            .insert(test_task("b1"), Some("bob".into()))
            .await
            .unwrap();
        store
            .insert(test_task("a2"), Some("alice".into()))
            .await
            .unwrap();
        store.insert(test_task("n1"), None).await.unwrap();

        let ids =
            |resp: &ListTasksResponse| resp.tasks.iter().map(|t| t.id.clone()).collect::<Vec<_>>();

        let alice = store
            .list(&ListTasksRequest::default(), Some("alice"))
            .await
            .unwrap();
        assert_eq!(ids(&alice), vec!["a2", "a1"]);
        assert_eq!(alice.total_size, 2);

        let bob = store
            .list(&ListTasksRequest::default(), Some("bob"))
            .await
            .unwrap();
        assert_eq!(ids(&bob), vec!["b1"]);

        let anonymous = store
            .list(&ListTasksRequest::default(), None)
            .await
            .unwrap();
        assert_eq!(ids(&anonymous), vec!["n1"]);
    }

    // ── bounded store ─────────────────────────────────────────────────────

    #[tokio::test(start_paused = true)]
    async fn ttl_evicts_idle_tasks_and_closes_their_streams() {
        let store = InMemoryTaskStore::with_limits(StoreLimits {
            max_tasks: 100,
            ttl: Some(Duration::from_secs(60)),
        });
        store.insert(test_task("old"), None).await.unwrap();
        let mut rx = store.subscribe("old").await.unwrap();

        tokio::time::advance(Duration::from_secs(61)).await;
        store.insert(test_task("new"), None).await.unwrap();

        assert!(store.get("old").await.unwrap().is_none());
        assert!(store.get("new").await.unwrap().is_some());
        assert_eq!(store.len().await, 1);
        assert!(matches!(
            rx.recv().await,
            Err(broadcast::error::RecvError::Closed)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn ttl_is_measured_from_the_last_update() {
        let store = InMemoryTaskStore::with_limits(StoreLimits {
            max_tasks: 100,
            ttl: Some(Duration::from_secs(60)),
        });
        store.insert(test_task("t1"), None).await.unwrap();
        tokio::time::advance(Duration::from_secs(40)).await;
        store
            .update_status("t1", status(TaskState::Working))
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(40)).await;
        assert!(store.get("t1").await.unwrap().is_some());
        tokio::time::advance(Duration::from_secs(21)).await;
        assert!(store.get("t1").await.unwrap().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn full_store_evicts_least_recently_updated_terminal_task() {
        let store = InMemoryTaskStore::with_limits(StoreLimits {
            max_tasks: 3,
            ttl: None,
        });
        store.insert(test_task("done-old"), None).await.unwrap();
        tokio::time::advance(Duration::from_secs(1)).await;
        store.insert(test_task("done-new"), None).await.unwrap();
        tokio::time::advance(Duration::from_secs(1)).await;
        store.insert(test_task("active"), None).await.unwrap();
        tokio::time::advance(Duration::from_secs(1)).await;
        store
            .update_status("done-old", status(TaskState::Completed))
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(1)).await;
        store
            .update_status("done-new", status(TaskState::Completed))
            .await
            .unwrap();

        store.insert(test_task("fresh"), None).await.unwrap();

        assert!(store.get("done-old").await.unwrap().is_none());
        assert!(store.get("done-new").await.unwrap().is_some());
        assert!(store.get("active").await.unwrap().is_some());
        assert!(store.get("fresh").await.unwrap().is_some());
        assert_eq!(store.len().await, 3);
    }

    #[tokio::test]
    async fn full_store_of_active_tasks_refuses_new_tasks() {
        let store = InMemoryTaskStore::with_limits(StoreLimits {
            max_tasks: 2,
            ttl: None,
        });
        store.insert(test_task("t1"), None).await.unwrap();
        store.insert(test_task("t2"), None).await.unwrap();
        let err = store.insert(test_task("t3"), None).await.unwrap_err();
        assert!(
            matches!(
                err,
                A2aError::Store(A2aStoreError::CapacityExceeded { capacity: 2 })
            ),
            "got {err:?}"
        );
        assert!(store.get("t1").await.unwrap().is_some());
        assert!(store.get("t2").await.unwrap().is_some());
        assert!(store.get("t3").await.unwrap().is_none());
    }
}
