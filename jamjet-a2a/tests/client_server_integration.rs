//! Integration test: start a real A2A server and exercise it with the client.

use jamjet_a2a::client::A2aClient;
use jamjet_a2a::server::{A2aServer, TaskHandler};
use jamjet_a2a::store::TaskStore;
use jamjet_a2a_types::*;
use std::sync::Arc;
use std::time::Duration;

// ────────────────────────────────────────────────────────────────────────────
// EchoHandler — echoes message parts back as an artifact, then completes.
// ────────────────────────────────────────────────────────────────────────────

struct EchoHandler;

#[async_trait::async_trait]
impl TaskHandler for EchoHandler {
    async fn handle(
        &self,
        task_id: String,
        message: Message,
        store: Arc<dyn TaskStore>,
    ) -> Result<(), A2aError> {
        // Add the incoming parts as an artifact.
        store
            .add_artifact(
                &task_id,
                Artifact {
                    artifact_id: "echo-1".into(),
                    name: Some("echo".into()),
                    description: None,
                    parts: message.parts,
                    metadata: None,
                    extensions: vec![],
                },
            )
            .await?;

        // Mark completed.
        store
            .update_status(
                &task_id,
                TaskStatus {
                    state: TaskState::Completed,
                    message: None,
                    timestamp: Some(chrono::Utc::now().to_rfc3339()),
                },
            )
            .await?;

        Ok(())
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Helpers
// ────────────────────────────────────────────────────────────────────────────

fn test_agent_card() -> AgentCard {
    AgentCard {
        name: "echo-agent".into(),
        description: "Echoes input back".into(),
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
        skills: vec![AgentSkill {
            id: "echo".into(),
            name: "echo".into(),
            description: "Echo skill".into(),
            ..Default::default()
        }],
        provider: None,
        security_schemes: Default::default(),
        security_requirements: vec![],
        signatures: vec![],
        icon_url: None,
    }
}

/// Start the server on a random port and return the base URL.
async fn start_server() -> String {
    let card = test_agent_card();
    let router = A2aServer::new(card).with_handler(EchoHandler).into_router();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind to random port");
    let port = listener.local_addr().unwrap().port();
    let base_url = format!("http://127.0.0.1:{port}");

    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    // Give the server a moment to start accepting connections.
    tokio::time::sleep(Duration::from_millis(50)).await;

    base_url
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn full_lifecycle() {
    let base_url = start_server().await;
    let client = A2aClient::new();

    // 1. Discover the agent card.
    let card = client
        .discover(&base_url)
        .await
        .expect("agent card discovery failed");
    assert_eq!(card.name, "echo-agent");
    assert_eq!(card.skills.len(), 1);
    assert_eq!(card.skills[0].id, "echo");

    // 2. Send a message.
    let send_req = SendMessageRequest {
        tenant: None,
        message: Message {
            message_id: "msg-1".into(),
            context_id: Some("ctx-1".into()),
            task_id: None, // server assigns an ID
            role: Role::User,
            parts: vec![Part {
                content: PartContent::Text("Hello, echo!".into()),
                metadata: None,
                filename: None,
                media_type: None,
            }],
            metadata: None,
            extensions: vec![],
            reference_task_ids: vec![],
        },
        configuration: None,
        metadata: None,
    };

    let send_resp = client
        .send_message(&base_url, send_req)
        .await
        .expect("send_message failed");

    // Extract the task from the response.
    let task = send_resp
        .into_task()
        .expect("expected Task response from SendMessage");
    let task_id = task.id.clone();
    assert!(!task_id.is_empty());

    // 3. Wait for the handler to finish (short timeout).
    let completed_task = client
        .wait_for_completion(
            &base_url,
            &task_id,
            Duration::from_millis(100),
            Some(Duration::from_secs(5)),
        )
        .await
        .expect("wait_for_completion failed");

    assert_eq!(completed_task.status.state, TaskState::Completed);
    assert_eq!(completed_task.artifacts.len(), 1);
    assert_eq!(completed_task.artifacts[0].artifact_id, "echo-1");
    assert_eq!(completed_task.artifacts[0].parts.len(), 1);
    match &completed_task.artifacts[0].parts[0].content {
        PartContent::Text(text) => assert_eq!(text, "Hello, echo!"),
        other => panic!("expected Text part, got: {other:?}"),
    }

    // 4. Get the task directly and verify.
    let fetched = client
        .get_task(
            &base_url,
            GetTaskRequest {
                tenant: None,
                id: task_id.clone(),
                history_length: None,
            },
        )
        .await
        .expect("get_task failed");
    assert_eq!(fetched.id, task_id);
    assert_eq!(fetched.status.state, TaskState::Completed);

    // 5. List tasks and verify count.
    let list_resp = client
        .list_tasks(&base_url, ListTasksRequest::default())
        .await
        .expect("list_tasks failed");
    assert_eq!(list_resp.tasks.len(), 1);
    assert_eq!(list_resp.total_size, 1);
    assert_eq!(list_resp.tasks[0].id, task_id);

    // 6. Cancel the completed task — should fail.
    let cancel_result = client
        .cancel_task(
            &base_url,
            CancelTaskRequest {
                tenant: None,
                id: task_id.clone(),
                metadata: None,
            },
        )
        .await;
    assert!(
        cancel_result.is_err(),
        "canceling a completed task should fail"
    );
    let err = cancel_result.unwrap_err();
    assert!(
        matches!(
            err,
            A2aError::Protocol(A2aProtocolError::TaskNotCancelable { .. })
        ),
        "expected TaskNotCancelable, got: {err:?}"
    );
}

#[tokio::test]
async fn get_nonexistent_task_returns_error() {
    let base_url = start_server().await;
    let client = A2aClient::new();

    let result = client
        .get_task(
            &base_url,
            GetTaskRequest {
                tenant: None,
                id: "nonexistent-task-id".into(),
                history_length: None,
            },
        )
        .await;

    assert!(result.is_err(), "fetching a nonexistent task should fail");
    let err = result.unwrap_err();
    assert!(
        matches!(
            err,
            A2aError::Protocol(A2aProtocolError::TaskNotFound { .. })
        ),
        "expected TaskNotFound, got: {err:?}"
    );
}

#[tokio::test]
async fn multiple_tasks_listed_correctly() {
    let base_url = start_server().await;
    let client = A2aClient::new();

    // Send three messages to create three separate tasks.
    let mut task_ids = Vec::new();
    for i in 0..3 {
        let send_req = SendMessageRequest {
            tenant: None,
            message: Message {
                message_id: format!("msg-{i}"),
                context_id: Some("ctx-multi".into()),
                task_id: None,
                role: Role::User,
                parts: vec![Part {
                    content: PartContent::Text(format!("message {i}")),
                    metadata: None,
                    filename: None,
                    media_type: None,
                }],
                metadata: None,
                extensions: vec![],
                reference_task_ids: vec![],
            },
            configuration: None,
            metadata: None,
        };
        let resp = client
            .send_message(&base_url, send_req)
            .await
            .expect("send_message failed");
        if let Some(t) = resp.into_task() {
            task_ids.push(t.id.clone());
        }
    }

    // Wait for all to complete.
    for tid in &task_ids {
        client
            .wait_for_completion(
                &base_url,
                tid,
                Duration::from_millis(100),
                Some(Duration::from_secs(5)),
            )
            .await
            .expect("wait_for_completion failed");
    }

    // List all tasks.
    let list_resp = client
        .list_tasks(&base_url, ListTasksRequest::default())
        .await
        .expect("list_tasks failed");
    assert_eq!(list_resp.total_size, 3);
    assert_eq!(list_resp.tasks.len(), 3);
}

#[tokio::test]
async fn extended_card_returns_agent_card() {
    let base_url = start_server().await;
    let client = A2aClient::new();

    let card = client
        .get_extended_card(&base_url)
        .await
        .expect("get_extended_card failed");
    assert_eq!(card.name, "echo-agent");
}

// ────────────────────────────────────────────────────────────────────────────
// JSON-RPC 2.0 validation tests (TCK compliance)
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn malformed_json_returns_parse_error() {
    let base_url = start_server().await;
    let http = reqwest::Client::new();

    let resp = http
        .post(&base_url)
        .header("content-type", "application/json")
        .body("this is not json{{{")
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.expect("response is not JSON");
    assert_eq!(body["jsonrpc"], "2.0");
    assert!(body["id"].is_null());
    assert_eq!(body["error"]["code"], -32700);
    assert_eq!(body["error"]["message"], "Parse error");
}

#[tokio::test]
async fn missing_jsonrpc_field_returns_invalid_request() {
    let base_url = start_server().await;
    let http = reqwest::Client::new();

    let resp = http
        .post(&base_url)
        .json(&serde_json::json!({
            "id": 1,
            "method": "GetTask",
            "params": {}
        }))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.expect("response is not JSON");
    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["id"], 1);
    assert_eq!(body["error"]["code"], -32600);
    assert_eq!(body["error"]["message"], "Invalid Request");
}

#[tokio::test]
async fn wrong_jsonrpc_version_returns_invalid_request() {
    let base_url = start_server().await;
    let http = reqwest::Client::new();

    let resp = http
        .post(&base_url)
        .json(&serde_json::json!({
            "jsonrpc": "1.0",
            "id": 2,
            "method": "GetTask",
            "params": {}
        }))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.expect("response is not JSON");
    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["id"], 2);
    assert_eq!(body["error"]["code"], -32600);
    assert_eq!(body["error"]["message"], "Invalid Request");
}

#[tokio::test]
async fn missing_method_field_returns_invalid_request() {
    let base_url = start_server().await;
    let http = reqwest::Client::new();

    let resp = http
        .post(&base_url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "params": {}
        }))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.expect("response is not JSON");
    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["id"], 3);
    assert_eq!(body["error"]["code"], -32600);
    assert_eq!(body["error"]["message"], "Invalid Request");
}

#[tokio::test]
async fn missing_id_returns_null_id_in_response() {
    let base_url = start_server().await;
    let http = reqwest::Client::new();

    let resp = http
        .post(&base_url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "UnknownMethod",
            "params": {}
        }))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.expect("response is not JSON");
    assert_eq!(body["jsonrpc"], "2.0");
    assert!(body["id"].is_null());
    assert_eq!(body["error"]["code"], -32601);
}

#[tokio::test]
async fn agent_card_has_cors_headers() {
    let base_url = start_server().await;
    let http = reqwest::Client::new();

    let resp = http
        .get(format!("{base_url}/.well-known/agent-card.json"))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("access-control-allow-origin").map(|v| v.to_str().unwrap()),
        Some("*")
    );
    assert_eq!(
        resp.headers().get("cache-control").map(|v| v.to_str().unwrap()),
        Some("no-cache")
    );
}
