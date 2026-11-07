//! A2A federation auth and mTLS configuration.
//!
//! - **Authorization middleware**: validates Bearer tokens on the A2A server,
//!   enforces capability-scoped access control, and logs federation events.
//! - **mTLS configuration**: TLS settings for cross-org federation with
//!   mutual certificate authentication.

use crate::auth::{forbidden_response, unauthenticated_response, Authenticator, Principal};
use crate::server::canonical_method;
use axum::{
    body::Body,
    extract::Request,
    http::{HeaderMap, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};
use jamjet_a2a_types::A2aError;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;
use tracing::{info, warn};

// ── Federation auth policy ──────────────────────────────────────────────────

/// Authorization policy for incoming A2A requests.
///
/// Use it either as the server's authenticator
/// (`A2aServer::with_authenticator(policy)`), which also binds tasks to the
/// caller, or as an Axum layer via [`federation_auth_layer`].
///
/// Authentication is required unless `require_auth` is explicitly set to
/// `false`. With `require_auth: false` every caller without a token shares
/// one `anonymous` principal, so the server is effectively single-tenant
/// for them, and the startup warning does not fire.
///
/// `public_agent_card` only matters for [`federation_auth_layer`]. As the
/// server's authenticator the policy only sees JSON-RPC requests; the Agent
/// Card routes stay public.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FederationPolicy {
    /// If true (the default), every request except the public Agent Card
    /// needs a valid Bearer token. Setting it to false turns authentication
    /// off and makes the server single-tenant.
    #[serde(default = "default_true")]
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

    /// Scopes required per RPC method. Maps method names to required
    /// scopes; the caller needs at least one of them, and an empty list means
    /// any authenticated caller. Keys may use the v1.0 names ("SendMessage",
    /// "GetTask", ...) or the v0.3 aliases ("message/send", ...); both
    /// spellings of a method are checked against the same entry.
    ///
    /// An empty map puts no scope restrictions on any method. Once the map
    /// has an entry, it is an allowlist: a method without an entry (including
    /// one whose key is misspelled) is refused. List every method callers
    /// need, e.g. `SendStreamingMessage` next to `SendMessage`.
    #[serde(default)]
    pub method_scopes: HashMap<String, Vec<String>>,
}

impl Default for FederationPolicy {
    fn default() -> Self {
        Self {
            require_auth: true,
            tokens: Vec::new(),
            public_agent_card: true,
            allowed_agents: Vec::new(),
            method_scopes: HashMap::new(),
        }
    }
}

fn default_true() -> bool {
    true
}

/// A federation token with associated metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FederationToken {
    /// The token value (matched against Bearer header).
    pub token: String,
    /// Human-readable name for audit logging. Tokens without an `agent_id`
    /// are bound to tasks by this name, so keep names unique.
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

impl FederationIdentity {
    /// The [`Principal`] tasks are bound to: `agent:<agent_id>` when the
    /// token names an agent, otherwise `token:<token_name>`.
    pub fn principal(&self) -> Principal {
        let id = match &self.agent_id {
            Some(agent) => format!("agent:{agent}"),
            None => format!("token:{}", self.token_name),
        };
        Principal::new(id).with_scopes(self.scopes.iter().cloned())
    }
}

/// Principal id used for unauthenticated callers when `require_auth` is
/// false and the policy is used as an [`Authenticator`].
pub const ANONYMOUS_PRINCIPAL: &str = "anonymous";

