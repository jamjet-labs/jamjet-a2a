//! Federation auth, as a layer and as the server's authenticator.

#![cfg(feature = "federation")]

mod common;

use axum::{middleware, routing::get};
use common::*;
use jamjet_a2a::auth::{FORBIDDEN_ERROR_CODE, UNAUTHENTICATED_ERROR_CODE};
use jamjet_a2a::federation::{federation_auth_layer, FederationPolicy, FederationToken};
use jamjet_a2a::server::A2aServer;
use serde_json::json;

const TASK_NOT_FOUND: i64 = -32001;

fn policy() -> FederationPolicy {
    let token = |token: &str, agent: &str, scopes: &[&str]| FederationToken {
        token: token.into(),
        name: agent.into(),
        agent_id: Some(agent.into()),
        scopes: scopes.iter().map(|s| s.to_string()).collect(),
    };
    FederationPolicy {
        tokens: vec![
            token("tok-alpha", "agent-alpha", &["read", "write"]),
            token("tok-beta", "agent-beta", &["read", "write"]),
            token("tok-ro", "agent-ro", &["read"]),
        ],
        method_scopes: [
            ("SendMessage".to_string(), vec!["write".to_string()]),
            (
                "SendStreamingMessage".to_string(),
                vec!["write".to_string()],
            ),
            ("CancelTask".to_string(), vec!["write".to_string()]),
            ("GetTask".to_string(), vec!["read".to_string()]),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    }
}

async fn layered_server() -> String {
    let router = A2aServer::new(agent_card())
        .with_handler(HoldHandler)
        .into_router()
        .layer(middleware::from_fn_with_state(
            policy(),
            federation_auth_layer,
        ));
    serve_router(router).await
}

async fn authenticator_server() -> String {
    serve(
        A2aServer::new(agent_card())
            .with_handler(HoldHandler)
            .with_authenticator(policy()),
    )
    .await
}

#[tokio::test]
async fn layer_enforces_method_scopes() {
    let url = layered_server().await;
    let ro = Some("tok-ro");

    // Read scope is enough for GetTask (the task just does not exist).
    let (status, body) = rpc(&url, ro, "GetTask", json!({"id": "nope"})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(error_code(&body), Some(TASK_NOT_FOUND));

    // ... but not for SendMessage, under either name.
    for method in ["SendMessage", "message/send"] {
        let (status, body) = rpc(
            &url,
            ro,
            method,
            json!({"message": message("m-1", "x", None, None)}),
        )
        .await;
        assert_eq!(status, 403, "{method}: {body}");
        assert_auth_code(&body, FORBIDDEN_ERROR_CODE);
    }

    let (status, _) = rpc(
        &url,
        Some("tok-alpha"),
        "SendMessage",
        json!({"message": message("m-1", "x", None, None)}),
    )
    .await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn layer_rejects_missing_and_unknown_tokens() {
    let url = layered_server().await;
    for token in [None, Some("tok-unknown")] {
        let (status, body) = rpc(&url, token, "ListTasks", json!({})).await;
        assert_eq!(status, 401, "{body}");
        assert_auth_code(&body, UNAUTHENTICATED_ERROR_CODE);
    }
}

#[tokio::test]
async fn layer_binds_tasks_to_the_caller() {
    let url = layered_server().await;
    let alpha = Some("tok-alpha");
    let beta = Some("tok-beta");

    let task_id = create_task(&url, alpha, None).await;

    let (_, body) = rpc(&url, beta, "GetTask", json!({"id": task_id})).await;
    assert_eq!(error_code(&body), Some(TASK_NOT_FOUND), "{body}");
    let (_, body) = rpc(&url, beta, "ListTasks", json!({})).await;
    assert_eq!(body["result"]["totalSize"], 0, "{body}");

    let (_, body) = rpc(&url, alpha, "ListTasks", json!({})).await;
    assert_eq!(body["result"]["totalSize"], 1, "{body}");
}

#[tokio::test]
async fn layer_only_exempts_the_agent_card_itself() {
    let router = A2aServer::new(agent_card())
        .into_router()
        .route(
            "/private/.well-known/agent-card.json",
            get(|| async { "secret" }),
        )
        .layer(middleware::from_fn_with_state(
            policy(),
            federation_auth_layer,
        ));
    let url = serve_router(router).await;
    let http = reqwest::Client::new();

    let card = http
        .get(format!("{url}/.well-known/agent-card.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(card.status(), 200);

    let private = http
        .get(format!("{url}/private/.well-known/agent-card.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(private.status(), 401);

    let post_card = http
        .post(format!("{url}/.well-known/agent-card.json"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(post_card.status(), 401);
}

#[tokio::test]
async fn policy_as_authenticator_binds_tasks_and_checks_scopes() {
    let url = authenticator_server().await;
    let alpha = Some("tok-alpha");

    let (status, _) = rpc(&url, None, "ListTasks", json!({})).await;
    assert_eq!(status, 401);

    for method in ["SendMessage", "message/send"] {
        let (status, body) = rpc(
            &url,
            Some("tok-ro"),
            method,
            json!({"message": message("m-1", "x", None, None)}),
        )
        .await;
        assert_eq!(status, 403, "{method}: {body}");
    }

    let task_id = create_task(&url, alpha, None).await;
    let (_, body) = rpc(&url, Some("tok-beta"), "GetTask", json!({"id": task_id})).await;
    assert_eq!(error_code(&body), Some(TASK_NOT_FOUND), "{body}");
    let (_, body) = rpc(&url, alpha, "GetTask", json!({"id": task_id})).await;
    assert_eq!(body["result"]["id"], task_id.as_str(), "{body}");
}

#[tokio::test]
async fn optional_auth_still_rejects_bad_tokens() {
    let policy = FederationPolicy {
        require_auth: false,
        ..policy()
    };
    let url = serve(
        A2aServer::new(agent_card())
            .with_handler(HoldHandler)
            .with_authenticator(policy),
    )
    .await;

    let (status, _) = rpc(&url, None, "ListTasks", json!({})).await;
    assert_eq!(status, 200);
    let (status, _) = rpc(&url, Some("tok-unknown"), "ListTasks", json!({})).await;
    assert_eq!(status, 401);
}
