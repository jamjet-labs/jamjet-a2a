//! The client gives up on servers that stop answering.

mod common;

use axum::response::sse::{Event, Sse};
use axum::routing::post;
use futures::StreamExt;
use jamjet_a2a::client::A2aClient;
use jamjet_a2a_types::*;
use std::convert::Infallible;
use std::time::Duration;

/// A TCP server that accepts connections and never writes a byte.
async fn silent_server() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });
    url
}

fn get_task_request() -> GetTaskRequest {
    GetTaskRequest {
        tenant: None,
        id: "t-1".into(),
        history_length: None,
    }
}

fn send_request() -> SendMessageRequest {
    SendMessageRequest {
        tenant: None,
        message: Message {
            message_id: "m-1".into(),
            context_id: None,
            task_id: None,
            role: Role::User,
            parts: vec![],
            metadata: None,
            extensions: vec![],
            reference_task_ids: vec![],
        },
        configuration: None,
        metadata: None,
    }
}

fn is_timeout<T>(result: &Result<T, A2aError>) -> bool {
    matches!(
        result,
        Err(A2aError::Transport(A2aTransportError::Timeout { .. }))
    )
}

#[tokio::test]
async fn unary_call_times_out_when_the_server_never_answers() {
    let url = silent_server().await;
    let client = A2aClient::new().with_request_timeout(Duration::from_millis(300));

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        client.get_task(&url, get_task_request()),
    )
    .await
    .expect("client call hung past its request timeout");
    assert!(is_timeout(&result), "{:?}", result.err());

    let result = tokio::time::timeout(Duration::from_secs(5), client.discover(&url))
        .await
        .expect("discover hung past its request timeout");
    assert!(is_timeout(&result), "{:?}", result.err());
}

#[tokio::test]
async fn streaming_call_times_out_waiting_for_headers() {
    let url = silent_server().await;
    let client = A2aClient::new().with_request_timeout(Duration::from_millis(300));

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        client.send_streaming(&url, send_request()),
    )
    .await
    .expect("streaming call hung past its request timeout");
    assert!(
        is_timeout(&result),
        "{:?}",
        result.err().map(|e| e.to_string())
    );
}

#[tokio::test]
async fn stream_times_out_when_the_server_goes_quiet() {
    // One event, then nothing: no further events and no keep-alives.
    let router = axum::Router::new().route(
        "/",
        post(|| async {
            let event = serde_json::json!({
                "taskId": "t-1",
                "contextId": "c-1",
                "status": {"state": "TASK_STATE_WORKING"}
            });
            let first = futures::stream::once(async move {
                Ok::<_, Infallible>(Event::default().data(event.to_string()))
            });
            Sse::new(first.chain(futures::stream::pending()))
        }),
    );
    let url = common::serve_router(router).await;
    let client = A2aClient::new().with_stream_idle_timeout(Duration::from_millis(300));

    let mut stream = client.send_streaming(&url, send_request()).await.unwrap();
    let first = stream.next().await.expect("first event");
    assert!(first.is_ok(), "{first:?}");

    let next = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("stream hung past its idle timeout")
        .expect("stream ended without reporting the timeout");
    assert!(is_timeout(&next), "{next:?}");
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn stream_gives_up_on_an_event_that_never_ends() {
    // 17 MB of data with no line break, and the connection stays open, so
    // only the size limit can end the stream.
    let router = axum::Router::new().route(
        "/",
        post(|| async {
            let chunk = axum::body::Bytes::from("x".repeat(17 * 1024 * 1024));
            let body = futures::stream::once(async move { Ok::<_, Infallible>(chunk) })
                .chain(futures::stream::pending());
            (
                [("content-type", "text/event-stream")],
                axum::body::Body::from_stream(body),
            )
        }),
    );
    let url = common::serve_router(router).await;
    let client = A2aClient::new();

    let mut stream = client.send_streaming(&url, send_request()).await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .expect("stream hung")
        .expect("stream ended without reporting the oversized event");
    match first {
        Err(A2aError::Transport(A2aTransportError::InvalidResponse { details })) => {
            assert!(details.contains("larger than"), "{details}")
        }
        other => panic!("expected a size error, got {other:?}"),
    }
}

/// Serve a fixed body with the given content type for POST /.
async fn fixed_server(content_type: &'static str, body: String) -> String {
    let router = axum::Router::new().route(
        "/",
        post(move || {
            let body = body.clone();
            async move { ([("content-type", content_type)], body) }
        }),
    );
    common::serve_router(router).await
}

fn state(event: &StreamResponse) -> Option<TaskState> {
    match event {
        StreamResponse::StatusUpdate(e) => Some(e.status.state.clone()),
        _ => None,
    }
}

#[tokio::test]
async fn sse_parser_handles_crlf_bare_colons_multiline_data_and_json_rpc() {
    let working = r#"{"taskId":"t-1","contextId":"c","status":{"state":"TASK_STATE_WORKING"}}"#;
    let body = format!(
        ": a comment\r\n\
         data:{working}\r\n\r\n\
         event: update\n\
         data: {{\"taskId\":\"t-1\",\n\
         data: \"contextId\":\"c\",\"status\":{{\"state\":\"TASK_STATE_INPUT_REQUIRED\"}}}}\n\n\
         data: {{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"statusUpdate\":{working}}}}}\r\r\
         data: {{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{{\"code\":-32001,\"message\":\"gone\"}}}}\n\n"
    );
    let url = fixed_server("text/event-stream", body).await;
    let stream = A2aClient::new()
        .send_streaming(&url, send_request())
        .await
        .unwrap();
    let items: Vec<_> = stream.collect().await;
    assert_eq!(items.len(), 4, "{items:?}");
    assert_eq!(state(items[0].as_ref().unwrap()), Some(TaskState::Working));
    assert_eq!(
        state(items[1].as_ref().unwrap()),
        Some(TaskState::InputRequired)
    );
    assert_eq!(state(items[2].as_ref().unwrap()), Some(TaskState::Working));
    assert!(
        matches!(
            &items[3],
            Err(A2aError::Protocol(A2aProtocolError::TaskNotFound { .. }))
        ),
        "{:?}",
        items[3]
    );
}

#[tokio::test]
async fn stream_that_stops_mid_event_is_an_error() {
    let body = r#"data: {"taskId":"t-1","contextId":"c","status":{"state":"TASK_STATE_WORKING"}}"#;
    let url = fixed_server("text/event-stream", body.to_string()).await;
    let stream = A2aClient::new()
        .send_streaming(&url, send_request())
        .await
        .unwrap();
    let items: Vec<_> = stream.collect().await;
    assert_eq!(items.len(), 1, "{items:?}");
    assert!(
        matches!(
            &items[0],
            Err(A2aError::Transport(
                A2aTransportError::InvalidResponse { .. }
            ))
        ),
        "{items:?}"
    );
}

#[tokio::test]
async fn oversized_unary_response_is_refused() {
    let body = format!(
        r#"{{"jsonrpc":"2.0","id":1,"result":{{"pad":"{}"}}}}"#,
        "x".repeat(17 * 1024 * 1024)
    );
    let url = fixed_server("application/json", body).await;
    let result = A2aClient::new().get_task(&url, get_task_request()).await;
    match result {
        Err(A2aError::Transport(A2aTransportError::InvalidResponse { details })) => {
            assert!(details.contains("larger than"), "{details}")
        }
        other => panic!("expected a size error, got {other:?}"),
    }
}