/// Compare two byte strings without stopping at the first difference.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
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

    if token.is_empty() {
        return Err(A2aError::Auth {
            reason: "empty bearer token".into(),
        });
    }

    // Check every configured token so the time taken does not depend on
    // which one (if any) matched.
    let mut found = None;
    for candidate in &policy.tokens {
        if constant_time_eq(candidate.token.as_bytes(), token.as_bytes()) && found.is_none() {
            found = Some(candidate);
        }
    }
    let found = found.ok_or_else(|| A2aError::Auth {
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

/// True if a caller holding `has_scope` may call `method` under `policy`.
///
/// Both the requested method and the policy keys are mapped to their v1.0
/// names first, so `message/send` is held to the scopes configured for
/// `SendMessage` and the other way round. If several keys name the same
/// method, each of them must be satisfied. With a non-empty map, a method
/// that no key names is refused.
fn scopes_allow(policy: &FederationPolicy, method: &str, has_scope: impl Fn(&str) -> bool) -> bool {
    if policy.method_scopes.is_empty() {
        return true;
    }
    let target = canonical_method(method).unwrap_or(method);
    let mut entries = policy
        .method_scopes
        .iter()
        .filter(|(key, _)| canonical_method(key).unwrap_or(key) == target)
        .peekable();
    if entries.peek().is_none() {
        return false;
    }
    entries.all(|(_, required)| required.is_empty() || required.iter().any(|s| has_scope(s)))
}

/// Check if an identity has the required scopes for a given RPC method.
///
/// Accepts v1.0 names ("SendMessage", "GetTask", ...) and the v0.3 aliases
/// the server serves ("message/send", ...).
pub fn check_method_scopes(
    identity: &FederationIdentity,
    method: &str,
    policy: &FederationPolicy,
) -> bool {
    scopes_allow(policy, method, |s| identity.scopes.contains(s))
}

#[async_trait::async_trait]
impl Authenticator for FederationPolicy {
    async fn authenticate(&self, headers: &HeaderMap) -> Result<Principal, A2aError> {
        if !self.require_auth && !headers.contains_key("authorization") {
            return Ok(Principal::new(ANONYMOUS_PRINCIPAL));
        }
        // A presented token is always checked, even when auth is optional.
        let identity = validate_federation_token(headers, self)?;
        info!(
            token_name = %identity.token_name,
            agent_id = ?identity.agent_id,
            "A2A federation auth: authorized"
        );
        Ok(identity.principal())
    }

    fn authorize(&self, principal: &Principal, method: &str) -> bool {
        scopes_allow(self, method, |s| principal.scopes.contains(s))
    }
}

/// Agent Card paths served by `A2aServer`.
const AGENT_CARD_PATHS: [&str; 2] = ["/.well-known/agent-card.json", "/.well-known/agent.json"];

/// Largest JSON-RPC body the layer reads to find the method. Matches Axum's
/// default body limit for the server's own extractor.
const MAX_RPC_BODY_BYTES: usize = 2 * 1024 * 1024;

fn invalid_request_response() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": { "code": -32600, "message": "Invalid Request" }
        })),
    )
        .into_response()
}

