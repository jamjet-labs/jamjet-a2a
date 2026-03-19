//! Bidirectional mapping between A2A Agent Cards and MCP tool definitions.
//!
//! This module provides conversion functions to bridge the A2A and MCP
//! protocols without depending on any external MCP crate.

use jamjet_a2a_types::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

// ────────────────────────────────────────────────────────────────────────────
// McpToolDefinition
// ────────────────────────────────────────────────────────────────────────────

/// A minimal MCP tool definition, independent of any external MCP crate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolDefinition {
    /// Tool name (maps to skill name/id).
    pub name: String,
    /// Optional human-readable description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema describing the tool's input parameters.
    pub input_schema: Value,
}

// ────────────────────────────────────────────────────────────────────────────
// Conversion functions
// ────────────────────────────────────────────────────────────────────────────

/// Convert an Agent Card's skills into MCP tool definitions.
///
/// Each [`AgentSkill`] becomes one [`McpToolDefinition`], using the skill's
/// `name` as the tool name, `description` as the tool description, and a
/// simple JSON Schema derived from the skill's input modes and tags.
pub fn agent_card_to_mcp_tools(card: &AgentCard) -> Vec<McpToolDefinition> {
    card.skills
        .iter()
        .map(|skill| {
            // Build a minimal JSON Schema for the tool input.
            let input_schema = serde_json::json!({
                "type": "object",
                "properties": {
                    "message": {
                        "type": "string",
                        "description": format!("Input for {}", skill.name),
                    }
                },
                "required": ["message"],
            });

            McpToolDefinition {
                name: skill.name.clone(),
                description: if skill.description.is_empty() {
                    None
                } else {
                    Some(skill.description.clone())
                },
                input_schema,
            }
        })
        .collect()
}

/// Convert MCP tool definitions into an Agent Card.
///
/// Creates a minimal [`AgentCard`] where each tool becomes an [`AgentSkill`].
pub fn mcp_tools_to_agent_card(tools: &[McpToolDefinition], name: &str) -> AgentCard {
    let skills: Vec<AgentSkill> = tools
        .iter()
        .enumerate()
        .map(|(i, tool)| AgentSkill {
            id: format!("mcp-tool-{i}"),
            name: tool.name.clone(),
            description: tool.description.clone().unwrap_or_default(),
            tags: vec!["mcp".into()],
            ..Default::default()
        })
        .collect();

    AgentCard {
        name: name.into(),
        description: format!("Agent bridged from {n} MCP tool(s)", n = tools.len()),
        version: "1.0".into(),
        supported_interfaces: vec![],
        capabilities: AgentCapabilities {
            streaming: None,
            push_notifications: None,
            extensions: vec![],
            extended_agent_card: None,
        },
        default_input_modes: vec!["text/plain".into()],
        default_output_modes: vec!["text/plain".into()],
        skills,
        provider: None,
        security_schemes: HashMap::new(),
        security_requirements: vec![],
        signatures: vec![],
        icon_url: None,
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_card_with_skills(skill_names: &[&str]) -> AgentCard {
        let skills = skill_names
            .iter()
            .enumerate()
            .map(|(i, name)| AgentSkill {
                id: format!("s{i}"),
                name: name.to_string(),
                description: format!("{name} skill"),
                ..Default::default()
            })
            .collect();

        AgentCard {
            name: "test-agent".into(),
            description: "Test agent".into(),
            version: "1.0".into(),
            supported_interfaces: vec![],
            capabilities: AgentCapabilities {
                streaming: None,
                push_notifications: None,
                extensions: vec![],
                extended_agent_card: None,
            },
            default_input_modes: vec!["text/plain".into()],
            default_output_modes: vec!["text/plain".into()],
            skills,
            provider: None,
            security_schemes: HashMap::new(),
            security_requirements: vec![],
            signatures: vec![],
            icon_url: None,
        }
    }

    #[test]
    fn card_with_two_skills_produces_two_tools() {
        let card = make_card_with_skills(&["summarize", "translate"]);
        let tools = agent_card_to_mcp_tools(&card);
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "summarize");
        assert_eq!(tools[1].name, "translate");
        assert!(tools[0].description.is_some());
        assert!(tools[1].description.is_some());
    }

    #[test]
    fn round_trip_card_to_tools_to_card() {
        let original = make_card_with_skills(&["search", "classify", "generate"]);
        let tools = agent_card_to_mcp_tools(&original);
        let rebuilt = mcp_tools_to_agent_card(&tools, "rebuilt-agent");

        // Skill names should be preserved.
        let original_names: Vec<&str> = original.skills.iter().map(|s| s.name.as_str()).collect();
        let rebuilt_names: Vec<&str> = rebuilt.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(original_names, rebuilt_names);
    }

    #[test]
    fn empty_skills_produces_empty_tools() {
        let card = make_card_with_skills(&[]);
        let tools = agent_card_to_mcp_tools(&card);
        assert!(tools.is_empty());
    }
}
