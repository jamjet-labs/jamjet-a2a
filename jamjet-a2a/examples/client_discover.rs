//! Discover an A2A agent and print its card.
//!
//! Run with: cargo run --example client_discover -- https://agent.example.com

use jamjet_a2a::client::A2aClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "http://localhost:3000".to_string());

    println!("Discovering agent at {url} ...");

    let client = A2aClient::new();
    let card = client.discover(&url).await?;

    println!("Agent: {} v{}", card.name, card.version);
    println!("Description: {}", card.description);
    println!("Input modes: {:?}", card.default_input_modes);
    println!("Output modes: {:?}", card.default_output_modes);
    println!(
        "Streaming: {}",
        card.capabilities.streaming.unwrap_or(false)
    );

    if !card.skills.is_empty() {
        println!("Skills:");
        for skill in &card.skills {
            println!("  - {} ({}): {}", skill.name, skill.id, skill.description);
        }
    }

    Ok(())
}
