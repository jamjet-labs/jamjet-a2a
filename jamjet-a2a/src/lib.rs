//! Standalone Rust SDK for the A2A protocol.
pub use jamjet_a2a_types as types;

#[cfg(feature = "client")]
pub mod client;
#[cfg(feature = "client")]
pub use client::A2aClient;

#[cfg(feature = "server")]
pub mod store;
#[cfg(feature = "server")]
pub mod server;

#[cfg(feature = "server")]
pub use store::{TaskStore, InMemoryTaskStore};
#[cfg(feature = "server")]
pub use server::{A2aServer, TaskHandler};

#[cfg(feature = "federation")]
pub mod federation;

#[cfg(feature = "coordinator")]
pub mod coordinator;

#[cfg(feature = "mcp-bridge")]
pub mod mcp_bridge;
