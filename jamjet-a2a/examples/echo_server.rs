//! Minimal A2A echo server.
//!
//! Run with: cargo run --example echo_server

use jamjet_a2a::server::{A2aServer, TaskHandler};
use jamjet_a2a::store::TaskStore;
use jamjet_a2a_types::*;
use std::sync::Arc;

/// A handler that echoes the first text part back to the caller.
struct EchoHandler;

#[async_trait::async_trait]
impl TaskHandler for EchoHandler {
    async fn handle(
        &self,
        task_id: String,
        message: Message,
        store: Arc<dyn TaskStore>,
    ) -> Result<(), A2aError> {
        let text = message
            .parts
            .iter()
            .find_map(|p| match &p.content {
                PartContent::Text(t) => Some(t.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "(no text)".to_string());

        store
            .update_status(
                &task_id,
                TaskStatus {
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
                },
            )
            .await?;

        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), A2aError> {
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);

    let card = AgentCard {
        name: "echo".into(),
        description: "A minimal echo agent".into(),
        version: "1.0".into(),
        supported_interfaces: vec![AgentInterface {
            url: format!("http://localhost:{port}"),
            protocol_binding: "jsonrpc".into(),
            tenant: None,
            protocol_version: "1.0".into(),
        }],
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
            description: "Echoes input back to the caller".into(),
            ..Default::default()
        }],
        provider: None,
        security_schemes: Default::default(),
        security_requirements: vec![],
        signatures: vec![],
        icon_url: None,
    };

    println!("Starting echo server on http://0.0.0.0:{port}");
    println!("Agent card: http://localhost:{port}/.well-known/agent-card.json");

    A2aServer::new(card)
        .with_handler(EchoHandler)
        .with_port(port)
        .start()
        .await
}