/// Axum middleware layer for federation auth.
///
/// Insert into the A2A server router to require authentication:
/// ```ignore
/// use axum::middleware;
/// router.layer(middleware::from_fn_with_state(policy, federation_auth_layer))
/// ```
///
/// When `require_auth` is true (the default) the layer:
///
/// - lets `GET`/`HEAD` requests for the Agent Card through without a token
///   if `public_agent_card` is set (exact path match only);
/// - rejects requests without a valid Bearer token with HTTP 401;
/// - reads the JSON-RPC body of `POST` requests and rejects methods the
///   token lacks scopes for with HTTP 403, and bodies with no method with
///   HTTP 400;
/// - passes the caller's [`Principal`] to the server in the request
///   extensions, so tasks are bound to the caller.
///
/// If the server also has its own authenticator, the server's authenticator
/// decides and this layer's principal is ignored.
pub async fn federation_auth_layer(
    axum::extract::State(policy): axum::extract::State<FederationPolicy>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let path = request.uri().path().to_string();

    // Allow public access to the Agent Card if configured.
    let card_request = matches!(*request.method(), Method::GET | Method::HEAD)
        && AGENT_CARD_PATHS.contains(&path.as_str());
    if policy.public_agent_card && card_request {
        return next.run(request).await;
    }

    if !policy.require_auth {
        return next.run(request).await;
    }

    let identity = match validate_federation_token(request.headers(), &policy) {
        Ok(identity) => identity,
        Err(e) => {
            warn!(
                error = %e,
                path = %path,
                "A2A federation auth: rejected"
            );
            return unauthenticated_response();
        }
    };

    // The method is in the JSON-RPC body: read it (bounded), check scopes,
    // then hand the same bytes on.
    let (mut parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, MAX_RPC_BODY_BYTES).await {
        Ok(b) => b,
        Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
    };

    if parts.method == Method::POST {
        let rpc: Option<serde_json::Value> = serde_json::from_slice(&bytes).ok();
        let method = rpc
            .as_ref()
            .and_then(|v| v.get("method"))
            .and_then(|m| m.as_str());
        let Some(method) = method else {
            warn!(path = %path, "A2A federation auth: rejected request without a method");
            return invalid_request_response();
        };
        if !check_method_scopes(&identity, method, &policy) {
            warn!(
                token_name = %identity.token_name,
                method = %method,
                "A2A federation auth: missing scope"
            );
            let id = rpc
                .as_ref()
                .and_then(|v| v.get("id"))
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            return forbidden_response(id, canonical_method(method).unwrap_or(method));
        }
    }

    info!(
        token_name = %identity.token_name,
        agent_id = ?identity.agent_id,
        path = %path,
        "A2A federation auth: authorized"
    );
    parts.extensions.insert(identity.principal());
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
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

    // A configured CA that cannot be read is an error, not a silent fallback
    // to the system roots.
    let ca = match tls.ca_cert_path {
        Some(_) => {
            let ca_pem = tls.load_ca_cert()?;
            Some(
                reqwest::Certificate::from_pem(&ca_pem).map_err(|e| A2aError::Auth {
                    reason: format!("invalid CA cert: {e}"),
                })?,
            )
        }
        None => None,
    };

    let (cert_pem, key_pem) = tls.load_cert_key()?;
    let identity =
        reqwest::Identity::from_pkcs8_pem(&cert_pem, &key_pem).map_err(|e| A2aError::Auth {
            reason: format!("invalid identity PEM: {e}"),
        })?;

    let mut builder = reqwest::Client::builder()
        .identity(identity)
        .connect_timeout(Duration::from_secs(10));
    if let Some(ca) = ca {
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
    fn policy_requires_auth_unless_turned_off() {
        assert!(FederationPolicy::default().require_auth);
        let parsed: FederationPolicy = serde_json::from_str("{}").unwrap();
        assert!(parsed.require_auth);
        let parsed: FederationPolicy = serde_json::from_str(r#"{"require_auth": false}"#).unwrap();
        assert!(!parsed.require_auth);
    }

    #[test]
    fn v03_alias_is_held_to_the_v1_scope() {
        let policy = test_policy();
        let identity =
            validate_federation_token(&headers_with_token("tok-readonly"), &policy).unwrap();
        // SendMessage needs "write"; its alias must too.
        assert!(!check_method_scopes(&identity, "message/send", &policy));
        assert!(check_method_scopes(&identity, "tasks/get", &policy));

        // A scope configured under the alias applies to the v1.0 name.
        let mut policy = test_policy();
        policy.method_scopes = [("tasks/cancel".to_string(), vec!["admin".to_string()])]
            .into_iter()
            .collect();
        assert!(!check_method_scopes(&identity, "CancelTask", &policy));
    }

    #[test]
    fn method_scopes_are_an_allowlist_once_set() {
        let policy = test_policy();
        let identity =
            validate_federation_token(&headers_with_token("tok-alpha"), &policy).unwrap();
        // test_policy lists SendMessage and GetTask only.
        assert!(!check_method_scopes(
            &identity,
            "SendStreamingMessage",
            &policy
        ));
        assert!(!check_method_scopes(&identity, "CancelTask", &policy));

        // A misspelled key does not leave the real method open.
        let mut typo = test_policy();
        typo.method_scopes = [("sendMessage".to_string(), vec!["admin".to_string()])]
            .into_iter()
            .collect();
        assert!(!check_method_scopes(&identity, "SendMessage", &typo));

        // An empty list means any authenticated caller; an empty map means
        // no restrictions at all.
        let mut open = test_policy();
        open.method_scopes = [("ListTasks".to_string(), vec![])].into_iter().collect();
        assert!(check_method_scopes(&identity, "ListTasks", &open));
        open.method_scopes.clear();
        assert!(check_method_scopes(&identity, "CancelTask", &open));
    }

    #[test]
    fn empty_bearer_token_never_authenticates() {
        let mut policy = test_policy();
        policy.tokens.push(FederationToken {
            token: String::new(),
            name: "misconfigured".to_string(),
            agent_id: Some("agent-empty".to_string()),
            scopes: vec!["write".to_string()],
        });
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Bearer "));
        assert!(validate_federation_token(&headers, &policy).is_err());
    }

    #[test]
    fn identity_principal_prefers_agent_id() {
        let policy = test_policy();
        let identity =
            validate_federation_token(&headers_with_token("tok-alpha"), &policy).unwrap();
        assert_eq!(identity.principal().id, "agent:agent-alpha");
        let anonymous_agent = FederationIdentity {
            token_name: "ci".into(),
            agent_id: None,
            scopes: HashSet::new(),
        };
        assert_eq!(anonymous_agent.principal().id, "token:ci");
    }

    #[test]
    fn unreadable_ca_is_an_error_not_a_fallback() {
        let tls = TlsConfig {
            enabled: true,
            ca_cert_path: Some(PathBuf::from("/nonexistent/jamjet-a2a-test-ca.pem")),
            ..Default::default()
        };
        let err = build_mtls_client(&tls).unwrap_err();
        assert!(err.to_string().contains("CA cert"), "{err}");
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
