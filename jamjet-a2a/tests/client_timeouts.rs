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
