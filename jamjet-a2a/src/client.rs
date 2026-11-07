//! A2A v1.0 client — discover agents, send messages, stream events, manage push configs.

use jamjet_a2a_types::*;
use std::time::Duration;
use tracing::{debug, info};

// ────────────────────────────────────────────────────────────────────────────
// A2aClient
// ────────────────────────────────────────────────────────────────────────────

/// Async HTTP client for communicating with A2A v1.0 agents.
///
/// Wraps [`reqwest::Client`] (which is `Arc`-backed), so cloning is cheap.
///
/// Every call is bounded in time. Unary calls (`send_message`, `get_task`,
/// `discover`, ...) fail with [`A2aTransportError::Timeout`] after the
/// request timeout (default 60 s). Streaming calls must receive response
/// headers within the request timeout, and then fail if no bytes arrive for
/// the stream idle timeout (default 5 min).
///
/// ```rust
/// use jamjet_a2a::A2aClient;
/// use std::time::Duration;
///
/// let client = A2aClient::new()
///     .with_token("my-bearer-token")
///     .with_request_timeout(Duration::from_secs(30));
/// ```
#[derive(Clone)]
pub struct A2aClient {
    http: reqwest::Client,
    pub(crate) bearer_token: Option<String>,
    request_timeout: Duration,
    stream_idle_timeout: Duration,
}

impl A2aClient {
    /// Connect timeout of the HTTP client built by [`A2aClient::new`].
    pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
    /// Default limit for a whole unary request, and for the response headers
    /// of a streaming request.
    pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
    /// Default longest gap between bytes on an open stream.
    pub const DEFAULT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

