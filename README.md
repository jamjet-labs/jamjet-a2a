# jamjet-a2a

[![Crates.io](https://img.shields.io/crates/v/jamjet-a2a.svg)](https://crates.io/crates/jamjet-a2a)
[![docs.rs](https://docs.rs/jamjet-a2a/badge.svg)](https://docs.rs/jamjet-a2a)
[![License](https://img.shields.io/badge/license-Apache--2.0%20OR%20MIT-blue.svg)](#license)

Standalone Rust SDK for the A2A (Agent-to-Agent) protocol v1.0 — client, server, coordinator, MCP bridge.

## Feature Flags

| Feature | Default | Description |
|---------|---------|-------------|
| `client` | yes | `A2aClient` with all v1.0 operations |
| `server` | yes | `A2aServer`, `TaskHandler`, `TaskStore` |
| `federation` | no | Bearer token auth, mTLS, `FederationPolicy` |
| `coordinator` | no | Multi-dimensional agent scoring and selection |
| `mcp-bridge` | no | Bidirectional A2A ↔ MCP tool mapping |

## Quick Start — Client

```rust
use jamjet_a2a::client::A2aClient;
use jamjet_a2a_types::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = A2aClient::new();

    // Discover an agent's card
    let card = client.discover("https://agent.example.com").await?;
    println!("Agent: {} v{}", card.name, card.version);

    // Send a message
    let resp = client
        .send_message(
            "https://agent.example.com",
            SendMessageRequest {
                tenant: None,
                message: Message {
                    message_id: "msg-1".into(),
                    role: Role::User,
                    parts: vec![Part {
                        content: PartContent::Text("Hello".into()),
                        metadata: None,
                        filename: None,
                        media_type: None,
                    }],
                    context_id: None,
                    task_id: None,
                    metadata: None,
                    extensions: vec![],
                    reference_task_ids: vec![],
                },
                configuration: None,
                metadata: None,
            },
        )
        .await?;

    println!("{resp:?}");
    Ok(())
}
```

## Quick Start — Server

```rust
use jamjet_a2a::server::{A2aServer, TaskHandler};
use jamjet_a2a::store::TaskStore;
use jamjet_a2a_types::*;
use std::sync::Arc;

struct Echo;

#[async_trait::async_trait]
impl TaskHandler for Echo {
    async fn handle(
        &self,
        task_id: String,
        message: Message,
        store: Arc<dyn TaskStore>,
    ) -> Result<(), A2aError> {
        // Echo the first text part back as a completed status message
        let text = message.parts.iter().find_map(|p| match &p.content {
            PartContent::Text(t) => Some(t.clone()),
            _ => None,
        }).unwrap_or_default();

        store.update_status(&task_id, TaskStatus {
            state: TaskState::Completed,
            message: Some(Message {
                message_id: "reply-1".into(),
                role: Role::Agent,
                parts: vec![Part {
                    content: PartContent::Text(format!("Echo: {text}")),
                    metadata: None,
                    filename: None,
                    media_type: None,
                }],
                context_id: None,
                task_id: Some(task_id.clone()),
                metadata: None,
                extensions: vec![],
                reference_task_ids: vec![],
            }),
            timestamp: None,
        }).await?;

        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), A2aError> {
    let card = AgentCard {
        name: "echo".into(),
        description: "Echo agent".into(),
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
            description: "Echoes input back".into(),
            ..Default::default()
        }],
        provider: None,
        security_schemes: Default::default(),
        security_requirements: vec![],
        signatures: vec![],
        icon_url: None,
    };

    A2aServer::new(card)
        .with_handler(Echo)
        .with_port(3000)
        .start()
        .await
}
```

## Security Model

Without an authenticator the server is single-tenant. Any caller that can reach it can list, read, cancel and subscribe to every task. Run it that way only on loopback or behind something that authenticates callers. `A2aServer::start()` binds to `127.0.0.1` by default. Use `with_bind_addr` to listen elsewhere. Bind to a non-loopback address without an authenticator and the server logs a warning at startup.

To serve more than one caller, plug in an `Authenticator`:

```rust
use jamjet_a2a::auth::{Authenticator, Principal};
use jamjet_a2a_types::A2aError;

struct Tokens;

#[async_trait::async_trait]
impl Authenticator for Tokens {
    async fn authenticate(&self, headers: &jamjet_a2a::auth::HeaderMap) -> Result<Principal, A2aError> {
        match headers.get("authorization").and_then(|v| v.to_str().ok()) {
            Some("Bearer token-for-a") => Ok(Principal::new("client-a")),
            Some("Bearer token-for-b") => Ok(Principal::new("client-b")),
            _ => Err(A2aError::Auth { reason: "unknown token".into() }),
        }
    }
}

// A2aServer::new(card).with_handler(Echo).with_authenticator(Tokens)
```

With an authenticator, every JSON-RPC request is authenticated before its body is parsed. Bad or missing credentials get HTTP 401. `Authenticator::authorize` can refuse a method per caller (HTTP 403). Each task is bound to the principal that created it, so `GetTask`, `ListTasks`, `CancelTask`, `SubscribeToTask` and follow-up messages only see the caller's own tasks. Another caller's task id gets `TaskNotFoundError`, as A2A v1.0 section 13.1 requires.

With the `federation` feature, `FederationPolicy` implements `Authenticator` with Bearer tokens and per-method scopes. It also works as an Axum layer.

Other rules the server follows:

- Task ids are generated by the server. A message with a `taskId` continues that task and must name a task the caller can see; otherwise the server returns `TaskNotFoundError`.
- Completed, failed, canceled and rejected tasks are final. Messages to them get `UnsupportedOperationError`, and streams close when a task reaches one of these states.
- `InMemoryTaskStore` is bounded: 10,000 tasks and a one hour idle TTL by default (`StoreLimits`).
- `A2aClient` times out: 10 s to connect, 60 s per request, and 5 min of silence on a stream by default.

## A2A TCK Conformance

Tested against the official [A2A Technology Compatibility Kit](https://github.com/a2aproject/a2a-tck) (mandatory category):

| Category | Passed | Failed | Skipped |
|----------|--------|--------|---------|
| Mandatory | 75 | 1* | 40 |

\* The single failure is a [bug in the TCK itself](https://github.com/a2aproject/a2a-tck), not in this SDK.

In the optional quality category, `test_partial_update_recovery` fails on purpose: it keeps sending messages to a task after it completed, and A2A v1.0 requires the server to reject those.

The server accepts both v1.0 method names (`SendMessage`, `GetTask`) and v0.3 aliases (`message/send`, `tasks/get`) for backward compatibility.

## Two Crates

This workspace ships two crates:

| Crate | Purpose |
|-------|---------|
| [`jamjet-a2a-types`](jamjet-a2a-types/) | Pure A2A v1.0 types (`Task`, `Message`, `Part`, `AgentCard`, etc.). Zero I/O dependencies — use it when you only need the data model. |
| [`jamjet-a2a`](jamjet-a2a/) | Full SDK — client, server, task store, federation auth, coordinator, and MCP bridge. Depends on `jamjet-a2a-types`. |

Add only the types crate if you are building your own transport layer:

```toml
[dependencies]
jamjet-a2a-types = "0.2"
```

Or use the full SDK with the features you need:

```toml
[dependencies]
jamjet-a2a = { version = "0.2", features = ["client", "server", "coordinator"] }
```

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

at your option.
