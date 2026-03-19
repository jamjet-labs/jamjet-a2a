//! Coordinator strategy for multi-agent routing and selection.
//!
//! Provides a 5-dimension scoring system for selecting the best agent
//! from a set of candidates based on capability fit, cost, latency,
//! trust compatibility, and historical performance.

use jamjet_a2a_types::*;
use serde::{Deserialize, Serialize};
use tracing::debug;

// ────────────────────────────────────────────────────────────────────────────
// CoordinatorStrategy trait
// ────────────────────────────────────────────────────────────────────────────

/// A strategy for scoring and selecting agents.
pub trait CoordinatorStrategy: Send + Sync {
    /// Score all candidate agents for a given task message.
    fn score(&self, task: &Message, candidates: &[AgentCard]) -> Vec<AgentScore>;
}

// ────────────────────────────────────────────────────────────────────────────
// Types
// ────────────────────────────────────────────────────────────────────────────

/// Per-dimension scores (each 0.0–1.0).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DimensionScores {
    pub capability_fit: f64,
    pub cost_fit: f64,
    pub latency_fit: f64,
    pub trust_compatibility: f64,
    pub historical_performance: f64,
}

/// Weights for each scoring dimension.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DimensionWeights {
    pub capability_fit: f64,
    pub cost_fit: f64,
    pub latency_fit: f64,
    pub trust_compatibility: f64,
    pub historical_performance: f64,
}

impl Default for DimensionWeights {
    fn default() -> Self {
        Self {
            capability_fit: 1.0,
            cost_fit: 1.0,
            latency_fit: 1.0,
            trust_compatibility: 1.0,
            historical_performance: 0.5,
        }
    }
}

/// A scored agent candidate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentScore {
    pub card: AgentCard,
    pub total_score: f64,
    pub dimensions: DimensionScores,
    pub reasons: Vec<String>,
}

/// The coordinator's final selection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoordinatorDecision {
    pub selected: AgentCard,
    pub score: AgentScore,
    pub rejected: Vec<RejectedAgent>,
    pub method: DecisionMethod,
}

/// An agent that was not selected, with the reason.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RejectedAgent {
    pub card: AgentCard,
    pub score: AgentScore,
    pub reason: String,
}

/// How the decision was made.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum DecisionMethod {
    TopScore,
    TiebreakRandom,
    SingleCandidate,
    NoCandidates,
}

// ────────────────────────────────────────────────────────────────────────────
// DefaultCoordinatorStrategy
// ────────────────────────────────────────────────────────────────────────────

/// Default scoring strategy using keyword matching and extension metadata.
pub struct DefaultCoordinatorStrategy {
    weights: DimensionWeights,
}

impl DefaultCoordinatorStrategy {
    /// Create a new strategy with default weights.
    pub fn new() -> Self {
        Self {
            weights: DimensionWeights::default(),
        }
    }

    /// Create a strategy with custom dimension weights.
    pub fn with_weights(weights: DimensionWeights) -> Self {
        Self { weights }
    }

    /// Extract keywords from message parts for capability matching.
    fn extract_keywords(task: &Message) -> Vec<String> {
        let mut keywords = Vec::new();
        for part in &task.parts {
            if let PartContent::Text(text) = &part.content {
                for word in text.split_whitespace() {
                    let cleaned = word
                        .trim_matches(|c: char| !c.is_alphanumeric())
                        .to_lowercase();
                    if cleaned.len() >= 2 {
                        keywords.push(cleaned);
                    }
                }
            }
        }
        keywords
    }

    /// Score capability fit by checking skill name/description keyword overlap.
    fn score_capability(card: &AgentCard, keywords: &[String]) -> (f64, Vec<String>) {
        if keywords.is_empty() || card.skills.is_empty() {
            return (0.5, vec!["no keywords or skills to match".into()]);
        }

        let mut matches = 0usize;
        let mut reasons = Vec::new();
        for skill in &card.skills {
            let name_lower = skill.name.to_lowercase();
            let desc_lower = skill.description.to_lowercase();
            for keyword in keywords {
                if name_lower.contains(keyword) || desc_lower.contains(keyword) {
                    matches += 1;
                    reasons.push(format!(
                        "skill '{}' matches keyword '{}'",
                        skill.name, keyword
                    ));
                    break; // Count each skill at most once
                }
            }
        }

        let score = if card.skills.is_empty() {
            0.5
        } else {
            (matches as f64 / card.skills.len() as f64).min(1.0)
        };
        (score, reasons)
    }

