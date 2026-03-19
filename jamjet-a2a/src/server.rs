//! A2A v1.0 server — Axum-based JSON-RPC router with SSE streaming.

use crate::store::{InMemoryTaskStore, TaskStore};
use axum::{
    extract::State,
    response::{
        sse::{Event, Sse},
        IntoResponse, Json,
    },
    routing::{get, post},
    Router,
};
use jamjet_a2a_types::*;
use std::convert::Infallible;
use std::sync::Arc;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;
use tracing::{debug, error, info};

// ────────────────────────────────────────────────────────────────────────────
// TaskHandler trait
// ────────────────────────────────────────────────────────────────────────────

/// User-provided handler that processes an incoming message for a task.
///
/// Implementations are spawned in a background `tokio::spawn` and receive
/// an `Arc<dyn TaskStore>` to update status and add artifacts as they work.
#[async_trait::async_trait]
pub trait TaskHandler: Send + Sync {
    async fn handle(
        &self,
        task_id: String,
        message: Message,
        store: Arc<dyn TaskStore>,
    ) -> Result<(), A2aError>;
}

// ────────────────────────────────────────────────────────────────────────────
// ServerState
// ────────────────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct ServerState {
    card: Arc<AgentCard>,
    store: Arc<dyn TaskStore>,
    handler: Arc<Option<Box<dyn TaskHandler>>>,
}

// ────────────────────────────────────────────────────────────────────────────
// A2aServer builder
// ────────────────────────────────────────────────────────────────────────────

/// An A2A v1.0 server that serves an Agent Card, handles JSON-RPC requests,
/// and streams SSE events.
///
/// ```rust,no_run
/// use jamjet_a2a::server::A2aServer;
/// use jamjet_a2a_types::*;
///
/// # async fn run() -> Result<(), A2aError> {
/// let card = AgentCard {
///     name: "echo".into(),
///     description: "Echo agent".into(),
///     version: "1.0".into(),
///     supported_interfaces: vec![],
///     capabilities: AgentCapabilities {
///         streaming: Some(true),
///         push_notifications: None,
///         extensions: vec![],
///         extended_agent_card: None,
///     },
///     default_input_modes: vec!["text/plain".into()],
///     default_output_modes: vec!["text/plain".into()],
///     skills: vec![],
///     provider: None,
///     security_schemes: Default::default(),
///     security_requirements: vec![],
///     signatures: vec![],
///     icon_url: None,
/// };
///
/// A2aServer::new(card).with_port(3000).start().await?;
/// # Ok(())
/// # }
/// ```
pub struct A2aServer {
    card: AgentCard,
    port: u16,
    handler: Option<Box<dyn TaskHandler>>,
    store: Option<Box<dyn TaskStore>>,
}

impl A2aServer {
    /// Create a new server builder with the given Agent Card.
    pub fn new(card: AgentCard) -> Self {
        Self {
            card,
            port: 3000,
            handler: None,
            store: None,
        }
    }

    /// Set the port to listen on (default: 3000).
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Attach a [`TaskHandler`] that processes incoming messages.
    pub fn with_handler(mut self, handler: impl TaskHandler + 'static) -> Self {
        self.handler = Some(Box::new(handler));
        self
    }

    /// Provide a custom [`TaskStore`] implementation.
    pub fn with_store(mut self, store: impl TaskStore + 'static) -> Self {
        self.store = Some(Box::new(store));
        self
    }

    /// Build an [`axum::Router`] without starting the server.
    ///
    /// Useful for composing the A2A routes into a larger application or for
    /// testing with `axum::test`.
    pub fn into_router(self) -> Router {
        let store: Arc<dyn TaskStore> = match self.store {
            Some(s) => Arc::from(s),
            None => Arc::new(InMemoryTaskStore::new()),
        };

        let state = ServerState {
            card: Arc::new(self.card),
            store,
            handler: Arc::new(self.handler),
        };

        Router::new()
            .route("/.well-known/agent-card.json", get(agent_card_handler))
            .route("/.well-known/agent.json", get(agent_card_handler))
            .route("/", post(jsonrpc_handler))
            .with_state(state)
    }

