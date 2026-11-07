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
/// - A `contextId` is scoped to its owner: two owners may use the same
///   value without seeing each other's tasks.
/// - `list` returns only the tasks whose owner equals the `owner` argument.
///   The server validates a `pageToken` by looking it up as a task id, so
///   cursors must be task ids.
/// - `subscribe` returns a channel for that one task. Every status change
///   (including the terminal one, and cancellation) and every artifact is
///   sent on it. When a task is evicted its sender is dropped, which ends
///   open streams.
/// - `cancel` checks the state and writes `Canceled` atomically.
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

    /// True if the task exists and is bound to `owner`.
    ///
    /// The default goes through [`get_record`](Self::get_record), which
    /// copies the task; stores should override it with a cheaper lookup.
    async fn is_visible(&self, task_id: &str, owner: Option<&str>) -> Result<bool, A2aError> {
        Ok(self
            .get_record(task_id)
            .await?
            .is_some_and(|r| r.owner.as_deref() == owner))
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
///
/// Count and byte limits apply per owner and in total. When an owner reaches
/// its own limit, the store evicts that owner's least recently updated
/// *terminal* task. When the store as a whole is full, it evicts the least
/// recently updated terminal task of an owner holding more than its fair
/// share (the limit divided by the number of owners), and only then one of
/// the inserting owner's own. Active tasks are never evicted to make room;
/// if nothing can be evicted the insert fails
/// ([`A2aStoreError::OwnerCapacityExceeded`], [`A2aStoreError::CapacityExceeded`],
/// [`A2aStoreError::OwnerStorageFull`] or [`A2aStoreError::StorageFull`]).
///
/// Without an authenticator every task has the same (empty) owner, so the
/// per-owner limits are the limits of the whole server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreLimits {
    /// Maximum number of tasks held at once, across all owners.
    pub max_tasks: usize,
    /// Maximum number of tasks one owner holds at once.
    pub max_tasks_per_owner: usize,
    /// Maximum bytes of history and artifacts (as serialized JSON), across
    /// all owners.
    pub max_total_bytes: usize,
    /// Maximum bytes of history and artifacts one owner holds.
    pub max_bytes_per_owner: usize,
    /// Terminal tasks not updated for this long are evicted.
    pub terminal_ttl: Option<Duration>,
    /// Submitted and working tasks not updated for this long are evicted,
    /// so work whose handler died does not hold its slot forever.
    pub active_ttl: Option<Duration>,
    /// Tasks waiting on the client (input-required, auth-required) not
    /// updated for this long are evicted. The default is `None`: a task
    /// waiting on a human is never dropped for being idle.
    pub interrupted_ttl: Option<Duration>,
    /// Maximum number of messages in one task's history.
    pub max_history_messages: usize,
    /// Maximum size of one task's history, as serialized JSON.
    pub max_history_bytes: usize,
}

impl StoreLimits {
    /// Default maximum number of tasks across all owners: 10,000.
    pub const DEFAULT_MAX_TASKS: usize = 10_000;
    /// Default maximum number of tasks per owner: 1,000.
    pub const DEFAULT_MAX_TASKS_PER_OWNER: usize = 1_000;
    /// Default byte limit across all owners: 1 GiB.
    pub const DEFAULT_MAX_TOTAL_BYTES: usize = 1024 * 1024 * 1024;
    /// Default byte limit per owner: 256 MiB.
    pub const DEFAULT_MAX_BYTES_PER_OWNER: usize = 256 * 1024 * 1024;
    /// Default idle time before a terminal task is evicted: one hour.
    pub const DEFAULT_TERMINAL_TTL: Duration = Duration::from_secs(60 * 60);
    /// Default idle time before a submitted or working task is evicted: 24 h.
    pub const DEFAULT_ACTIVE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
    /// Default maximum number of messages per task: 1,000.
    pub const DEFAULT_MAX_HISTORY_MESSAGES: usize = 1_000;
    /// Default maximum history size per task: 16 MiB.
    pub const DEFAULT_MAX_HISTORY_BYTES: usize = 16 * 1024 * 1024;
}