    /// Create a new client with the default timeouts and no bearer token.
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Self::DEFAULT_CONNECT_TIMEOUT)
            .build()
            .unwrap_or_default();
        Self::from_http_client(http)
    }

    /// Create a client on top of an existing `reqwest::Client`, for example
    /// one built by `federation::build_mtls_client`.
    ///
    /// The request and stream idle timeouts still apply; connection settings
    /// such as the connect timeout come from `http`.
    pub fn from_http_client(http: reqwest::Client) -> Self {
        Self {
            http,
            bearer_token: None,
            request_timeout: Self::DEFAULT_REQUEST_TIMEOUT,
            stream_idle_timeout: Self::DEFAULT_STREAM_IDLE_TIMEOUT,
        }
    }

    /// Set the limit for a whole unary request, and for the response headers
    /// of a streaming request.
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Set the longest gap allowed between bytes on an open stream.
    pub fn with_stream_idle_timeout(mut self, timeout: Duration) -> Self {
        self.stream_idle_timeout = timeout;
        self
    }

    /// Attach a bearer token for authenticated requests.
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.bearer_token = Some(token.into());
        self
    }

    // ────────────────────────────────────────────────────────────────────────
    // Private helpers
    // ────────────────────────────────────────────────────────────────────────

    /// Build a request builder with optional bearer auth.
    fn authed_request(&self, method: reqwest::Method, url: &str) -> reqwest::RequestBuilder {
        let mut builder = self.http.request(method, url);
        if let Some(ref token) = self.bearer_token {
            builder = builder.bearer_auth(token);
        }
        builder
    }

    /// Map a reqwest error, reporting timeouts as [`A2aTransportError::Timeout`].
    fn transport_error(&self, url: &str, e: reqwest::Error) -> A2aError {
        if e.is_timeout() {
            A2aTransportError::Timeout {
                url: url.to_string(),
                duration: self.request_timeout,
            }
            .into()
        } else {
            A2aTransportError::Connection {
                url: url.to_string(),
                source: Box::new(e),
            }
            .into()
        }
    }

    /// Send a streaming request, waiting at most the request timeout for the
    /// response headers.
    async fn send_for_stream(
        &self,
        url: &str,
        builder: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, A2aError> {
        match tokio::time::timeout(self.request_timeout, builder.send()).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(e)) => Err(self.transport_error(url, e)),
            Err(_) => Err(A2aTransportError::Timeout {
                url: url.to_string(),
                duration: self.request_timeout,
            }
            .into()),
        }
    }

    /// Map a JSON-RPC error code to the corresponding [`A2aProtocolError`].
    fn protocol_error_from_json_rpc(err: &JsonRpcError) -> A2aProtocolError {
        match err.code {
            -32001 => A2aProtocolError::TaskNotFound {
                task_id: err
                    .data
                    .as_ref()
                    .and_then(|d| d.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
            },
            -32002 => A2aProtocolError::TaskNotCancelable {
                task_id: err
                    .data
                    .as_ref()
                    .and_then(|d| d.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
            },
            -32003 => A2aProtocolError::PushNotificationNotSupported,
            -32004 => A2aProtocolError::UnsupportedOperation {
                method: err.message.clone(),
            },
            -32005 => A2aProtocolError::ContentTypeNotSupported {
                media_type: err
                    .data
                    .as_ref()
                    .and_then(|d| d.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
            },
            -32006 => A2aProtocolError::InvalidAgentResponse {
                details: err.message.clone(),
            },
            -32007 => A2aProtocolError::ExtendedCardNotConfigured,
            -32008 => A2aProtocolError::ExtensionRequired {
                uri: err
                    .data
                    .as_ref()
                    .and_then(|d| d.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
            },
            -32009 => A2aProtocolError::VersionNotSupported {
                version: err
                    .data
                    .as_ref()
                    .and_then(|d| d.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
            },
            _ => A2aProtocolError::InvalidAgentResponse {
                details: format!("JSON-RPC error {}: {}", err.code, err.message),
            },
        }
    }

    /// Send a JSON-RPC 2.0 request and parse the result.
    async fn rpc_call<P: serde::Serialize, R: serde::de::DeserializeOwned>(
        &self,
        base_url: &str,
        method: &str,
        params: P,
    ) -> Result<R, A2aError> {
        let url = format!("{}/", base_url.trim_end_matches('/'));
        let rpc_request = JsonRpcRequest::new(method, params);
        debug!(method, url = %url, "sending JSON-RPC request");

        let response = self
            .authed_request(reqwest::Method::POST, &url)
            .timeout(self.request_timeout)
            .json(&rpc_request)
            .send()
            .await
            .map_err(|e| self.transport_error(&url, e))?;

        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(A2aError::Auth {
                reason: format!("HTTP {status}"),
            });
        }

        let body = response.text().await.map_err(|e| {
            if e.is_timeout() {
                self.transport_error(&url, e)
            } else {
                A2aTransportError::InvalidResponse {
                    details: format!("failed to read response body: {e}"),
                }
                .into()
            }
        })?;

        let rpc_response: JsonRpcResponse<R> =
            serde_json::from_str(&body).map_err(|e| A2aTransportError::InvalidResponse {
                details: format!("failed to parse JSON-RPC response: {e}"),
            })?;

        if let Some(ref err) = rpc_response.error {
            return Err(Self::protocol_error_from_json_rpc(err).into());
        }

        rpc_response.result.ok_or_else(|| {
            A2aTransportError::InvalidResponse {
                details: "JSON-RPC response has neither result nor error".into(),
            }
            .into()
        })
    }

    // ────────────────────────────────────────────────────────────────────────
    // Agent Card discovery
    // ────────────────────────────────────────────────────────────────────────

    /// Discover an agent's public card via the well-known endpoint.
    ///
    /// `GET {base_url}/.well-known/agent-card.json`
    pub async fn discover(&self, base_url: &str) -> Result<AgentCard, A2aError> {
        let url = format!(
            "{}/.well-known/agent-card.json",
            base_url.trim_end_matches('/')
        );
        info!(url = %url, "discovering agent card");

        let response = self
            .authed_request(reqwest::Method::GET, &url)
            .timeout(self.request_timeout)
            .send()
            .await
            .map_err(|e| self.transport_error(&url, e))?;

        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(A2aError::Auth {
                reason: format!("HTTP {status}"),
            });
        }

        if !status.is_success() {
            return Err(A2aTransportError::InvalidResponse {
                details: format!("agent card discovery returned HTTP {status}"),
            }
            .into());
        }

        response.json::<AgentCard>().await.map_err(|e| {
            if e.is_timeout() {
                self.transport_error(&url, e)
            } else {
                A2aTransportError::InvalidResponse {
                    details: format!("failed to parse agent card: {e}"),
                }
                .into()
            }
        })
    }

    /// Retrieve the extended (authenticated) Agent Card via JSON-RPC.
    pub async fn get_extended_card(&self, base_url: &str) -> Result<AgentCard, A2aError> {
        self.rpc_call(
            base_url,
            "GetExtendedAgentCard",
            GetExtendedAgentCardRequest { tenant: None },
        )
        .await
    }

    // ────────────────────────────────────────────────────────────────────────
    // Core task operations
    // ────────────────────────────────────────────────────────────────────────

    /// Send a message to an agent (synchronous request/response).
    pub async fn send_message(
        &self,
        base_url: &str,
        req: SendMessageRequest,
    ) -> Result<SendMessageResponse, A2aError> {
        self.rpc_call(base_url, "SendMessage", req).await
    }

    /// Send a message and receive a streaming SSE response.
    pub async fn send_streaming(
        &self,
        base_url: &str,
        req: SendMessageRequest,
    ) -> Result<impl futures::Stream<Item = Result<StreamResponse, A2aError>>, A2aError> {
        let url = format!("{}/", base_url.trim_end_matches('/'));
        let rpc_request = JsonRpcRequest::new("SendStreamingMessage", req);
        debug!(url = %url, "sending streaming message");

        let builder = self
            .authed_request(reqwest::Method::POST, &url)
            .json(&rpc_request);
        let response = self.send_for_stream(&url, builder).await?;

        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(A2aError::Auth {
                reason: format!("HTTP {status}"),
            });
        }

        if !status.is_success() {
            return Err(A2aTransportError::InvalidResponse {
                details: format!("streaming request returned HTTP {status}"),
            }
            .into());
        }

        Ok(parse_sse_stream(response, url, self.stream_idle_timeout))
    }

    /// Retrieve a task by ID.
    pub async fn get_task(&self, base_url: &str, req: GetTaskRequest) -> Result<Task, A2aError> {
        self.rpc_call(base_url, "GetTask", req).await
    }

    /// List tasks with optional filters and pagination.
    pub async fn list_tasks(
        &self,
        base_url: &str,
        req: ListTasksRequest,
    ) -> Result<ListTasksResponse, A2aError> {
        self.rpc_call(base_url, "ListTasks", req).await
    }

    /// Cancel a running task.
    pub async fn cancel_task(
        &self,
        base_url: &str,
        req: CancelTaskRequest,
    ) -> Result<Task, A2aError> {
        self.rpc_call(base_url, "CancelTask", req).await
    }

    /// Subscribe to streaming updates for an existing task.
    pub async fn subscribe(
        &self,
        base_url: &str,
        task_id: &str,
    ) -> Result<impl futures::Stream<Item = Result<StreamResponse, A2aError>>, A2aError> {
        let url = format!("{}/", base_url.trim_end_matches('/'));
        let rpc_request = JsonRpcRequest::new(
            "SubscribeToTask",
            SubscribeToTaskRequest {
                tenant: None,
                id: task_id.to_string(),
            },
        );
        debug!(url = %url, task_id, "subscribing to task");

        let builder = self
            .authed_request(reqwest::Method::POST, &url)
            .json(&rpc_request);
        let response = self.send_for_stream(&url, builder).await?;

        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(A2aError::Auth {
                reason: format!("HTTP {status}"),
            });
        }

        if !status.is_success() {
            return Err(A2aTransportError::InvalidResponse {
                details: format!("subscribe request returned HTTP {status}"),
            }
            .into());
        }

        Ok(parse_sse_stream(response, url, self.stream_idle_timeout))
    }

    // ────────────────────────────────────────────────────────────────────────
    // Push notification config CRUD
    // ────────────────────────────────────────────────────────────────────────

    /// Create a push notification configuration for a task.
    pub async fn create_push_config(
        &self,
        base_url: &str,
        req: CreateTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2aError> {
        self.rpc_call(base_url, "CreateTaskPushNotificationConfig", req)
            .await
    }

    /// Retrieve a specific push notification configuration.
    pub async fn get_push_config(
        &self,
        base_url: &str,
        req: GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2aError> {
        self.rpc_call(base_url, "GetTaskPushNotificationConfig", req)
            .await
    }

    /// List push notification configurations for a task.
    pub async fn list_push_configs(
        &self,
        base_url: &str,
        req: ListTaskPushNotificationConfigsRequest,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2aError> {
        self.rpc_call(base_url, "ListTaskPushNotificationConfigs", req)
            .await
    }

    /// Delete a push notification configuration.
    pub async fn delete_push_config(
        &self,
        base_url: &str,
        req: DeleteTaskPushNotificationConfigRequest,
    ) -> Result<(), A2aError> {
        // JSON-RPC returns null/empty for delete; we map to unit.
        let _: serde_json::Value = self
            .rpc_call(base_url, "DeleteTaskPushNotificationConfig", req)
            .await?;
        Ok(())
    }

    // ────────────────────────────────────────────────────────────────────────
    // Convenience: wait for terminal state
    // ────────────────────────────────────────────────────────────────────────

    /// Poll `GetTask` until the task reaches a terminal or interrupted state.
    ///
    /// Terminal states: `Completed`, `Failed`, `Canceled`, `Rejected`.
    /// Interrupted states: `InputRequired`, `AuthRequired`.
    ///
    /// Returns the final task snapshot, or an error if `max_duration` is exceeded.
    pub async fn wait_for_completion(
        &self,
        base_url: &str,
        task_id: &str,
        interval: Duration,
        max_duration: Option<Duration>,
    ) -> Result<Task, A2aError> {
        let start = tokio::time::Instant::now();
        info!(
            task_id,
            ?interval,
            ?max_duration,
            "waiting for task completion"
        );

        loop {
            let task = self
                .get_task(
                    base_url,
                    GetTaskRequest {
                        tenant: None,
                        id: task_id.to_string(),
                        history_length: None,
                    },
                )
                .await?;

            match task.status.state {
                TaskState::Completed
                | TaskState::Failed
                | TaskState::Canceled
                | TaskState::Rejected
                | TaskState::InputRequired
                | TaskState::AuthRequired => {
                    debug!(task_id, state = ?task.status.state, "task reached terminal state");
                    return Ok(task);
                }
                _ => {}
            }

            if let Some(max) = max_duration {
                if start.elapsed() >= max {
                    return Err(A2aTransportError::Timeout {
                        url: base_url.to_string(),
                        duration: max,
                    }
                    .into());
                }
            }

            tokio::time::sleep(interval).await;
        }
    }
}

impl Default for A2aClient {
    fn default() -> Self {
        Self::new()
    }
}

// ────────────────────────────────────────────────────────────────────────────
// SSE stream parser
// ────────────────────────────────────────────────────────────────────────────

/// Largest single SSE event the client buffers before giving up on the
/// stream, so a server cannot grow client memory without bound.
const MAX_SSE_EVENT_BYTES: usize = 16 * 1024 * 1024;

/// Parse an SSE byte stream into a stream of [`StreamResponse`] events.
///
/// TCP chunks are NOT aligned to SSE event boundaries, so we buffer bytes
/// across chunks and split on the `\n\n` delimiter before decoding. The
/// stream ends with [`A2aTransportError::Timeout`] if no bytes arrive for
/// `idle_timeout`, and with an error if one event grows past
/// `MAX_SSE_EVENT_BYTES`.
fn parse_sse_stream(
    response: reqwest::Response,
    url: String,
    idle_timeout: Duration,
) -> impl futures::Stream<Item = Result<StreamResponse, A2aError>> + Send + Unpin {
    use futures::StreamExt;
    use std::collections::VecDeque;

    struct SseState<S> {
        body: S,
        buf: Vec<u8>,
        /// Bytes of `buf` already searched for a delimiter.
        scanned: usize,
        pending: VecDeque<Result<StreamResponse, A2aError>>,
        done: bool,
        url: String,
        idle_timeout: Duration,
    }

    fn drain_events<S>(st: &mut SseState<S>) {
        loop {
            // Resume the search one byte back, in case a chunk ended between
            // the two newlines.
            let from = st.scanned.saturating_sub(1);
            let Some(rel) = st.buf[from..].windows(2).position(|w| w == b"\n\n") else {
                st.scanned = st.buf.len();
                return;
            };
            let pos = from + rel;
            let block: Vec<u8> = st.buf.drain(..pos + 2).collect();
            st.scanned = 0;
            let text = String::from_utf8_lossy(&block[..pos]);
            for line in text.lines() {
                if let Some(data) = line.strip_prefix("data: ") {
                    st.pending
                        .push_back(serde_json::from_str::<StreamResponse>(data).map_err(|e| {
                            A2aTransportError::InvalidResponse {
                                details: format!("SSE parse error: {e}"),
                            }
                            .into()
                        }));
                }
            }
        }
    }

    let init = SseState {
        body: response.bytes_stream().boxed(),
        buf: Vec::new(),
        scanned: 0,
        pending: VecDeque::new(),
        done: false,
        url,
        idle_timeout,
    };

    futures::stream::unfold(init, |mut st| async move {
        loop {
            if let Some(item) = st.pending.pop_front() {
                return Some((item, st));
            }
            if st.done {
                return None;
            }
            match tokio::time::timeout(st.idle_timeout, st.body.next()).await {
                Err(_) => {
                    st.done = true;
                    st.pending.push_back(Err(A2aTransportError::Timeout {
                        url: st.url.clone(),
                        duration: st.idle_timeout,
                    }
                    .into()));
                }
                Ok(None) => st.done = true,
                Ok(Some(Err(e))) => {
                    st.done = true;
                    st.pending.push_back(Err(A2aTransportError::SseError {
                        details: e.to_string(),
                    }
                    .into()));
                }
                Ok(Some(Ok(chunk))) => {
                    st.buf.extend_from_slice(&chunk);
                    drain_events(&mut st);
                    if st.buf.len() > MAX_SSE_EVENT_BYTES {
                        st.done = true;
                        st.pending
                            .push_back(Err(A2aTransportError::InvalidResponse {
                                details: format!(
                                    "SSE event larger than {MAX_SSE_EVENT_BYTES} bytes"
                                ),
                            }
                            .into()));
                    }
                }
            }
        }
    })
    .boxed()
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_is_clone() {
        let client = A2aClient::new();
        let _clone = client.clone();
    }

    #[test]
    fn client_with_token() {
        let client = A2aClient::new().with_token("test-token");
        assert!(client.bearer_token.is_some());
        assert_eq!(client.bearer_token.as_deref(), Some("test-token"));
    }

    #[test]
    fn client_default() {
        let client = A2aClient::default();
        assert!(client.bearer_token.is_none());
    }

    #[test]
    fn client_has_bounded_timeouts_by_default() {
        let client = A2aClient::new();
        assert_eq!(client.request_timeout, A2aClient::DEFAULT_REQUEST_TIMEOUT);
        assert_eq!(
            client.stream_idle_timeout,
            A2aClient::DEFAULT_STREAM_IDLE_TIMEOUT
        );
        let client = client
            .with_request_timeout(Duration::from_secs(5))
            .with_stream_idle_timeout(Duration::from_secs(7));
        assert_eq!(client.request_timeout, Duration::from_secs(5));
        assert_eq!(client.stream_idle_timeout, Duration::from_secs(7));
    }

    #[test]
    fn protocol_error_mapping_task_not_found() {
        let err = JsonRpcError {
            code: -32001,
            message: "Task not found".into(),
            data: Some(serde_json::json!("task-42")),
        };
        let proto = A2aClient::protocol_error_from_json_rpc(&err);
        match proto {
            A2aProtocolError::TaskNotFound { task_id } => assert_eq!(task_id, "task-42"),
            other => panic!("expected TaskNotFound, got {other:?}"),
        }
    }

    #[test]
    fn protocol_error_mapping_not_cancelable() {
        let err = JsonRpcError {
            code: -32002,
            message: "Task not cancelable".into(),
            data: Some(serde_json::json!("task-99")),
        };
        let proto = A2aClient::protocol_error_from_json_rpc(&err);
        match proto {
            A2aProtocolError::TaskNotCancelable { task_id } => assert_eq!(task_id, "task-99"),
            other => panic!("expected TaskNotCancelable, got {other:?}"),
        }
    }

    #[test]
    fn protocol_error_mapping_push_not_supported() {
        let err = JsonRpcError {
            code: -32003,
            message: "Push not supported".into(),
            data: None,
        };
        let proto = A2aClient::protocol_error_from_json_rpc(&err);
        assert!(matches!(
            proto,
            A2aProtocolError::PushNotificationNotSupported
        ));
    }

    #[test]
    fn protocol_error_mapping_unsupported_operation() {
        let err = JsonRpcError {
            code: -32004,
            message: "ListTasks".into(),
            data: None,
        };
        let proto = A2aClient::protocol_error_from_json_rpc(&err);
        match proto {
            A2aProtocolError::UnsupportedOperation { method } => assert_eq!(method, "ListTasks"),
            other => panic!("expected UnsupportedOperation, got {other:?}"),
        }
    }

    #[test]
    fn protocol_error_mapping_extended_card_not_configured() {
        let err = JsonRpcError {
            code: -32007,
            message: "No extended card".into(),
            data: None,
        };
        let proto = A2aClient::protocol_error_from_json_rpc(&err);
        assert!(matches!(proto, A2aProtocolError::ExtendedCardNotConfigured));
    }

    #[test]
    fn protocol_error_mapping_version_not_supported() {
        let err = JsonRpcError {
            code: -32009,
            message: "Version mismatch".into(),
            data: Some(serde_json::json!("2.0")),
        };
        let proto = A2aClient::protocol_error_from_json_rpc(&err);
        match proto {
            A2aProtocolError::VersionNotSupported { version } => assert_eq!(version, "2.0"),
            other => panic!("expected VersionNotSupported, got {other:?}"),
        }
    }

    #[test]
    fn protocol_error_mapping_unknown_code() {
        let err = JsonRpcError {
            code: -32000,
            message: "Something unknown".into(),
            data: None,
        };
        let proto = A2aClient::protocol_error_from_json_rpc(&err);
        assert!(matches!(
            proto,
            A2aProtocolError::InvalidAgentResponse { .. }
        ));
    }
}
