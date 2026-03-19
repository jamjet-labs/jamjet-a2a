//! Standalone Rust SDK for the A2A protocol.
pub use jamjet_a2a_types as types;

#[cfg(feature = "client")]
pub mod client;
#[cfg(feature = "client")]
pub use client::A2aClient;
