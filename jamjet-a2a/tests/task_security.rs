//! Task ownership, server-generated ids, terminal states and stream closing.

mod common;

use common::*;
use futures::StreamExt;
use jamjet_a2a::auth::{
    Authenticator, Principal, FORBIDDEN_ERROR_CODE, UNAUTHENTICATED_ERROR_CODE,
};
use jamjet_a2a::client::A2aClient;
use jamjet_a2a::server::A2aServer;
use jamjet_a2a_types::*;
use serde_json::json;
use std::time::Duration;

const TASK_NOT_FOUND: i64 = -32001;
const UNSUPPORTED_OPERATION: i64 = -32004;
const INVALID_PARAMS: i64 = -32602;

/// Three fixed bearer tokens. `reader` may only call GetTask and ListTasks.
struct TestTokens;

#[async_trait::async_trait]
impl Authenticator for TestTokens {
    async fn authenticate(&self, headers: &axum::http::HeaderMap) -> Result<Principal, A2aError> {
        let token = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        match token {
            Some("alice-token") => Ok(Principal::new("alice")),
            Some("bob-token") => Ok(Principal::new("bob")),
            Some("reader-token") => Ok(Principal::new("reader")),
            Some("empty-id-token") => Ok(Principal::new("")),
            _ => Err(A2aError::Auth {
                reason: "unknown token".into(),
            }),
        }
    }

    fn authorize(&self, principal: &Principal, method: &str) -> bool {
        principal.id != "reader" || matches!(method, "GetTask" | "ListTasks")
    }
}

// ── server-generated task ids ───────────────────────────────────────────────

#[tokio::test]
async fn client_cannot_choose_the_id_of_a_new_task() {
    let url = serve(A2aServer::new(agent_card()).with_handler(HoldHandler)).await;

    let (_, body) = rpc(
        &url,
        None,
        "SendMessage",
        json!({"message": message("m-1", "hi", Some("chosen-by-client"), None)}),
    )
    .await;
    assert_eq!(error_code(&body), Some(TASK_NOT_FOUND), "{body}");

    let (_, body) = rpc(&url, None, "ListTasks", json!({})).await;
    assert_eq!(body["result"]["totalSize"], 0, "{body}");

    // Without a taskId the server picks a UUID.
    let id = create_task(&url, None, None).await;
    assert!(uuid_like(&id), "{id}");
}

fn uuid_like(s: &str) -> bool {
    s.len() == 36 && s.chars().filter(|c| *c == '-').count() == 4
}

#[tokio::test]
async fn streaming_message_cannot_replace_an_existing_task() {
    let url = serve(A2aServer::new(agent_card()).with_handler(HoldHandler)).await;

    let (_, body) = rpc(
        &url,
        None,
        "SendMessage",
        json!({"message": message("victim-msg", "victim", None, Some("victim-ctx"))}),
    )
    .await;
    let task_id = body["result"]["task"]["id"].as_str().unwrap().to_string();
    wait_for_state(&url, None, &task_id, "TASK_STATE_WORKING").await;

    // Reusing the id on the streaming path used to insert a fresh task over it.
    let (status, _) = rpc(
        &url,
        None,
        "SendStreamingMessage",
        json!({"message": message("attacker-msg", "attacker", Some(&task_id), None)}),
    )
    .await;
    assert_eq!(status, 200);

    let (_, body) = rpc(&url, None, "GetTask", json!({"id": task_id})).await;
    let task = &body["result"];
    assert_eq!(task["contextId"], "victim-ctx", "{body}");
    assert_eq!(task["history"][0]["messageId"], "victim-msg", "{body}");
    assert_eq!(task["status"]["state"], "TASK_STATE_WORKING", "{body}");
}

