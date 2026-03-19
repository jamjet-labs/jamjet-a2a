//! A2A federation auth and mTLS configuration.
//!
//! - **Authorization middleware**: validates Bearer tokens on the A2A server,
//!   enforces capability-scoped access control, and logs federation events.
//! - **mTLS configuration**: TLS settings for cross-org federation with
//!   mutual certificate authentication.

use axum::{
    body::Body,
    extract::Request,
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use jamjet_a2a_types::A2aError;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use tracing::{info, warn};

// ── Federation auth policy ──────────────────────────────────────────────────

/// Authorization policy for incoming A2A requests.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FederationPolicy {
    /// If true, all incoming requests require a valid Bearer token.
    #[serde(default)]
    pub require_auth: bool,

    /// Allowed Bearer tokens mapped to their granted scopes.
    #[serde(default)]
    pub tokens: Vec<FederationToken>,

    /// If true, the Agent Card endpoint is public (no auth required).
    #[serde(default = "default_true")]
    pub public_agent_card: bool,

    /// Allowed caller agent IDs (if empty, all authenticated callers are allowed).
    #[serde(default)]
    pub allowed_agents: Vec<String>,

    /// Scopes required per RPC method. Maps v1.0 method names
    /// ("SendMessage", "GetTask", etc.) to required scopes.
    #[serde(default)]
    pub method_scopes: HashMap<String, Vec<String>>,
}

fn default_true() -> bool {
    true
}

/// A federation token with associated metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FederationToken {
    /// The token value (matched against Bearer header).
    pub token: String,
    /// Human-readable name for audit logging.
    pub name: String,
    /// Agent ID of the token holder.
    pub agent_id: Option<String>,
    /// Granted scopes (e.g., "read", "write").
    #[serde(default)]
    pub scopes: Vec<String>,
}

/// Result of federation auth validation.
#[derive(Debug, Clone)]
pub struct FederationIdentity {
    /// Name of the authenticated token.
    pub token_name: String,
    /// Agent ID of the caller, if known.
    pub agent_id: Option<String>,
    /// Granted scopes.
    pub scopes: HashSet<String>,
}

/// Validate a Bearer token against the federation policy.
pub fn validate_federation_token(
    headers: &HeaderMap,
    policy: &FederationPolicy,
) -> Result<FederationIdentity, A2aError> {
    let auth_header = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| A2aError::Auth {
            reason: "missing authorization header".into(),
        })?;

    let token = auth_header
        .strip_prefix("Bearer ")
        .ok_or_else(|| A2aError::Auth {
            reason: "authorization must be Bearer token".into(),
        })?;

    let found = policy
        .tokens
        .iter()
        .find(|t| t.token == token)
        .ok_or_else(|| A2aError::Auth {
            reason: "invalid token".into(),
        })?;

    // Check agent ID allowlist.
    if !policy.allowed_agents.is_empty() {
        if let Some(agent_id) = &found.agent_id {
            if !policy.allowed_agents.contains(agent_id) {
                return Err(A2aError::Auth {
                    reason: "agent not in allowlist".into(),
                });
            }
        } else {
            return Err(A2aError::Auth {
                reason: "token has no agent_id and allowlist is active".into(),
            });
        }
    }

    Ok(FederationIdentity {
        token_name: found.name.clone(),
        agent_id: found.agent_id.clone(),
        scopes: found.scopes.iter().cloned().collect(),
    })
}

/// Check if an identity has the required scopes for a given RPC method.
///
/// Method names use v1.0 convention: "SendMessage", "GetTask", etc.
pub fn check_method_scopes(
    identity: &FederationIdentity,
    method: &str,
    policy: &FederationPolicy,
) -> bool {
    if let Some(required) = policy.method_scopes.get(method) {
        if required.is_empty() {
            return true;
        }
        required.iter().any(|s| identity.scopes.contains(s))
    } else {
        // No specific scope requirement for this method — allow.
        true
    }
}

/// Axum middleware layer for federation auth.
///
/// Insert into the A2A server router to require authentication:
/// ```ignore
/// use axum::middleware;
/// router.layer(middleware::from_fn_with_state(policy, federation_auth_layer))
/// ```
pub async fn federation_auth_layer(
    axum::extract::State(policy): axum::extract::State<FederationPolicy>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let path = request.uri().path().to_string();

    // Allow public access to Agent Card if configured (v1.0 well-known path).
    if policy.public_agent_card && path.contains(".well-known/agent-card.json") {
        return next.run(request).await;
    }

    if !policy.require_auth {
        return next.run(request).await;
    }

    match validate_federation_token(request.headers(), &policy) {
        Ok(identity) => {
            info!(
                token_name = %identity.token_name,
                agent_id = ?identity.agent_id,
                path = %path,
                "A2A federation auth: authorized"
            );
            next.run(request).await
        }
        Err(e) => {
            warn!(
                error = %e,
                path = %path,
                "A2A federation auth: rejected"
            );
            (
                StatusCode::UNAUTHORIZED,
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "error": { "code": -32003, "message": e.to_string() }
                })
                .to_string(),
            )
                .into_response()
        }
    }
}

// ── mTLS configuration ──────────────────────────────────────────────────────

/// TLS / mTLS configuration for A2A federation.
///
/// Used by both the A2A server (to require client certificates) and the A2A
/// client (to present client certificates when connecting to federated agents).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TlsConfig {
    /// Enable TLS. If false, all other fields are ignored.
    #[serde(default)]
    pub enabled: bool,

    /// Path to the server/client certificate (PEM).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cert_path: Option<PathBuf>,

    /// Path to the private key (PEM).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_path: Option<PathBuf>,

    /// Path to the CA certificate bundle for verifying peer certificates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_cert_path: Option<PathBuf>,

    /// If true, the server requires client certificates (mTLS).
    #[serde(default)]
    pub require_client_cert: bool,

    /// Allowed Common Names (CNs) from client certificates.
    #[serde(default)]
    pub allowed_cns: Vec<String>,
}

