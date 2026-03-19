//! JamJet-specific Agent Card extensions for A2A v1.0.

use serde::{Deserialize, Serialize};

// ────────────────────────────────────────────────────────────────────────────
// LatencyClass
// ────────────────────────────────────────────────────────────────────────────

/// Expected latency tier for an agent's responses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LatencyClass {
    Realtime,
    Fast,
    Medium,
    Slow,
}

// ────────────────────────────────────────────────────────────────────────────
// CostClass
// ────────────────────────────────────────────────────────────────────────────

/// Expected cost tier for invoking an agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CostClass {
    Free,
    Low,
    Medium,
    High,
}

// ────────────────────────────────────────────────────────────────────────────
// AutonomyLevel
// ────────────────────────────────────────────────────────────────────────────

/// How autonomously the agent operates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AutonomyLevel {
    Deterministic,
    Guided,
    BoundedAutonomous,
    FullyAutonomous,
}

// ────────────────────────────────────────────────────────────────────────────
// AutonomyConstraints
// ────────────────────────────────────────────────────────────────────────────

/// Constraints that bound an agent's autonomous behaviour.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutonomyConstraints {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_iterations: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_budget_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_delegations: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub require_approval_for: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_budget_secs: Option<u64>,
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_class_round_trip() {
        for class in [
            LatencyClass::Realtime,
            LatencyClass::Fast,
            LatencyClass::Medium,
            LatencyClass::Slow,
        ] {
            let json = serde_json::to_value(&class).unwrap();
            let back: LatencyClass = serde_json::from_value(json).unwrap();
            assert_eq!(back, class);
        }
    }
}