impl Default for StoreLimits {
    fn default() -> Self {
        Self {
            max_tasks: Self::DEFAULT_MAX_TASKS,
            max_tasks_per_owner: Self::DEFAULT_MAX_TASKS_PER_OWNER,
            max_total_bytes: Self::DEFAULT_MAX_TOTAL_BYTES,
            max_bytes_per_owner: Self::DEFAULT_MAX_BYTES_PER_OWNER,
            terminal_ttl: Some(Self::DEFAULT_TERMINAL_TTL),
            active_ttl: Some(Self::DEFAULT_ACTIVE_TTL),
            interrupted_ttl: None,
            max_history_messages: Self::DEFAULT_MAX_HISTORY_MESSAGES,
            max_history_bytes: Self::DEFAULT_MAX_HISTORY_BYTES,
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
    /// Serialized size of `task.history`.
    history_bytes: usize,
    /// Serialized size of the history and artifacts, counted against the
    /// byte limits.
    bytes: usize,
}

#[derive(Default, Clone, Copy)]
struct Usage {
    tasks: usize,
    bytes: usize,
}

struct InMemoryInner {
    tasks: HashMap<String, Entry>,
    /// Insertion-order list of task IDs (used for cursor-based pagination).
    order: Vec<String>,
    /// Live tasks and stored bytes per owner.
    per_owner: HashMap<Option<String>, Usage>,
    total_bytes: usize,
}

fn json_bytes<T: serde::Serialize>(value: &T) -> usize {
    serde_json::to_vec(value).map(|v| v.len()).unwrap_or(0)
}

/// Which limit an eviction is trying to satisfy.
#[derive(Clone, Copy)]
enum Measure {
    Tasks,
    Bytes,
}

impl InMemoryInner {
    fn is_expired(entry: &Entry, limits: &StoreLimits, now: Instant) -> bool {
        let state = &entry.task.status.state;
        let ttl = if state.is_terminal() {
            limits.terminal_ttl
        } else if state.is_interrupted() {
            limits.interrupted_ttl
        } else {
            limits.active_ttl
        };
        match ttl {
            Some(ttl) => now.saturating_duration_since(entry.last_update) >= ttl,
            None => false,
        }
    }

    fn usage(&self, owner: &Option<String>) -> Usage {
        self.per_owner.get(owner).copied().unwrap_or_default()
    }

    fn add_usage(&mut self, owner: &Option<String>, tasks: usize, bytes: usize) {
        let u = self.per_owner.entry(owner.clone()).or_default();
        u.tasks += tasks;
        u.bytes += bytes;
        self.total_bytes += bytes;
    }

    /// Drop a task's map entry and usage. The caller fixes `order`.
    fn forget(&mut self, task_id: &str) -> bool {
        let Some(entry) = self.tasks.remove(task_id) else {
            return false;
        };
        self.total_bytes -= entry.bytes;
        if let Some(u) = self.per_owner.get_mut(&entry.owner) {
            u.tasks -= 1;
            u.bytes -= entry.bytes;
            if u.tasks == 0 {
                self.per_owner.remove(&entry.owner);
            }
        }
        true
    }

    fn remove(&mut self, task_id: &str) {
        if self.forget(task_id) {
            self.order.retain(|id| id != task_id);
        }
    }

    /// Remove every task whose TTL has run out.
    fn evict_expired(&mut self, limits: &StoreLimits, now: Instant) {
        let expired: Vec<String> = self
            .tasks
            .iter()
            .filter(|(_, e)| Self::is_expired(e, limits, now))
            .map(|(id, _)| id.clone())
            .collect();
        if expired.is_empty() {
            return;
        }
        for id in &expired {
            self.forget(id);
        }
        let tasks = &self.tasks;
        self.order.retain(|id| tasks.contains_key(id));
        debug!(evicted = expired.len(), "evicted expired tasks");
    }

    /// Look up a live task, evicting it first if its TTL has run out.
    fn live_mut(
        &mut self,
        task_id: &str,
        limits: &StoreLimits,
        now: Instant,
    ) -> Option<&mut Entry> {
        if Self::is_expired(self.tasks.get(task_id)?, limits, now) {
            self.remove(task_id);
            return None;
        }
        self.tasks.get_mut(task_id)
    }

    /// Like [`Self::live_mut`], but refuses tasks in a terminal state.
    fn live_open_mut(
        &mut self,
        task_id: &str,
        limits: &StoreLimits,
        now: Instant,
    ) -> Result<&mut Entry, A2aError> {
        let entry =
            self.live_mut(task_id, limits, now)
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

    /// Evict the least recently updated terminal task among those `pick`
    /// accepts. Returns false if there was none.
    fn evict_oldest_terminal(&mut self, pick: impl Fn(&Entry) -> bool) -> bool {
        let victim = self
            .tasks
            .iter()
            .filter(|(_, e)| e.task.status.state.is_terminal() && pick(e))
            .min_by_key(|(_, e)| e.last_update)
            .map(|(id, _)| id.clone());
        match victim {
            Some(id) => {
                self.remove(&id);
                debug!(task_id = %id, "evicted terminal task to make room");
                true
            }
            None => false,
        }
    }

    /// Free global room for `owner`: first take a finished task from an
    /// owner over its fair share of `limit`, then one of `owner`'s own.
    fn evict_for_global(&mut self, owner: &Option<String>, limit: usize, measure: Measure) -> bool {
        let owners = self.per_owner.len() + usize::from(!self.per_owner.contains_key(owner));
        let fair = limit / owners.max(1);
        let usage = self.per_owner.clone();
        let over_share = |e: &Entry| {
            let u = usage.get(&e.owner).copied().unwrap_or_default();
            match measure {
                Measure::Tasks => u.tasks > fair,
                Measure::Bytes => u.bytes > fair,
            }
        };
        self.evict_oldest_terminal(over_share) || self.evict_oldest_terminal(|e| e.owner == *owner)
    }

    /// Make room for one more task of `owner` (if `new_task`) carrying
    /// `extra_bytes` more history or artifacts.
    fn make_room(
        &mut self,
        owner: &Option<String>,
        new_task: bool,
        extra_bytes: usize,
        limits: &StoreLimits,
    ) -> Result<(), A2aError> {
        if new_task {
            while self.usage(owner).tasks >= limits.max_tasks_per_owner {
                if !self.evict_oldest_terminal(|e| e.owner == *owner) {
                    return Err(A2aStoreError::OwnerCapacityExceeded {
                        capacity: limits.max_tasks_per_owner,
                    }
                    .into());
                }
            }
        }
        while self.usage(owner).bytes + extra_bytes > limits.max_bytes_per_owner {
            if !self.evict_oldest_terminal(|e| e.owner == *owner) {
                return Err(A2aStoreError::OwnerStorageFull {
                    limit_bytes: limits.max_bytes_per_owner,
                }
                .into());
            }
        }
        if new_task {
            while self.tasks.len() >= limits.max_tasks {
                if !self.evict_for_global(owner, limits.max_tasks, Measure::Tasks) {
                    return Err(A2aStoreError::CapacityExceeded {
                        capacity: limits.max_tasks,
                    }
                    .into());
                }
            }
        }
        while self.total_bytes + extra_bytes > limits.max_total_bytes {
            if !self.evict_for_global(owner, limits.max_total_bytes, Measure::Bytes) {
                return Err(A2aStoreError::StorageFull {
                    limit_bytes: limits.max_total_bytes,
                }
                .into());
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
/// The store is bounded by [`StoreLimits`]. By default it holds at most
/// 10,000 tasks and 1 GiB of history and artifacts, 1,000 tasks and
/// 256 MiB per owner, and 1,000 messages or 16 MiB of history per task.
/// Terminal tasks are evicted after an hour without updates, submitted and
/// working tasks after 24 hours, and tasks waiting for input are kept.
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
                per_owner: HashMap::new(),
                total_bytes: 0,
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
        let limits = &self.limits;
        let mut inner = self.inner.lock().await;
        inner.evict_expired(limits, now);
        let task_id = task.id.clone();
        if inner.tasks.contains_key(&task_id) {
            return Err(A2aStoreError::TaskAlreadyExists { task_id }.into());
        }

        let history_bytes = task.history.iter().flatten().map(json_bytes).sum::<usize>();
        let bytes = history_bytes + task.artifacts.iter().map(json_bytes).sum::<usize>();
        inner.make_room(&owner, true, bytes, limits)?;

        let (tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        inner.add_usage(&owner, 1, bytes);
        inner.order.push(task_id.clone());
        inner.tasks.insert(
            task_id,
            Entry {
                task,
                owner,
                tx,
                last_update: now,
                history_bytes,
                bytes,
            },
        );
        Ok(())
    }

    async fn is_visible(&self, task_id: &str, owner: Option<&str>) -> Result<bool, A2aError> {
        let now = Instant::now();
        let mut inner = self.inner.lock().await;
        Ok(inner
            .live_mut(task_id, &self.limits, now)
            .is_some_and(|e| e.owner.as_deref() == owner))
    }

    async fn get_record(&self, task_id: &str) -> Result<Option<TaskRecord>, A2aError> {
        let now = Instant::now();
        let mut inner = self.inner.lock().await;
        Ok(inner
            .live_mut(task_id, &self.limits, now)
            .map(|e| TaskRecord {
                task: e.task.clone(),
                owner: e.owner.clone(),
            }))
    }

    async fn update_status(&self, task_id: &str, status: TaskStatus) -> Result<(), A2aError> {
        let now = Instant::now();
        let mut inner = self.inner.lock().await;
        let entry = inner.live_open_mut(task_id, &self.limits, now)?;
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
        let size = json_bytes(&artifact);
        let limits = &self.limits;
        let mut inner = self.inner.lock().await;
        let owner = inner.live_open_mut(task_id, limits, now)?.owner.clone();
        // The task itself is not terminal, so it is never evicted here.
        inner.make_room(&owner, false, size, limits)?;
        inner.add_usage(&owner, 0, size);

        let entry = inner.live_open_mut(task_id, limits, now)?;
        entry.task.artifacts.push(artifact.clone());
        entry.bytes += size;
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
        let size = json_bytes(&message);
        let limits = &self.limits;
        let mut inner = self.inner.lock().await;
        let entry = inner.live_open_mut(task_id, limits, now)?;
        let count = entry.task.history.as_ref().map_or(0, Vec::len);
        if count + 1 > limits.max_history_messages
            || entry.history_bytes + size > limits.max_history_bytes
        {
            return Err(A2aStoreError::HistoryFull {
                task_id: task_id.to_string(),
            }
            .into());
        }
        let owner = entry.owner.clone();
        // The task itself is not terminal, so it is never evicted here.
        inner.make_room(&owner, false, size, limits)?;
        inner.add_usage(&owner, 0, size);

        let entry = inner.live_open_mut(task_id, limits, now)?;
        entry
            .task
            .history
            .get_or_insert_with(Vec::new)
            .push(message);
        entry.history_bytes += size;
        entry.bytes += size;
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
        inner.evict_expired(&self.limits, now);

        let page_size = req.page_size.unwrap_or(50).clamp(1, 100) as usize;
        let history_length = req.history_length;
        let include_artifacts = req.include_artifacts.unwrap_or(false);

        // Parse statusTimestampAfter filter.
        let ts_after = req
            .status_timestamp_after
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok());

        // Step 1: Find the matching tasks, most recently inserted first (per
        // A2A spec), without copying them. Only tasks bound to `owner` are
        // visible.
        let matching: Vec<&Task> = inner
            .order
            .iter()
            .rev()
            .filter_map(|id| inner.tasks.get(id))
            .filter(|entry| entry.owner.as_deref() == owner)
            .map(|entry| &entry.task)
            .filter(|task| match &req.context_id {
                Some(ctx) => task.context_id.as_deref() == Some(ctx.as_str()),
                None => true,
            })
            .filter(|task| match &req.status {
                Some(status) => task.status.state == *status,
                None => true,
            })
            .filter(|task| match &ts_after {
                Some(cutoff) => task
                    .status
                    .timestamp
                    .as_deref()
                    .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                    .is_some_and(|t| t >= *cutoff),
                None => true,
            })
            .collect();

        let total_size = matching.len();

        // Step 2: Cursor-based pagination on the filtered set.
        let start_idx = match req.page_token.as_deref() {
            None | Some("") => 0,
            // Find the cursor in the filtered list and start after it.
            Some(token) => matching
                .iter()
                .position(|t| t.id == token)
                .map(|pos| pos + 1)
                .unwrap_or(total_size),
        };

        // Step 3: Copy only the page.
        let page: Vec<Task> = matching
            .iter()
            .skip(start_idx)
            .take(page_size)
            .map(|task| {
                let mut task = (*task).clone();
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

        // next_page_token: the last id on this page if more tasks follow,
        // otherwise an empty string.
        let next_page_token = if page.len() == page_size && start_idx + page_size < total_size {
            page.last().map(|t| t.id.clone()).unwrap_or_default()
        } else {
            String::new()
        };

        // pageSize in response = actual number of tasks returned (capped by request).
        Ok(ListTasksResponse {
            page_size: page.len() as i32,
            tasks: page,
            next_page_token,
            total_size: total_size as i32,
        })
    }

    async fn cancel(&self, task_id: &str) -> Result<(), A2aError> {
        let now = Instant::now();
        let mut inner = self.inner.lock().await;
        let entry = inner.live_mut(task_id, &self.limits, now).ok_or_else(|| {
            A2aProtocolError::TaskNotFound {
                task_id: task_id.to_string(),
            }
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
            .live_mut(task_id, &self.limits, now)
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
            active_ttl: Some(Duration::from_secs(60)),
            ..Default::default()
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
            active_ttl: Some(Duration::from_secs(60)),
            ..Default::default()
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
            terminal_ttl: None,
            ..Default::default()
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
            terminal_ttl: None,
            ..Default::default()
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

    fn with_context(id: &str, ctx: &str) -> Task {
        let mut t = test_task(id);
        t.context_id = Some(ctx.into());
        t
    }

    // ── per-owner limits ──────────────────────────────────────────────────

    #[tokio::test(start_paused = true)]
    async fn eviction_only_takes_the_inserting_owners_tasks() {
        let store = InMemoryTaskStore::with_limits(StoreLimits {
            max_tasks: 3,
            terminal_ttl: None,
            ..Default::default()
        });
        let alice = || Some("alice".to_string());
        let bob = || Some("bob".to_string());
        store.insert(test_task("a1"), alice()).await.unwrap();
        store
            .update_status("a1", status(TaskState::Completed))
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(1)).await;
        for id in ["b1", "b2"] {
            store.insert(test_task(id), bob()).await.unwrap();
            store
                .update_status(id, status(TaskState::Completed))
                .await
                .unwrap();
            tokio::time::advance(Duration::from_secs(1)).await;
        }

        // Full. Bob's insert evicts bob's oldest result, never alice's.
        store.insert(test_task("b3"), bob()).await.unwrap();
        assert!(store.get("a1").await.unwrap().is_some());
        assert!(store.get("b1").await.unwrap().is_none());
        assert!(store.get("b2").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn one_owner_cannot_fill_the_store_for_everyone() {
        let store = InMemoryTaskStore::with_limits(StoreLimits {
            max_tasks: 10,
            max_tasks_per_owner: 2,
            ..Default::default()
        });
        store
            .insert(test_task("b1"), Some("bob".into()))
            .await
            .unwrap();
        store
            .insert(test_task("b2"), Some("bob".into()))
            .await
            .unwrap();
        let err = store
            .insert(test_task("b3"), Some("bob".into()))
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                A2aError::Store(A2aStoreError::OwnerCapacityExceeded { capacity: 2 })
            ),
            "got {err:?}"
        );
        store
            .insert(test_task("a1"), Some("alice".into()))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn global_ceiling_holds_when_the_caller_has_nothing_to_evict() {
        let store = InMemoryTaskStore::with_limits(StoreLimits {
            max_tasks: 2,
            ..Default::default()
        });
        // Bob's tasks are still running, so nothing may be evicted.
        for id in ["b1", "b2"] {
            store
                .insert(test_task(id), Some("bob".into()))
                .await
                .unwrap();
        }
        let err = store
            .insert(test_task("a1"), Some("alice".into()))
            .await
            .unwrap_err();
        assert!(
            matches!(err, A2aError::Store(A2aStoreError::CapacityExceeded { .. })),
            "got {err:?}"
        );
        assert!(store.get("b1").await.unwrap().is_some());
        assert!(store.get("b2").await.unwrap().is_some());
    }

    // ── TTL by state ──────────────────────────────────────────────────────

    #[tokio::test(start_paused = true)]
    async fn default_ttl_keeps_waiting_tasks_and_drops_old_results() {
        let store = InMemoryTaskStore::new();
        store.insert(test_task("waiting"), None).await.unwrap();
        store
            .update_status("waiting", status(TaskState::InputRequired))
            .await
            .unwrap();
        store.insert(test_task("stuck"), None).await.unwrap();
        store
            .update_status("stuck", status(TaskState::Working))
            .await
            .unwrap();
        store.insert(test_task("done"), None).await.unwrap();
        store
            .update_status("done", status(TaskState::Completed))
            .await
            .unwrap();

        tokio::time::advance(Duration::from_secs(2 * 60 * 60)).await;
        assert!(store.get("done").await.unwrap().is_none());
        assert!(store.get("stuck").await.unwrap().is_some());

        // Work that never finishes is dropped after a day; a task waiting
        // for a human is not.
        tokio::time::advance(Duration::from_secs(46 * 60 * 60)).await;
        assert!(store.get("stuck").await.unwrap().is_none());
        assert!(store.get("waiting").await.unwrap().is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn full_store_takes_finished_tasks_from_owners_over_their_share() {
        let store = InMemoryTaskStore::with_limits(StoreLimits {
            max_tasks: 4,
            terminal_ttl: None,
            ..Default::default()
        });
        for (id, owner) in [("b1", "bob"), ("b2", "bob"), ("b3", "bob"), ("a1", "alice")] {
            store
                .insert(test_task(id), Some(owner.into()))
                .await
                .unwrap();
            store
                .update_status(id, status(TaskState::Completed))
                .await
                .unwrap();
            tokio::time::advance(Duration::from_secs(1)).await;
        }

        // A new caller is not locked out by others' finished tasks: bob is
        // over his share, so his oldest result goes; alice's stays.
        store
            .insert(test_task("c1"), Some("carol".into()))
            .await
            .unwrap();
        assert!(store.get("b1").await.unwrap().is_none());
        assert!(store.get("a1").await.unwrap().is_some());
        assert!(store.get("b2").await.unwrap().is_some());
    }

    // ── byte budgets ──────────────────────────────────────────────────────

    fn task_with_text(id: &str, len: usize) -> Task {
        let mut t = test_task(id);
        t.history = Some(vec![msg(&format!("{id}-m"), &"x".repeat(len))]);
        t
    }

    #[tokio::test]
    async fn owner_byte_budget_is_enforced() {
        let store = InMemoryTaskStore::with_limits(StoreLimits {
            max_bytes_per_owner: 4096,
            ..Default::default()
        });
        store
            .insert(task_with_text("b1", 3000), Some("bob".into()))
            .await
            .unwrap();
        let err = store
            .insert(task_with_text("b2", 3000), Some("bob".into()))
            .await
            .unwrap_err();
        assert!(
            matches!(err, A2aError::Store(A2aStoreError::OwnerStorageFull { .. })),
            "got {err:?}"
        );
        let err = store
            .append_message("b1", msg("b1-m2", &"y".repeat(3000)))
            .await
            .unwrap_err();
        assert!(
            matches!(err, A2aError::Store(A2aStoreError::OwnerStorageFull { .. })),
            "got {err:?}"
        );
        // Another owner has its own budget.
        store
            .insert(task_with_text("a1", 3000), Some("alice".into()))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn total_byte_budget_is_enforced() {
        let store = InMemoryTaskStore::with_limits(StoreLimits {
            max_total_bytes: 8192,
            ..Default::default()
        });
        store
            .insert(task_with_text("b1", 3000), Some("bob".into()))
            .await
            .unwrap();
        store
            .insert(task_with_text("a1", 3000), Some("alice".into()))
            .await
            .unwrap();
        let err = store
            .insert(task_with_text("c1", 3000), Some("carol".into()))
            .await
            .unwrap_err();
        assert!(
            matches!(err, A2aError::Store(A2aStoreError::StorageFull { .. })),
            "got {err:?}"
        );
        let artifact = Artifact {
            artifact_id: "big".into(),
            name: None,
            description: None,
            parts: vec![Part {
                content: PartContent::Text("z".repeat(3000)),
                metadata: None,
                filename: None,
                media_type: None,
            }],
            metadata: None,
            extensions: vec![],
        };
        let err = store.add_artifact("b1", artifact).await.unwrap_err();
        assert!(
            matches!(err, A2aError::Store(A2aStoreError::StorageFull { .. })),
            "got {err:?}"
        );
    }

    // ── history limits ────────────────────────────────────────────────────

    fn msg(id: &str, text: &str) -> Message {
        Message {
            message_id: id.into(),
            context_id: None,
            task_id: None,
            role: Role::User,
            parts: vec![Part {
                content: PartContent::Text(text.into()),
                metadata: None,
                filename: None,
                media_type: None,
            }],
            metadata: None,
            extensions: vec![],
            reference_task_ids: vec![],
        }
    }

    #[tokio::test]
    async fn history_message_count_is_capped() {
        let store = InMemoryTaskStore::with_limits(StoreLimits {
            max_history_messages: 2,
            ..Default::default()
        });
        let mut task = test_task("t1");
        task.history = Some(vec![msg("m1", "a")]);
        store.insert(task, None).await.unwrap();
        store.append_message("t1", msg("m2", "b")).await.unwrap();
        let err = store
            .append_message("t1", msg("m3", "c"))
            .await
            .unwrap_err();
        assert!(
            matches!(err, A2aError::Store(A2aStoreError::HistoryFull { .. })),
            "got {err:?}"
        );
        let task = store.get("t1").await.unwrap().unwrap();
        assert_eq!(task.history.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn history_bytes_are_capped() {
        let store = InMemoryTaskStore::with_limits(StoreLimits {
            max_history_bytes: 1024,
            ..Default::default()
        });
        store.insert(test_task("t1"), None).await.unwrap();
        store
            .append_message("t1", msg("m1", "small"))
            .await
            .unwrap();
        let err = store
            .append_message("t1", msg("m2", &"x".repeat(2048)))
            .await
            .unwrap_err();
        assert!(
            matches!(err, A2aError::Store(A2aStoreError::HistoryFull { .. })),
            "got {err:?}"
        );
    }

    // ── contextId ownership ───────────────────────────────────────────────

    #[tokio::test]
    async fn context_ids_are_per_owner() {
        let store = InMemoryTaskStore::new();
        store
            .insert(with_context("a1", "ctx-1"), Some("alice".into()))
            .await
            .unwrap();
        // Bob may use the same contextId; he only ever sees his own tasks.
        store
            .insert(with_context("b1", "ctx-1"), Some("bob".into()))
            .await
            .unwrap();
        let req = ListTasksRequest {
            context_id: Some("ctx-1".into()),
            ..Default::default()
        };
        let bob = store.list(&req, Some("bob")).await.unwrap();
        assert_eq!(bob.tasks.len(), 1);
        assert_eq!(bob.tasks[0].id, "b1");
    }
}