    /// Score cost fit based on CostClass extension.
    fn score_cost(card: &AgentCard) -> f64 {
        for ext in &card.capabilities.extensions {
            if ext.uri.contains("cost_class") || ext.uri.contains("costClass") {
                if let Some(params) = &ext.params {
                    if let Some(class_str) = params.as_str() {
                        if let Ok(class) = serde_json::from_value::<CostClass>(
                            serde_json::Value::String(class_str.to_string()),
                        ) {
                            return match class {
                                CostClass::Free => 1.0,
                                CostClass::Low => 0.75,
                                CostClass::Medium => 0.5,
                                CostClass::High => 0.25,
                                _ => 0.5,
                            };
                        }
                    }
                }
            }
        }
        0.5 // No cost info
    }

    /// Score latency fit based on LatencyClass extension.
    fn score_latency(card: &AgentCard) -> f64 {
        for ext in &card.capabilities.extensions {
            if ext.uri.contains("latency_class") || ext.uri.contains("latencyClass") {
                if let Some(params) = &ext.params {
                    if let Some(class_str) = params.as_str() {
                        if let Ok(class) = serde_json::from_value::<LatencyClass>(
                            serde_json::Value::String(class_str.to_string()),
                        ) {
                            return match class {
                                LatencyClass::Realtime => 1.0,
                                LatencyClass::Fast => 0.75,
                                LatencyClass::Medium => 0.5,
                                LatencyClass::Slow => 0.25,
                                _ => 0.5,
                            };
                        }
                    }
                }
            }
        }
        0.5 // No latency info
    }

    /// Compute the total weighted score for a single agent.
    fn score_agent(&self, card: &AgentCard, keywords: &[String]) -> AgentScore {
        let (capability_fit, reasons) = Self::score_capability(card, keywords);
        let cost_fit = Self::score_cost(card);
        let latency_fit = Self::score_latency(card);
        let trust_compatibility = 0.8; // Default — full trust scoring requires runtime
        let historical_performance = 0.5; // No history in standalone

        let dimensions = DimensionScores {
            capability_fit,
            cost_fit,
            latency_fit,
            trust_compatibility,
            historical_performance,
        };

        let w = &self.weights;
        let weight_sum = w.capability_fit
            + w.cost_fit
            + w.latency_fit
            + w.trust_compatibility
            + w.historical_performance;

        let total_score = if weight_sum > 0.0 {
            (dimensions.capability_fit * w.capability_fit
                + dimensions.cost_fit * w.cost_fit
                + dimensions.latency_fit * w.latency_fit
                + dimensions.trust_compatibility * w.trust_compatibility
                + dimensions.historical_performance * w.historical_performance)
                / weight_sum
        } else {
            0.0
        };

        AgentScore {
            card: card.clone(),
            total_score,
            dimensions,
            reasons,
        }
    }
}

impl Default for DefaultCoordinatorStrategy {
    fn default() -> Self {
        Self::new()
    }
}

impl CoordinatorStrategy for DefaultCoordinatorStrategy {
    fn score(&self, task: &Message, candidates: &[AgentCard]) -> Vec<AgentScore> {
        let keywords = Self::extract_keywords(task);
        candidates
            .iter()
            .map(|card| self.score_agent(card, &keywords))
            .collect()
    }
}

// ────────────────────────────────────────────────────────────────────────────
// select_agent
// ────────────────────────────────────────────────────────────────────────────