    /// Start the server, binding to `0.0.0.0:{port}`.
    pub async fn start(self) -> Result<(), A2aError> {
        let port = self.port;
        let router = self.into_router();
        let addr = format!("0.0.0.0:{port}");
        info!(addr = %addr, "starting A2A server");

        let listener = tokio::net::TcpListener::bind(&addr).await.map_err(|e| {
            A2aTransportError::Connection {
                url: addr.clone(),
                source: Box::new(e),
            }
        })?;

        axum::serve(listener, router)
            .await
            .map_err(|e| A2aTransportError::Connection {
                url: addr,
                source: Box::new(e),
            })?;

        Ok(())
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Route handlers
// ────────────────────────────────────────────────────────────────────────────

/// `GET /.well-known/agent-card.json`
async fn agent_card_handler(State(state): State<ServerState>) -> impl IntoResponse {
    Json(state.card.as_ref().clone())
}

/// Incoming JSON-RPC request parsed from the POST body.
/// We use a `Value` for params since each method has different params.
#[derive(serde::Deserialize)]
#[allow(dead_code)]
struct IncomingRpc {
    jsonrpc: String,
    id: serde_json::Value,
    method: String,
    #[serde(default)]
    params: serde_json::Value,
}

/// `POST /` — JSON-RPC 2.0 dispatcher.
async fn jsonrpc_handler(
    State(state): State<ServerState>,
    Json(rpc): Json<IncomingRpc>,
) -> axum::response::Response {
    debug!(method = %rpc.method, "incoming JSON-RPC request");

    match rpc.method.as_str() {
        // v1.0 names + v0.3 aliases
        "SendMessage" | "message/send" => handle_send_message(state, rpc).await,
        "GetTask" | "tasks/get" => handle_get_task(state, rpc).await,
        "ListTasks" => handle_list_tasks(state, rpc).await,
        "CancelTask" | "tasks/cancel" => handle_cancel_task(state, rpc).await,
        "SendStreamingMessage" | "message/stream" => handle_send_streaming(state, rpc).await,
        "SubscribeToTask" | "tasks/resubscribe" => handle_subscribe(state, rpc).await,
        "GetExtendedAgentCard" => handle_get_extended_card(state, rpc).await,
        "CreateTaskPushNotificationConfig"
        | "tasks/pushNotificationConfig/set"
        | "GetTaskPushNotificationConfig"
        | "ListTaskPushNotificationConfigs"
        | "DeleteTaskPushNotificationConfig" => make_error_response(
            rpc.id,
            A2aProtocolError::UnsupportedOperation { method: rpc.method },
        )
        .into_response(),
        _ => make_json_rpc_error_response(rpc.id, -32601, "Method not found", None).into_response(),
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Individual method handlers
// ────────────────────────────────────────────────────────────────────────────

async fn handle_send_message(state: ServerState, rpc: IncomingRpc) -> axum::response::Response {
    let req: SendMessageRequest = match serde_json::from_value(rpc.params) {
        Ok(r) => r,
        Err(e) => {
            return make_json_rpc_error_response(
                rpc.id,
                -32602,
                &format!("Invalid params: {e}"),
                None,
            )
            .into_response();
        }
    };

    let message = req.message.clone();
    let task_id = message
        .task_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let context_id = message.context_id.clone();

    let task = Task {
        id: task_id.clone(),
        context_id,
        status: TaskStatus {
            state: TaskState::Submitted,
            message: None,
            timestamp: Some(chrono::Utc::now().to_rfc3339()),
        },
        artifacts: vec![],
        history: Some(vec![message.clone()]),
        metadata: req.metadata.clone(),
    };

    if let Err(e) = state.store.insert(task.clone()).await {
        error!(error = %e, "failed to insert task");
        return make_error_response(rpc.id, e).into_response();
    }

    // Spawn the handler in the background.
    if state.handler.is_some() {
        let handler = Arc::clone(&state.handler);
        let store = Arc::clone(&state.store);
        let tid = task_id.clone();
        let msg = message;

        tokio::spawn(async move {
            // Transition to Working.
            let working_status = TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: Some(chrono::Utc::now().to_rfc3339()),
            };
            store.update_status(&tid, working_status).await.ok();

            if let Some(ref h) = *handler {
                match h.handle(tid.clone(), msg, store.clone()).await {
                    Ok(()) => {}
                    Err(e) => {
                        error!(task_id = %tid, error = %e, "handler failed");
                        let failed_status = TaskStatus {
                            state: TaskState::Failed,
                            message: Some(Message {
                                message_id: uuid::Uuid::new_v4().to_string(),
                                context_id: None,
                                task_id: Some(tid.clone()),
                                role: Role::Agent,
                                parts: vec![Part {
                                    content: PartContent::Text(format!("Handler error: {e}")),
                                    metadata: None,
                                    filename: None,
                                    media_type: None,
                                }],
                                metadata: None,
                                extensions: vec![],
                                reference_task_ids: vec![],
                            }),
                            timestamp: Some(chrono::Utc::now().to_rfc3339()),
                        };
                        store.update_status(&tid, failed_status).await.ok();
                    }
                }
            }
        });
    }

    // If return_immediately is set, return the task in Submitted state.
    // Otherwise, return the task as-is (the handler runs in background either way).
    let resp_task = state.store.get(&task_id).await.unwrap_or(Some(task));
    make_success_response(rpc.id, &resp_task).into_response()
}

async fn handle_get_task(state: ServerState, rpc: IncomingRpc) -> axum::response::Response {
    let req: GetTaskRequest = match serde_json::from_value(rpc.params) {
        Ok(r) => r,
        Err(e) => {
            return make_json_rpc_error_response(
                rpc.id,
                -32602,
                &format!("Invalid params: {e}"),
                None,
            )
            .into_response();
        }
    };

    match state.store.get(&req.id).await {
        Ok(Some(task)) => make_success_response(rpc.id, &task).into_response(),
        Ok(None) => make_error_response(rpc.id, A2aProtocolError::TaskNotFound { task_id: req.id })
            .into_response(),
        Err(e) => make_error_response(rpc.id, e).into_response(),
    }
}

async fn handle_list_tasks(state: ServerState, rpc: IncomingRpc) -> axum::response::Response {
    let req: ListTasksRequest = match serde_json::from_value(rpc.params) {
        Ok(r) => r,
        Err(e) => {
            return make_json_rpc_error_response(
                rpc.id,
                -32602,
                &format!("Invalid params: {e}"),
                None,
            )
            .into_response();
        }
    };

    match state.store.list(&req).await {
        Ok(resp) => make_success_response(rpc.id, &resp).into_response(),
        Err(e) => make_error_response(rpc.id, e).into_response(),
    }
}

async fn handle_cancel_task(state: ServerState, rpc: IncomingRpc) -> axum::response::Response {
    let req: CancelTaskRequest = match serde_json::from_value(rpc.params) {
        Ok(r) => r,
        Err(e) => {
            return make_json_rpc_error_response(
                rpc.id,
                -32602,
                &format!("Invalid params: {e}"),
                None,
            )
            .into_response();
        }
    };

    let task_id = req.id.clone();
    match state.store.cancel(&task_id).await {
        Ok(()) => {
            // Return the updated task.
            match state.store.get(&task_id).await {
                Ok(Some(task)) => make_success_response(rpc.id, &task).into_response(),
                Ok(None) => make_error_response(rpc.id, A2aProtocolError::TaskNotFound { task_id })
                    .into_response(),
                Err(e) => make_error_response(rpc.id, e).into_response(),
            }
        }
        Err(e) => make_error_response(rpc.id, e).into_response(),
    }
}

async fn handle_send_streaming(state: ServerState, rpc: IncomingRpc) -> axum::response::Response {
    let req: SendMessageRequest = match serde_json::from_value(rpc.params) {
        Ok(r) => r,
        Err(e) => {
            return make_json_rpc_error_response(
                rpc.id,
                -32602,
                &format!("Invalid params: {e}"),
                None,
            )
            .into_response();
        }
    };

    let message = req.message.clone();
    let task_id = message
        .task_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let context_id = message.context_id.clone();

    let task = Task {
        id: task_id.clone(),
        context_id,
        status: TaskStatus {
            state: TaskState::Submitted,
            message: None,
            timestamp: Some(chrono::Utc::now().to_rfc3339()),
        },
        artifacts: vec![],
        history: Some(vec![message.clone()]),
        metadata: req.metadata.clone(),
    };

    if let Err(e) = state.store.insert(task).await {
        error!(error = %e, "failed to insert task");
        return make_error_response(rpc.id, e).into_response();
    }

    // Get a receiver before spawning the handler so we don't miss events.
    let rx = state.store.subscribe(&task_id).await;

    // Spawn the handler.
    if state.handler.is_some() {
        let handler = Arc::clone(&state.handler);
        let store = Arc::clone(&state.store);
        let tid = task_id.clone();
        let msg = message;

        tokio::spawn(async move {
            let working_status = TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: Some(chrono::Utc::now().to_rfc3339()),
            };
            store.update_status(&tid, working_status).await.ok();

            if let Some(ref h) = *handler {
                match h.handle(tid.clone(), msg, store.clone()).await {
                    Ok(()) => {}
                    Err(e) => {
                        error!(task_id = %tid, error = %e, "handler failed");
                        let failed_status = TaskStatus {
                            state: TaskState::Failed,
                            message: Some(Message {
                                message_id: uuid::Uuid::new_v4().to_string(),
                                context_id: None,
                                task_id: Some(tid.clone()),
                                role: Role::Agent,
                                parts: vec![Part {
                                    content: PartContent::Text(format!("Handler error: {e}")),
                                    metadata: None,
                                    filename: None,
                                    media_type: None,
                                }],
                                metadata: None,
                                extensions: vec![],
                                reference_task_ids: vec![],
                            }),
                            timestamp: Some(chrono::Utc::now().to_rfc3339()),
                        };
                        store.update_status(&tid, failed_status).await.ok();
                    }
                }
            }
        });
    }

    match rx {
        Some(rx) => make_sse_stream(rx).into_response(),
        None => make_json_rpc_error_response(rpc.id, -32603, "Failed to create event stream", None)
            .into_response(),
    }
}

async fn handle_subscribe(state: ServerState, rpc: IncomingRpc) -> axum::response::Response {
    let req: SubscribeToTaskRequest = match serde_json::from_value(rpc.params) {
        Ok(r) => r,
        Err(e) => {
            return make_json_rpc_error_response(
                rpc.id,
                -32602,
                &format!("Invalid params: {e}"),
                None,
            )
            .into_response();
        }
    };

    // Verify the task exists.
    match state.store.get(&req.id).await {
        Ok(None) => {
            return make_error_response(rpc.id, A2aProtocolError::TaskNotFound { task_id: req.id })
                .into_response();
        }
        Err(e) => return make_error_response(rpc.id, e).into_response(),
        Ok(Some(_)) => {}
    }

    match state.store.subscribe(&req.id).await {
        Some(rx) => make_sse_stream(rx).into_response(),
        None => {
            make_json_rpc_error_response(rpc.id, -32603, "Failed to subscribe to task events", None)
                .into_response()
        }
    }
}

async fn handle_get_extended_card(
    state: ServerState,
    rpc: IncomingRpc,
) -> axum::response::Response {
    make_success_response(rpc.id, state.card.as_ref()).into_response()
}

// ────────────────────────────────────────────────────────────────────────────
// SSE stream helper
// ────────────────────────────────────────────────────────────────────────────

fn make_sse_stream(
    rx: tokio::sync::broadcast::Receiver<StreamResponse>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let stream = BroadcastStream::new(rx).filter_map(|result| match result {
        Ok(event) => {
            let json = serde_json::to_string(&event).unwrap_or_default();
            Some(Ok(Event::default().data(json)))
        }
        Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n)) => {
            debug!(lagged = n, "SSE client lagged behind");
            None
        }
    });
    Sse::new(stream)
}

// ────────────────────────────────────────────────────────────────────────────
// JSON-RPC response helpers
// ────────────────────────────────────────────────────────────────────────────

fn make_success_response<T: serde::Serialize>(
    id: serde_json::Value,
    result: &T,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    }))
}

fn make_error_response(
    id: serde_json::Value,
    error: impl Into<A2aError>,
) -> Json<serde_json::Value> {
    let error: A2aError = error.into();
    match error {
        A2aError::Protocol(ref proto) => {
            make_json_rpc_error_response(id, proto.json_rpc_code(), &error.to_string(), None)
        }
        _ => make_json_rpc_error_response(id, -32603, &error.to_string(), None),
    }
}

fn make_json_rpc_error_response(
    id: serde_json::Value,
    code: i32,
    message: &str,
    data: Option<serde_json::Value>,
) -> Json<serde_json::Value> {
    let mut error = serde_json::json!({
        "code": code,
        "message": message,
    });
    if let Some(d) = data {
        error["data"] = d;
    }
    Json(serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": error,
    }))
}