// ── ownership ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn other_principals_cannot_see_or_change_a_task() {
    let url = serve(
        A2aServer::new(agent_card())
            .with_handler(HoldHandler)
            .with_authenticator(TestTokens),
    )
    .await;
    let alice = Some("alice-token");
    let bob = Some("bob-token");

    let task_id = create_task(&url, alice, Some("ctx-a")).await;
    wait_for_state(&url, alice, &task_id, "TASK_STATE_WORKING").await;

    for (method, params) in [
        ("GetTask", json!({"id": task_id})),
        ("CancelTask", json!({"id": task_id})),
        ("SubscribeToTask", json!({"id": task_id})),
        (
            "SendMessage",
            json!({"message": message("b-1", "x", Some(&task_id), None)}),
        ),
        (
            "SendStreamingMessage",
            json!({"message": message("b-2", "x", Some(&task_id), None)}),
        ),
        ("tasks/get", json!({"id": task_id})),
    ] {
        let (_, body) = rpc(&url, bob, method, params).await;
        assert_eq!(error_code(&body), Some(TASK_NOT_FOUND), "{method}: {body}");
    }

    let (_, body) = rpc(&url, bob, "ListTasks", json!({})).await;
    assert_eq!(body["result"]["totalSize"], 0, "{body}");
    assert_eq!(body["result"]["tasks"], json!([]), "{body}");

    let (_, body) = rpc(&url, bob, "ListTasks", json!({"pageToken": task_id})).await;
    assert_eq!(error_code(&body), Some(INVALID_PARAMS), "{body}");

    // Alice's task is untouched.
    let (_, body) = rpc(&url, alice, "GetTask", json!({"id": task_id})).await;
    assert_eq!(body["result"]["status"]["state"], "TASK_STATE_WORKING");
    assert_eq!(body["result"]["history"].as_array().unwrap().len(), 1);
    let (_, body) = rpc(&url, alice, "ListTasks", json!({})).await;
    assert_eq!(body["result"]["totalSize"], 1, "{body}");
}

#[tokio::test]
async fn requests_without_valid_credentials_are_rejected() {
    let url = serve(
        A2aServer::new(agent_card())
            .with_handler(HoldHandler)
            .with_authenticator(TestTokens),
    )
    .await;

    for token in [None, Some("wrong-token")] {
        let (status, headers, body) = rpc_raw(&url, token, "ListTasks", json!({})).await;
        assert_eq!(status, 401, "{body}");
        assert_eq!(headers.get("www-authenticate").unwrap(), "Bearer");
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_auth_code(&body, UNAUTHENTICATED_ERROR_CODE);
    }

    // The public Agent Card stays public.
    let card = A2aClient::new().discover(&url).await.unwrap();
    assert_eq!(card.name, "test-agent");
}

#[tokio::test]
async fn authorize_hook_refuses_methods_including_aliases() {
    let url = serve(
        A2aServer::new(agent_card())
            .with_handler(HoldHandler)
            .with_authenticator(TestTokens),
    )
    .await;
    let reader = Some("reader-token");

    let (status, _) = rpc(&url, reader, "ListTasks", json!({})).await;
    assert_eq!(status, 200);

    for method in ["SendMessage", "message/send", "CancelTask", "tasks/cancel"] {
        let (status, body) = rpc(
            &url,
            reader,
            method,
            json!({"message": message("r-1", "x", None, None), "id": "x"}),
        )
        .await;
        assert_eq!(status, 403, "{method}: {body}");
        assert_auth_code(&body, FORBIDDEN_ERROR_CODE);
    }
}

// ── multi-turn ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn follow_up_message_continues_the_task() {
    let url = serve(A2aServer::new(agent_card()).with_handler(HoldHandler)).await;
    let task_id = create_task(&url, None, Some("ctx-1")).await;

    let (_, body) = rpc(
        &url,
        None,
        "SendMessage",
        json!({"message": message("m-2", "more", Some(&task_id), Some("ctx-1"))}),
    )
    .await;
    assert_eq!(body["result"]["task"]["id"], task_id.as_str(), "{body}");
    assert_eq!(
        body["result"]["task"]["history"].as_array().unwrap().len(),
        2
    );
}