impl TlsConfig {
    /// Load the certificate and key as raw bytes.
    pub fn load_cert_key(&self) -> Result<(Vec<u8>, Vec<u8>), A2aError> {
        let cert = std::fs::read(self.cert_path.as_ref().ok_or(A2aError::Auth {
            reason: "cert_path not configured".into(),
        })?)
        .map_err(|e| A2aError::Auth {
            reason: format!("failed to read cert: {e}"),
        })?;

        let key = std::fs::read(self.key_path.as_ref().ok_or(A2aError::Auth {
            reason: "key_path not configured".into(),
        })?)
        .map_err(|e| A2aError::Auth {
            reason: format!("failed to read key: {e}"),
        })?;

        Ok((cert, key))
    }

    /// Load the CA certificate as raw bytes (for client cert verification).
    pub fn load_ca_cert(&self) -> Result<Vec<u8>, A2aError> {
        std::fs::read(self.ca_cert_path.as_ref().ok_or(A2aError::Auth {
            reason: "ca_cert_path not configured".into(),
        })?)
        .map_err(|e| A2aError::Auth {
            reason: format!("failed to read CA cert: {e}"),
        })
    }
}

// ── reqwest client builder with mTLS ────────────────────────────────────────

/// Build a `reqwest::Client` with mTLS configuration for outbound A2A calls.
///
/// Used by `A2aClient` when connecting to federated agents that require mTLS.
pub fn build_mtls_client(tls: &TlsConfig) -> Result<reqwest::Client, A2aError> {
    if !tls.enabled {
        return Ok(reqwest::Client::new());
    }

    let (cert_pem, key_pem) = tls.load_cert_key()?;
    let identity =
        reqwest::Identity::from_pkcs8_pem(&cert_pem, &key_pem).map_err(|e| A2aError::Auth {
            reason: format!("invalid identity PEM: {e}"),
        })?;

    let mut builder = reqwest::Client::builder().identity(identity);

    if let Ok(ca_pem) = tls.load_ca_cert() {
        let ca = reqwest::Certificate::from_pem(&ca_pem).map_err(|e| A2aError::Auth {
            reason: format!("invalid CA cert: {e}"),
        })?;
        builder = builder.add_root_certificate(ca);
    }

    builder.build().map_err(|e| A2aError::Auth {
        reason: format!("failed to build mTLS client: {e}"),
    })
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn test_policy() -> FederationPolicy {
        FederationPolicy {
            require_auth: true,
            tokens: vec![
                FederationToken {
                    token: "tok-alpha".to_string(),
                    name: "Alpha Agent".to_string(),
                    agent_id: Some("agent-alpha".to_string()),
                    scopes: vec!["read".to_string(), "write".to_string()],
                },
                FederationToken {
                    token: "tok-readonly".to_string(),
                    name: "Read-Only Agent".to_string(),
                    agent_id: Some("agent-ro".to_string()),
                    scopes: vec!["read".to_string()],
                },
            ],
            public_agent_card: true,
            allowed_agents: vec![],
            method_scopes: [
                ("SendMessage".to_string(), vec!["write".to_string()]),
                ("GetTask".to_string(), vec!["read".to_string()]),
            ]
            .into_iter()
            .collect(),
        }
    }

    fn headers_with_token(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            "authorization",
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        h
    }

    #[test]
    fn valid_token_authenticates() {
        let policy = test_policy();
        let headers = headers_with_token("tok-alpha");
        let identity = validate_federation_token(&headers, &policy).unwrap();
        assert_eq!(identity.token_name, "Alpha Agent");
        assert_eq!(identity.agent_id, Some("agent-alpha".to_string()));
        assert!(identity.scopes.contains("write"));
    }

    #[test]
    fn invalid_token_rejected() {
        let policy = test_policy();
        let headers = headers_with_token("tok-invalid");
        assert!(validate_federation_token(&headers, &policy).is_err());
    }

    #[test]
    fn missing_auth_header_rejected() {
        let policy = test_policy();
        let headers = HeaderMap::new();
        assert!(validate_federation_token(&headers, &policy).is_err());
    }

    #[test]
    fn scope_check_for_method() {
        let policy = test_policy();
        let headers = headers_with_token("tok-readonly");
        let identity = validate_federation_token(&headers, &policy).unwrap();

        // Read-only token can GetTask but not SendMessage.
        assert!(check_method_scopes(&identity, "GetTask", &policy));
        assert!(!check_method_scopes(&identity, "SendMessage", &policy));
    }

    #[test]
    fn write_token_has_all_scopes() {
        let policy = test_policy();
        let headers = headers_with_token("tok-alpha");
        let identity = validate_federation_token(&headers, &policy).unwrap();

        assert!(check_method_scopes(&identity, "SendMessage", &policy));
        assert!(check_method_scopes(&identity, "GetTask", &policy));
    }

    #[test]
    fn agent_allowlist_restricts_access() {
        let mut policy = test_policy();
        policy.allowed_agents = vec!["agent-alpha".to_string()]; // Only alpha allowed

        let headers = headers_with_token("tok-readonly");
        // agent-ro is not in allowlist
        assert!(validate_federation_token(&headers, &policy).is_err());

        let headers = headers_with_token("tok-alpha");
        assert!(validate_federation_token(&headers, &policy).is_ok());
    }
}
