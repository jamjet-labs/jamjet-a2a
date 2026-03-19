//! Agent Card types for A2A v1.0.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

use crate::security::{SecurityRequirement, SecurityScheme};

// ────────────────────────────────────────────────────────────────────────────
// AgentCard
// ────────────────────────────────────────────────────────────────────────────

/// The top-level Agent Card describing an agent's identity, capabilities,
/// skills, and security requirements.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCard {
    pub name: String,
    pub description: String,
    pub version: String,
    pub supported_interfaces: Vec<AgentInterface>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<AgentProvider>,
    pub capabilities: AgentCapabilities,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub security_schemes: HashMap<String, SecurityScheme>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub security_requirements: Vec<SecurityRequirement>,
    pub default_input_modes: Vec<String>,
    pub default_output_modes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<AgentSkill>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub signatures: Vec<AgentCardSignature>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_url: Option<String>,
}

// ────────────────────────────────────────────────────────────────────────────
// AgentInterface
// ────────────────────────────────────────────────────────────────────────────

/// A protocol binding that the agent exposes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentInterface {
    pub url: String,
    pub protocol_binding: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    pub protocol_version: String,
}

// ────────────────────────────────────────────────────────────────────────────
// AgentSkill
// ────────────────────────────────────────────────────────────────────────────

/// A skill that an agent can perform.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSkill {
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_modes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_modes: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub security_requirements: Vec<SecurityRequirement>,
}

// ────────────────────────────────────────────────────────────────────────────
// AgentCapabilities
// ────────────────────────────────────────────────────────────────────────────

/// Capabilities that an agent advertises.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub streaming: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub push_notifications: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<AgentExtension>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extended_agent_card: Option<bool>,
}

// ────────────────────────────────────────────────────────────────────────────
// AgentExtension
// ────────────────────────────────────────────────────────────────────────────

/// A protocol extension that an agent supports.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentExtension {
    pub uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

// ────────────────────────────────────────────────────────────────────────────
// AgentProvider
// ────────────────────────────────────────────────────────────────────────────

/// The organization or entity that provides the agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentProvider {
    pub url: String,
    pub organization: String,
}

// ────────────────────────────────────────────────────────────────────────────
// AgentCardSignature
// ────────────────────────────────────────────────────────────────────────────

/// A JWS signature attached to the Agent Card.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCardSignature {
    pub protected: String,
    pub signature: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header: Option<Value>,
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_card_minimal_round_trip() {
        let card = AgentCard {
            name: "test-agent".into(),
            description: "A test agent".into(),
            version: "1.0".into(),
            supported_interfaces: vec![AgentInterface {
                url: "https://agent.example.com".into(),
                protocol_binding: "JSONRPC".into(),
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
            skills: vec![],
            provider: None,
            security_schemes: Default::default(),
            security_requirements: vec![],
            signatures: vec![],
            icon_url: None,
        };
        let json = serde_json::to_string(&card).unwrap();
        let back: AgentCard = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "test-agent");
        assert_eq!(back.supported_interfaces.len(), 1);
    }

    #[test]
    fn agent_card_with_skills() {
        let card = AgentCard {
            name: "skilled-agent".into(),
            description: "Agent with skills".into(),
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
            skills: vec![AgentSkill {
                id: "s1".into(),
                name: "summarize".into(),
                description: "Summarize text".into(),
                tags: vec!["nlp".into()],
                ..Default::default()
            }],
            provider: Some(AgentProvider {
                url: "https://example.com".into(),
                organization: "Test Org".into(),
            }),
            security_schemes: Default::default(),
            security_requirements: vec![],
            signatures: vec![],
            icon_url: None,
        };
        let json = serde_json::to_value(&card).unwrap();
        assert_eq!(json["skills"][0]["name"], "summarize");
        assert_eq!(json["provider"]["organization"], "Test Org");
    }
}