#[tokio::test]
async fn server_assigns_a_context_id_when_the_message_has_none() {
    let url = serve(A2aServer::new(agent_card()).with_handler(HoldHandler)).await;
    let task_id = create_task(&url, None, None).await;

    let (_, body) = rpc(&url, None, "GetTask", json!({"id": task_id})).await;
    let context_id = body["result"]["contextId"]
        .as_str()
        .unwrap_or_else(|| panic!("no contextId in {body}"))
        .to_string();
    assert!(uuid_like(&context_id), "{context_id}");

    // A follow-up that carries the assigned contextId is accepted.
    let (_, body) = rpc(
        &url,
        None,
        "SendMessage",
        json!({"message": message("m-2", "more", Some(&task_id), Some(&context_id))}),
    )
    .await;
    assert_eq!(body["result"]["task"]["id"], task_id.as_str(), "{body}");
}

#[tokio::test]
async fn follow_up_with_a_different_context_is_rejected() {
    let url = serve(A2aServer::new(agent_card()).with_handler(HoldHandler)).await;
    let task_id = create_task(&url, None, Some("ctx-1")).await;

    let (_, body) = rpc(
        &url,
        None,
        "SendMessage",
        json!({"message": message("m-2", "more", Some(&task_id), Some("ctx-2"))}),
    )
    .await;
    assert_eq!(error_code(&body), Some(INVALID_PARAMS), "{body}");
}

// ── terminal states ─────────────────────────────────────────────────────────

#[tokio::test]
async fn completed_task_rejects_further_messages() {
    let url = serve(A2aServer::new(agent_card()).with_handler(CompleteHandler)).await;
    let task_id = create_task(&url, None, None).await;
    wait_for_state(&url, None, &task_id, "TASK_STATE_COMPLETED").await;

    for method in ["SendMessage", "SendStreamingMessage"] {
        let (_, body) = rpc(
            &url,
            None,
            method,
            json!({"message": message("m-2", "again", Some(&task_id), None)}),
        )
        .await;
        assert_eq!(
            error_code(&body),
            Some(UNSUPPORTED_OPERATION),
            "{method}: {body}"
        );
    }

    let (_, body) = rpc(&url, None, "GetTask", json!({"id": task_id})).await;
    assert_eq!(body["result"]["status"]["state"], "TASK_STATE_COMPLETED");
    assert_eq!(body["result"]["history"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn subscribing_to_a_terminal_task_is_rejected() {
    let url = serve(A2aServer::new(agent_card()).with_handler(CompleteHandler)).await;
    let task_id = create_task(&url, None, None).await;
    wait_for_state(&url, None, &task_id, "TASK_STATE_COMPLETED").await;

    let (_, body) = rpc(&url, None, "SubscribeToTask", json!({"id": task_id})).await;
    assert_eq!(error_code(&body), Some(UNSUPPORTED_OPERATION), "{body}");
}

// ── streams close at the terminal state ─────────────────────────────────────

fn send_request(text: &str) -> SendMessageRequest {
    SendMessageRequest {
        tenant: None,
        message: Message {
            message_id: "s-1".into(),
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
        },
        configuration: None,
        metadata: None,
    }
}

fn state_of(event: &StreamResponse) -> Option<TaskState> {
    match event {
        StreamResponse::StatusUpdate(e) => Some(e.status.state.clone()),
        _ => None,
    }
}

#[tokio::test]
async fn streaming_message_stream_ends_when_task_completes() {
    let url = serve(A2aServer::new(agent_card()).with_handler(CompleteHandler)).await;
    let stream = A2aClient::new()
        .send_streaming(&url, send_request("hi"))
        .await
        .unwrap();

    let events: Vec<_> = tokio::time::timeout(Duration::from_secs(5), stream.collect())
        .await
        .expect("stream did not close after the task completed");
    let states: Vec<_> = events
        .iter()
        .map(|e| state_of(e.as_ref().unwrap()))
        .collect();
    assert_eq!(
        states.last(),
        Some(&Some(TaskState::Completed)),
        "{states:?}"
    );
}

#[tokio::test]
async fn subscription_ends_when_task_is_canceled() {
    let url = serve(A2aServer::new(agent_card()).with_handler(HoldHandler)).await;
    let task_id = create_task(&url, None, None).await;
    wait_for_state(&url, None, &task_id, "TASK_STATE_WORKING").await;

    let client = A2aClient::new();
    let stream = client.subscribe(&url, &task_id).await.unwrap();
    let collector = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(5), stream.collect::<Vec<_>>()).await
    });

    // Give the subscription a moment to register before canceling.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (_, body) = rpc(&url, None, "CancelTask", json!({"id": task_id})).await;
    assert_eq!(body["result"]["status"]["state"], "TASK_STATE_CANCELED");

    let events = collector
        .await
        .unwrap()
        .expect("subscription did not close after the task was canceled");
    let last = events.last().expect("no events").as_ref().unwrap();
    assert_eq!(state_of(last), Some(TaskState::Canceled));
}