/// Discover agents at the given URLs, score them, and select the best.
pub async fn select_agent(
    client: &crate::client::A2aClient,
    urls: &[&str],
    task: &Message,
    strategy: &dyn CoordinatorStrategy,
) -> Result<CoordinatorDecision, A2aError> {
    // Discover all agent cards.
    let mut candidates = Vec::new();
    for url in urls {
        match client.discover(url).await {
            Ok(card) => candidates.push(card),
            Err(e) => {
                debug!(url, error = %e, "failed to discover agent, skipping");
            }
        }
    }

    if candidates.is_empty() {
        return Err(A2aError::Auth {
            reason: "no candidates available for selection".into(),
        });
    }

    let scores = strategy.score(task, &candidates);

    if scores.is_empty() {
        return Err(A2aError::Auth {
            reason: "no candidates available for selection".into(),
        });
    }

    if scores.len() == 1 {
        let score = scores.into_iter().next().unwrap();
        let selected = score.card.clone();
        return Ok(CoordinatorDecision {
            selected,
            score,
            rejected: vec![],
            method: DecisionMethod::SingleCandidate,
        });
    }

    // Find the best score.
    let mut sorted = scores;
    sorted.sort_by(|a, b| {
        b.total_score
            .partial_cmp(&a.total_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let best = sorted.remove(0);
    let selected = best.card.clone();

    let rejected: Vec<RejectedAgent> = sorted
        .into_iter()
        .map(|s| {
            let reason = format!(
                "score {:.3} < best {:.3}",
                s.total_score, best.total_score
            );
            RejectedAgent {
                card: s.card.clone(),
                score: s,
                reason,
            }
        })
        .collect();

    Ok(CoordinatorDecision {
        selected,
        score: best,
        rejected,
        method: DecisionMethod::TopScore,
    })
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_card(name: &str, skills: Vec<AgentSkill>) -> AgentCard {
        AgentCard {
            name: name.into(),
            description: format!("{name} agent"),
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

    fn make_message(text: &str) -> Message {
        Message {
            message_id: "msg-1".into(),
            context_id: None,
            task_id: None,
            role: Role::User,
            parts: vec![Part {
                content: PartContent::Text(text.into()),
                metadata: None,
                filename: None,
                media_type: None,
            }],
            metadata: None,
            extensions: vec![],
            reference_task_ids: vec![],
        }
    }

    #[test]
    fn scores_skill_match_highest() {
        let strategy = DefaultCoordinatorStrategy::new();

        let summarize_card = make_card(
            "summarizer",
            vec![AgentSkill {
                id: "s1".into(),
                name: "summarize".into(),
                description: "Summarize text documents".into(),
                ..Default::default()
            }],
        );
        let translate_card = make_card(
            "translator",
            vec![AgentSkill {
                id: "s2".into(),
                name: "translate".into(),
                description: "Translate between languages".into(),
                ..Default::default()
            }],
        );

        let task = make_message("Please summarize this document");
        let scores = strategy.score(&task, &[summarize_card, translate_card]);

        assert_eq!(scores.len(), 2);
        // The summarizer should score higher on capability_fit.
        assert!(
            scores[0].dimensions.capability_fit > scores[1].dimensions.capability_fit,
            "summarizer ({}) should beat translator ({}) on capability_fit",
            scores[0].dimensions.capability_fit,
            scores[1].dimensions.capability_fit
        );
        assert!(scores[0].total_score > scores[1].total_score);
    }

    #[test]
    fn empty_candidates_returns_error() {
        let strategy = DefaultCoordinatorStrategy::new();
        let task = make_message("do something");
        let scores = strategy.score(&task, &[]);
        assert!(scores.is_empty());
    }

    #[test]
    fn single_candidate_returns_it() {
        let strategy = DefaultCoordinatorStrategy::new();
        let card = make_card(
            "only-one",
            vec![AgentSkill {
                id: "s1".into(),
                name: "anything".into(),
                description: "Does anything".into(),
                ..Default::default()
            }],
        );

        let task = make_message("do something");
        let scores = strategy.score(&task, &[card]);
        assert_eq!(scores.len(), 1);
        assert!(scores[0].total_score > 0.0);
    }

    #[test]
    fn custom_weights_affect_scoring() {
        let heavy_capability = DefaultCoordinatorStrategy::with_weights(DimensionWeights {
            capability_fit: 10.0,
            cost_fit: 0.0,
            latency_fit: 0.0,
            trust_compatibility: 0.0,
            historical_performance: 0.0,
        });

        let matching_card = make_card(
            "matcher",
            vec![AgentSkill {
                id: "s1".into(),
                name: "summarize".into(),
                description: "Summarize text".into(),
                ..Default::default()
            }],
        );
        let non_matching_card = make_card(
            "other",
            vec![AgentSkill {
                id: "s2".into(),
                name: "translate".into(),
                description: "Translate languages".into(),
                ..Default::default()
            }],
        );

        let task = make_message("Please summarize");
        let scores = heavy_capability.score(&task, &[matching_card, non_matching_card]);

        // With all weight on capability_fit, the matching agent should dominate.
        let matcher_score = scores.iter().find(|s| s.card.name == "matcher").unwrap();
        let other_score = scores.iter().find(|s| s.card.name == "other").unwrap();
        assert!(
            matcher_score.total_score > other_score.total_score,
            "matcher ({}) should beat other ({}) with heavy capability weight",
            matcher_score.total_score,
            other_score.total_score
        );
    }
}
