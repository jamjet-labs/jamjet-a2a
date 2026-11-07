//! Shared helpers for the integration tests.

#![allow(dead_code)]

use jamjet_a2a::server::{A2aServer, TaskHandler};
use jamjet_a2a::store::TaskStore;
use jamjet_a2a_types::*;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

pub fn agent_card() -> AgentCard {
    AgentCard {
        name: "test-agent".into(),
        description: "Test agent".into(),
        version: "1.0".into(),
        supported_interfaces: vec![],
        capabilities: AgentCapabilities {
            streaming: Some(true),
            push_notifications: None,
            extensions: vec![],
            extended_agent_card: None,
        },
        default_input_modes: vec!["text/plain".into()],
        default_output_modes: vec!["text/plain".into()],
        skills: vec![],
        provider: None,
        security_schemes: Default::default(),
        security_requirements: vec![],
        signatures: vec![],
        icon_url: None,
    }
}

/// Completes every task straight away.
pub struct CompleteHandler;

#[async_trait::async_trait]
impl TaskHandler for CompleteHandler {
    async fn handle(
        &self,
        task_id: String,
        _message: Message,
        store: Arc<dyn TaskStore>,
    ) -> Result<(), A2aError> {
        store
            .update_status(
                &task_id,
                TaskStatus {
                    state: TaskState::Completed,
                    message: None,
                    timestamp: Some(chrono::Utc::now().to_rfc3339()),
                },
            )
            .await
    }
}

/// Never finishes, so tasks stay in `Working`.
pub struct HoldHandler;

#[async_trait::async_trait]
impl TaskHandler for HoldHandler {
    async fn handle(
        &self,
        _task_id: String,
        _message: Message,
        _store: Arc<dyn TaskStore>,
    ) -> Result<(), A2aError> {
        std::future::pending::<()>().await;
        Ok(())
    }
}

/// Serve a router on a random loopback port and return its base URL.
pub async fn serve_router(router: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    base_url
}

pub async fn serve(server: A2aServer) -> String {
    serve_router(server.into_router()).await
}

/// JSON for a user message.
pub fn message(id: &str, text: &str, task_id: Option<&str>, context_id: Option<&str>) -> Value {
    let mut m = json!({
        "messageId": id,
        "role": "ROLE_USER",
        "parts": [{"text": text}],
    });
    if let Some(t) = task_id {
        m["taskId"] = json!(t);
    }
    if let Some(c) = context_id {
        m["contextId"] = json!(c);
    }
    m
}

/// POST a JSON-RPC request and return the HTTP status, headers and body.
pub async fn rpc_raw(
    base_url: &str,
    token: Option<&str>,
    method: &str,
    params: Value,
) -> (u16, reqwest::header::HeaderMap, String) {
    let mut req = reqwest::Client::new()
        .post(format!("{base_url}/"))
        .timeout(Duration::from_secs(5))
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}));
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await.expect("request failed");
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    // Streams are not read to the end; the first event is enough here.
    let body = if content_type.starts_with("text/event-stream") {
        String::from("<stream>")
    } else {
        resp.text().await.expect("body")
    };
    (status, headers, body)
}

/// POST a JSON-RPC request and return the HTTP status and parsed JSON body.
/// SSE responses come back as `{"stream": true}`.
pub async fn rpc(base_url: &str, token: Option<&str>, method: &str, params: Value) -> (u16, Value) {
    let (status, _, body) = rpc_raw(base_url, token, method, params).await;
    if body == "<stream>" {
        return (status, json!({"stream": true}));
    }
    (status, serde_json::from_str(&body).expect("JSON body"))
}

/// The JSON-RPC error code in a response body, if any.
pub fn error_code(body: &Value) -> Option<i64> {
    body.get("error")?.get("code")?.as_i64()
}

/// Assert an auth error code. It must not be a code that JSON-RPC or A2A
/// reserves: the old federation layer sent -32003, which clients read as
/// PushNotificationNotSupportedError.
pub fn assert_auth_code(body: &Value, expected: i32) {
    let code = error_code(body).unwrap_or_else(|| panic!("no error code in {body}"));
    assert_eq!(code, i64::from(expected), "{body}");
    assert!(
        !(-32768..=-32000).contains(&code),
        "reserved code {code} in {body}"
    );
}

/// Create a task with SendMessage and return its id.
pub async fn create_task(base_url: &str, token: Option<&str>, context_id: Option<&str>) -> String {
    let (status, body) = rpc(
        base_url,
        token,
        "SendMessage",
        json!({"message": message("m-1", "first", None, context_id)}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    body["result"]["task"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("no task id in {body}"))
        .to_string()
}

/// Poll GetTask until the task is in `state` (as serialized on the wire).
pub async fn wait_for_state(base_url: &str, token: Option<&str>, task_id: &str, state: &str) {
    for _ in 0..100 {
        let (_, body) = rpc(base_url, token, "GetTask", json!({"id": task_id})).await;
        if body["result"]["status"]["state"] == state {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("task {task_id} never reached {state}");
}

/// Send raw bytes on a fresh TCP connection and return what the server
/// sends back within `wait` (or until it closes the connection).
pub async fn raw_exchange(base_url: &str, request: &[u8], wait: Duration) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let addr = base_url.trim_start_matches("http://");
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    stream.write_all(request).await.expect("write");
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        match tokio::time::timeout_at(deadline, stream.read(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
            Ok(Ok(n)) => {
                out.extend_from_slice(&buf[..n]);
                if out.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