// ── a task canceled before its handler starts ───────────────────────────────

/// Wraps the in-memory store and cancels every task right after inserting
/// it, so the task is already terminal when the handler would start.
struct CancelOnInsert(jamjet_a2a::store::InMemoryTaskStore);

#[async_trait::async_trait]
impl jamjet_a2a::store::TaskStore for CancelOnInsert {
    async fn insert(&self, task: Task, owner: Option<String>) -> Result<(), A2aError> {
        let id = task.id.clone();
        self.0.insert(task, owner).await?;
        self.0.cancel(&id).await
    }
    async fn get_record(
        &self,
        task_id: &str,
    ) -> Result<Option<jamjet_a2a::store::TaskRecord>, A2aError> {
        self.0.get_record(task_id).await
    }
    async fn update_status(&self, task_id: &str, status: TaskStatus) -> Result<(), A2aError> {
        self.0.update_status(task_id, status).await
    }
    async fn add_artifact(&self, task_id: &str, artifact: Artifact) -> Result<(), A2aError> {
        self.0.add_artifact(task_id, artifact).await
    }
    async fn list(
        &self,
        req: &ListTasksRequest,
        owner: Option<&str>,
    ) -> Result<ListTasksResponse, A2aError> {
        self.0.list(req, owner).await
    }
    async fn append_message(&self, task_id: &str, message: Message) -> Result<(), A2aError> {
        self.0.append_message(task_id, message).await
    }
    async fn cancel(&self, task_id: &str) -> Result<(), A2aError> {
        self.0.cancel(task_id).await
    }
    async fn subscribe(
        &self,
        task_id: &str,
    ) -> Option<tokio::sync::broadcast::Receiver<StreamResponse>> {
        self.0.subscribe(task_id).await
    }
}

struct RecordingHandler(std::sync::Arc<std::sync::atomic::AtomicBool>);

