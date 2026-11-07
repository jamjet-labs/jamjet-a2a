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

    /// Read a response body, refusing more than `MAX_RESPONSE_BYTES`.
    async fn read_body(
        &self,
        url: &str,
        mut response: reqwest::Response,
    ) -> Result<Vec<u8>, A2aError> {
        let too_big = || -> A2aError {
            A2aTransportError::InvalidResponse {
                details: format!("response body larger than {MAX_RESPONSE_BYTES} bytes"),
            }
            .into()
        };
        if response
            .content_length()
            .is_some_and(|n| n > MAX_RESPONSE_BYTES as u64)
        {
            return Err(too_big());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| {
            if e.is_timeout() {
                self.transport_error(url, e)
            } else {
                A2aTransportError::InvalidResponse {
                    details: format!("failed to read response body: {e}"),
                }
                .into()
            }
        })? {
            if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(too_big());
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    /// Send a streaming request and return its event stream.
    ///
    /// A server answers errors to a streaming call with a plain JSON-RPC
    /// error (`application/json`) instead of an event stream; those are
    /// returned as errors here, not as an empty stream.
    async fn open_stream(
        &self,
        url: String,
        builder: reqwest::RequestBuilder,
    ) -> Result<impl futures::Stream<Item = Result<StreamResponse, A2aError>>, A2aError> {
        let response = self.send_for_stream(&url, builder).await?;

        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(A2aError::Auth {
                reason: format!("HTTP {status}"),
            });
        }

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();

        if content_type.starts_with("application/json") {
            let body =
                match tokio::time::timeout(self.request_timeout, self.read_body(&url, response))
                    .await
                {
                    Ok(body) => body?,
                    Err(_) => {
                        return Err(A2aTransportError::Timeout {
                            url,
                            duration: self.request_timeout,
                        }
                        .into())
                    }
                };
            let rpc: JsonRpcResponse<serde_json::Value> =
                serde_json::from_slice(&body).map_err(|e| A2aTransportError::InvalidResponse {
                    details: format!("HTTP {status}: failed to parse JSON-RPC response: {e}"),
                })?;
            return Err(match rpc.error {
                Some(err) => Self::protocol_error_from_json_rpc(&err).into(),
                None => A2aTransportError::InvalidResponse {
                    details: "expected an event stream, got a JSON-RPC result".into(),
                }
                .into(),
            });
        }

        if !status.is_success() {
            return Err(A2aTransportError::InvalidResponse {
                details: format!("streaming request returned HTTP {status}"),
            }
            .into());
        }
        if !content_type.starts_with("text/event-stream") {
            return Err(A2aTransportError::InvalidResponse {
                details: format!("expected text/event-stream, got {content_type:?}"),
            }
            .into());
        }

        Ok(parse_sse_stream(response, url, self.stream_idle_timeout))
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

        let body = self.read_body(&url, response).await?;

        let rpc_response: JsonRpcResponse<R> =
            serde_json::from_slice(&body).map_err(|e| A2aTransportError::InvalidResponse {
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

        let body = self.read_body(&url, response).await?;
        serde_json::from_slice::<AgentCard>(&body).map_err(|e| {
            A2aTransportError::InvalidResponse {
                details: format!("failed to parse agent card: {e}"),
            }
            .into()
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
        self.open_stream(url, builder).await
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
        self.open_stream(url, builder).await
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

/// Largest unary response body the client reads.
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// Turn the `data` of one SSE event into a [`StreamResponse`].
///
/// Accepts a bare StreamResponse (what this crate's server sends) and the
/// A2A v1.0 JSON-RPC form, `{"jsonrpc":"2.0","result":...}`, with the result
/// either bare or wrapped as `{"task":...}`, `{"statusUpdate":...}` and so
/// on. A JSON-RPC error event becomes the matching protocol error.
fn parse_sse_data(data: &str) -> Result<StreamResponse, A2aError> {
    let invalid =
        |details: String| -> A2aError { A2aTransportError::InvalidResponse { details }.into() };
    let value: serde_json::Value =
        serde_json::from_str(data).map_err(|e| invalid(format!("SSE parse error: {e}")))?;
    let value = if value.get("jsonrpc").is_some() {
        let rpc: JsonRpcResponse<serde_json::Value> =
            serde_json::from_value(value).map_err(|e| invalid(format!("SSE parse error: {e}")))?;
        if let Some(err) = rpc.error {
            return Err(A2aClient::protocol_error_from_json_rpc(&err).into());
        }
        let result = rpc
            .result
            .ok_or_else(|| invalid("SSE event has neither result nor error".into()))?;
        match result.as_object() {
            Some(obj) if obj.len() == 1 => {
                let (key, inner) = obj.iter().next().expect("one entry");
                match key.as_str() {
                    "task" | "message" | "statusUpdate" | "artifactUpdate" => inner.clone(),
                    _ => result,
                }
            }
            _ => result,
        }
    } else {
        value
    };
    serde_json::from_value(value).map_err(|e| invalid(format!("SSE parse error: {e}")))
}

/// Parse an SSE byte stream into a stream of [`StreamResponse`] events.
///
/// Follows the SSE line format: lines end in LF, CRLF or CR; `data` lines
/// (with or without a space after the colon) are joined with newlines; a
/// blank line ends an event; comments and other fields are ignored. Each
/// byte is scanned once. The stream ends with an error if no bytes arrive
/// for `idle_timeout`, if one event grows past `MAX_SSE_EVENT_BYTES`, or if
/// the body ends in the middle of an event.
fn parse_sse_stream(
    response: reqwest::Response,
    url: String,
    idle_timeout: Duration,
) -> impl futures::Stream<Item = Result<StreamResponse, A2aError>> + Send + Unpin {
    use futures::StreamExt;
    use std::collections::VecDeque;

    struct SseState<S> {
        body: S,
        /// Bytes of the current, unfinished line.
        line: Vec<u8>,
        /// The last byte seen was CR, so a following LF belongs to it.
        after_cr: bool,
        /// `data` of the current event so far.
        data: String,
        has_data: bool,
        pending: VecDeque<Result<StreamResponse, A2aError>>,
        done: bool,
        url: String,
        idle_timeout: Duration,
    }

    fn end_line<S>(st: &mut SseState<S>) {
        let line = String::from_utf8_lossy(&st.line).into_owned();
        st.line.clear();
        if line.is_empty() {
            if st.has_data {
                let data = std::mem::take(&mut st.data);
                st.has_data = false;
                st.pending.push_back(parse_sse_data(&data));
            }
            return;
        }
        if line.starts_with(':') {
            return;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line.as_str(), ""),
        };
        if field == "data" {
            if st.has_data {
                st.data.push('\n');
            }
            st.data.push_str(value);
            st.has_data = true;
        }
    }

    fn feed<S>(st: &mut SseState<S>, chunk: &[u8]) {
        for &b in chunk {
            if st.after_cr {
                st.after_cr = false;
                if b == b'\n' {
                    continue;
                }
            }
            match b {
                b'\n' => end_line(st),
                b'\r' => {
                    end_line(st);
                    st.after_cr = true;
                }
                _ => st.line.push(b),
            }
        }
    }

    let init = SseState {
        body: response.bytes_stream().boxed(),
        line: Vec::new(),
        after_cr: false,
        data: String::new(),
        has_data: false,
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
                Ok(None) => {
                    st.done = true;
                    let partial = !st.line.iter().all(u8::is_ascii_whitespace);
                    if st.has_data || partial {
                        st.pending
                            .push_back(Err(A2aTransportError::InvalidResponse {
                                details: "stream ended in the middle of an event".into(),
                            }
                            .into()));
                    }
                }
                Ok(Some(Err(e))) => {
                    st.done = true;
                    st.pending.push_back(Err(A2aTransportError::SseError {
                        details: e.to_string(),
                    }
                    .into()));
                }
                Ok(Some(Ok(chunk))) => {
                    feed(&mut st, &chunk);
                    if st.line.len() + st.data.len() > MAX_SSE_EVENT_BYTES {
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