#[async_trait::async_trait]
impl jamjet_a2a::server::TaskHandler for RecordingHandler {
    async fn handle(
        &self,
        _task_id: String,
        _message: Message,
        _store: std::sync::Arc<dyn jamjet_a2a::store::TaskStore>,
    ) -> Result<(), A2aError> {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn handler_does_not_run_for_a_task_canceled_before_it_started() {
    let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let url = serve(
        A2aServer::new(agent_card())
            .with_store(CancelOnInsert(jamjet_a2a::store::InMemoryTaskStore::new()))
            .with_handler(RecordingHandler(ran.clone())),
    )
    .await;

    let task_id = create_task(&url, None, None).await;
    wait_for_state(&url, None, &task_id, "TASK_STATE_CANCELED").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !ran.load(std::sync::atomic::Ordering::SeqCst),
        "handler ran for a canceled task"
    );
}

// ── review follow-ups ───────────────────────────────────────────────────────

#[tokio::test]
async fn principal_with_empty_id_is_rejected() {
    let url = serve(
        A2aServer::new(agent_card())
            .with_handler(HoldHandler)
            .with_authenticator(TestTokens),
    )
    .await;
    let (status, body) = rpc(&url, Some("empty-id-token"), "ListTasks", json!({})).await;
    assert_eq!(status, 401, "{body}");
}

#[tokio::test]
async fn unauthenticated_request_is_rejected_before_its_body_is_read() {
    let url = serve(
        A2aServer::new(agent_card())
            .with_handler(HoldHandler)
            .with_authenticator(TestTokens),
    )
    .await;
    // Announce a body and never send it. A server that reads the body before
    // authenticating sends "100 Continue" and waits; this one must answer 401
    // straight away.
    let request = b"POST / HTTP/1.1\r\nHost: test\r\nContent-Type: application/json\r\n\
Content-Length: 3145728\r\nExpect: 100-continue\r\n\r\n";
    let response = raw_exchange(&url, request, Duration::from_secs(3)).await;
    assert!(
        response.starts_with("HTTP/1.1 401"),
        "expected 401 before the body, got {response:?}"
    );
}

#[tokio::test]
async fn follow_up_without_context_gets_the_tasks_context() {
    let url = serve(A2aServer::new(agent_card()).with_handler(HoldHandler)).await;
    let task_id = create_task(&url, None, Some("ctx-1")).await;

    let (_, body) = rpc(
        &url,
        None,
        "SendMessage",
        json!({"message": message("m-2", "more", Some(&task_id), None)}),
    )
    .await;
    let history = &body["result"]["task"]["history"];
    assert_eq!(history[1]["contextId"], "ctx-1", "{body}");
}

/// Asks for input on the first message and completes on the next one.
struct AskThenFinishHandler;

#[async_trait::async_trait]
impl jamjet_a2a::server::TaskHandler for AskThenFinishHandler {
    async fn handle(
        &self,
        task_id: String,
        message: Message,
        store: std::sync::Arc<dyn jamjet_a2a::store::TaskStore>,
    ) -> Result<(), A2aError> {
        let first = message.message_id == "s-1" || message.message_id == "m-1";
        let state = if first {
            TaskState::InputRequired
        } else {
            TaskState::Completed
        };
        store
            .update_status(
                &task_id,
                TaskStatus {
                    state,
                    message: None,
                    timestamp: None,
                },
            )
            .await
    }
}

#[tokio::test]
async fn stream_ends_when_the_task_needs_input() {
    let url = serve(A2aServer::new(agent_card()).with_handler(AskThenFinishHandler)).await;
    let stream = A2aClient::new()
        .send_streaming(&url, send_request("hi"))
        .await
        .unwrap();
    let events: Vec<_> = tokio::time::timeout(Duration::from_secs(5), stream.collect())
        .await
        .expect("stream did not close when the task asked for input");
    let last = events.last().expect("no events").as_ref().unwrap();
    assert_eq!(state_of(last), Some(TaskState::InputRequired));
}

#[tokio::test]
async fn follow_up_stream_ends_when_the_task_completes() {
    let url = serve(A2aServer::new(agent_card()).with_handler(AskThenFinishHandler)).await;
    let task_id = create_task(&url, None, None).await;
    wait_for_state(&url, None, &task_id, "TASK_STATE_INPUT_REQUIRED").await;

    let mut req = send_request("here is the input");
    req.message.message_id = "s-2".into();
    req.message.task_id = Some(task_id.clone());
    let stream = A2aClient::new().send_streaming(&url, req).await.unwrap();
    let events: Vec<_> = tokio::time::timeout(Duration::from_secs(5), stream.collect())
        .await
        .expect("follow-up stream did not close after the task completed");
    let last = events.last().expect("no events").as_ref().unwrap();
    assert_eq!(state_of(last), Some(TaskState::Completed));
}

// ── second review ───────────────────────────────────────────────────────────

/// Allows everything except CancelTask (a denylist).
struct NoCancel;

#[async_trait::async_trait]
impl Authenticator for NoCancel {
    async fn authenticate(&self, _headers: &axum::http::HeaderMap) -> Result<Principal, A2aError> {
        Ok(Principal::new("u"))
    }
    fn authorize(&self, _principal: &Principal, method: &str) -> bool {
        method != "CancelTask"
    }
}

#[tokio::test]
async fn alias_cannot_get_around_a_denylist_authorizer() {
    let url = serve(
        A2aServer::new(agent_card())
            .with_handler(HoldHandler)
            .with_authenticator(NoCancel),
    )
    .await;
    let task_id = create_task(&url, None, None).await;
    for method in ["CancelTask", "tasks/cancel"] {
        let (status, body) = rpc(&url, None, method, json!({"id": task_id})).await;
        assert_eq!(status, 403, "{method}: {body}");
    }
}

fn is_protocol_error(result: &Result<impl Sized, A2aError>, code: i32) -> bool {
    matches!(result, Err(A2aError::Protocol(p)) if p.json_rpc_code() == code)
}

#[tokio::test]
async fn client_reports_errors_from_streaming_calls() {
    let url = serve(A2aServer::new(agent_card()).with_handler(CompleteHandler)).await;
    let task_id = create_task(&url, None, None).await;
    wait_for_state(&url, None, &task_id, "TASK_STATE_COMPLETED").await;
    let client = A2aClient::new();

    let result = client.subscribe(&url, &task_id).await.map(|_| ());
    assert!(is_protocol_error(&result, -32004), "{result:?}");

    let mut follow_up = send_request("again");
    follow_up.message.task_id = Some(task_id.clone());
    let result = client.send_streaming(&url, follow_up).await.map(|_| ());
    assert!(is_protocol_error(&result, -32004), "{result:?}");

    let mut unknown = send_request("hi");
    unknown.message.task_id = Some("no-such-task".into());
    let result = client.send_streaming(&url, unknown).await.map(|_| ());
    assert!(is_protocol_error(&result, -32001), "{result:?}");
}

#[tokio::test]
async fn streams_start_with_the_task() {
    let url = serve(A2aServer::new(agent_card()).with_handler(HoldHandler)).await;
    let client = A2aClient::new();

    // SendStreamingMessage (A2A 3.1.2).
    let mut stream = client
        .send_streaming(&url, send_request("hi"))
        .await
        .unwrap();
    let first = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("no first event")
        .unwrap()
        .unwrap();
    let StreamResponse::Task(task) = first else {
        panic!("first event is not a Task: {first:?}");
    };

    // SubscribeToTask (A2A 3.1.6).
    wait_for_state(&url, None, &task.id, "TASK_STATE_WORKING").await;
    let mut stream = client.subscribe(&url, &task.id).await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("no first event")
        .unwrap()
        .unwrap();
    match first {
        StreamResponse::Task(t) => assert_eq!(t.id, task.id),
        other => panic!("first event is not a Task: {other:?}"),
    }
}

#[tokio::test]
async fn subscribing_to_a_task_waiting_for_input_sends_it_and_closes() {
    let url = serve(A2aServer::new(agent_card()).with_handler(AskThenFinishHandler)).await;
    let task_id = create_task(&url, None, None).await;
    wait_for_state(&url, None, &task_id, "TASK_STATE_INPUT_REQUIRED").await;

    let stream = A2aClient::new().subscribe(&url, &task_id).await.unwrap();
    let events: Vec<_> = tokio::time::timeout(Duration::from_secs(5), stream.collect())
        .await
        .expect("subscription to an input-required task did not close");
    assert_eq!(events.len(), 1, "{events:?}");
    match events[0].as_ref().unwrap() {
        StreamResponse::Task(t) => assert_eq!(t.status.state, TaskState::InputRequired),
        other => panic!("expected the Task, got {other:?}"),
    }
}

#[tokio::test]
async fn open_streams_per_caller_are_limited() {
    let url = serve(
        A2aServer::new(agent_card())
            .with_handler(HoldHandler)
            .with_max_streams_per_caller(1),
    )
    .await;
    let first_task = create_task(&url, None, None).await;
    let second_task = create_task(&url, None, None).await;
    let client = A2aClient::new();

    let held = client.subscribe(&url, &first_task).await.unwrap();
    let (status, body) = rpc(&url, None, "SubscribeToTask", json!({"id": second_task})).await;
    assert_eq!(status, 429, "{body}");
    assert_auth_code(&body, jamjet_a2a::server::TOO_MANY_STREAMS_ERROR_CODE);

    // Once the first stream ends, the slot is free again.
    let (_, body) = rpc(&url, None, "CancelTask", json!({"id": first_task})).await;
    assert_eq!(body["result"]["status"]["state"], "TASK_STATE_CANCELED");
    let _: Vec<_> = tokio::time::timeout(Duration::from_secs(5), held.collect())
        .await
        .expect("first stream did not end");
    let stream = client.subscribe(&url, &second_task).await;
    assert!(stream.is_ok(), "{:?}", stream.err());
}

#[tokio::test]
async fn slow_request_body_gets_408() {
    let url = serve(
        A2aServer::new(agent_card())
            .with_handler(HoldHandler)
            .with_body_read_timeout(Duration::from_millis(300)),
    )
    .await;
    let request = b"POST / HTTP/1.1\r\nHost: test\r\nContent-Type: application/json\r\n\
Content-Length: 100\r\n\r\n{\"jsonrpc\":";
    let response = raw_exchange(&url, request, Duration::from_secs(3)).await;
    assert!(response.starts_with("HTTP/1.1 408"), "{response:?}");
}

#[tokio::test]
async fn slow_request_headers_close_the_connection() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = A2aServer::new(agent_card())
        .with_handler(HoldHandler)
        .with_header_read_timeout(Duration::from_millis(300));
    tokio::spawn(server.serve(listener));

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let addr = url.trim_start_matches("http://").to_string();
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(b"POST / HTTP/1.1\r\nHost: te")
        .await
        .unwrap();
    let mut buf = [0u8; 256];
    let read = tokio::time::timeout(Duration::from_secs(3), stream.read(&mut buf)).await;
    assert!(
        read.is_ok(),
        "server kept a connection with unfinished headers open"
    );
}

#[tokio::test]
async fn context_id_of_another_caller_is_refused() {
    let url = serve(
        A2aServer::new(agent_card())
            .with_handler(HoldHandler)
            .with_authenticator(TestTokens),
    )
    .await;
    create_task(&url, Some("alice-token"), Some("ctx-shared")).await;
    let (_, body) = rpc(
        &url,
        Some("bob-token"),
        "SendMessage",
        json!({"message": message("b-1", "x", None, Some("ctx-shared"))}),
    )
    .await;
    assert_eq!(error_code(&body), Some(INVALID_PARAMS), "{body}");
}

/// A store whose `list` ignores the caller and always returns alice's tasks.
struct LeakyList(jamjet_a2a::store::InMemoryTaskStore);

#[async_trait::async_trait]
impl jamjet_a2a::store::TaskStore for LeakyList {
    async fn insert(&self, task: Task, owner: Option<String>) -> Result<(), A2aError> {
        self.0.insert(task, owner).await
    }
    async fn get_record(
        &self,
        task_id: &str,
    ) -> Result<Option<jamjet_a2a::store::TaskRecord>, A2aError> {
        self.0.get_record(task_id).await
    }
    async fn update_status(&self, task_id: &str, status: TaskStatus) -> Result<(), A2aError> {
        self.0.update_status(task_id, status).await
    }
    async fn add_artifact(&self, task_id: &str, artifact: Artifact) -> Result<(), A2aError> {
        self.0.add_artifact(task_id, artifact).await
    }
    async fn list(
        &self,
        req: &ListTasksRequest,
        _owner: Option<&str>,
    ) -> Result<ListTasksResponse, A2aError> {
        self.0.list(req, Some("alice")).await
    }
    async fn append_message(&self, task_id: &str, message: Message) -> Result<(), A2aError> {
        self.0.append_message(task_id, message).await
    }
    async fn cancel(&self, task_id: &str) -> Result<(), A2aError> {
        self.0.cancel(task_id).await
    }
    async fn subscribe(
        &self,
        task_id: &str,
    ) -> Option<tokio::sync::broadcast::Receiver<StreamResponse>> {
        self.0.subscribe(task_id).await
    }
}

#[tokio::test]
async fn server_drops_other_callers_tasks_from_a_leaky_list() {
    let url = serve(
        A2aServer::new(agent_card())
            .with_handler(HoldHandler)
            .with_store(LeakyList(jamjet_a2a::store::InMemoryTaskStore::new()))
            .with_authenticator(TestTokens),
    )
    .await;
    create_task(&url, Some("alice-token"), None).await;
    let (_, body) = rpc(&url, Some("bob-token"), "ListTasks", json!({})).await;
    assert_eq!(body["result"]["tasks"], json!([]), "{body}");
}
