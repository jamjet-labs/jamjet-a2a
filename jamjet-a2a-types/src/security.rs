//! Security scheme types for A2A v1.0 Agent Cards.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ────────────────────────────────────────────────────────────────────────────
// SecurityScheme
// ────────────────────────────────────────────────────────────────────────────

/// A security scheme that an agent supports.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum SecurityScheme {
    ApiKey(APIKeySecurityScheme),
    HttpAuth(HTTPAuthSecurityScheme),
    OAuth2(OAuth2SecurityScheme),
    OpenIdConnect(OpenIdConnectSecurityScheme),
    MutualTls(MutualTlsSecurityScheme),
}

// ────────────────────────────────────────────────────────────────────────────
// APIKeySecurityScheme
// ────────────────────────────────────────────────────────────────────────────

/// API-key based authentication.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct APIKeySecurityScheme {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub location: String,
    pub name: String,
}

// ────────────────────────────────────────────────────────────────────────────
// HTTPAuthSecurityScheme
// ────────────────────────────────────────────────────────────────────────────

/// HTTP authentication (e.g. Bearer, Basic).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HTTPAuthSecurityScheme {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub scheme: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bearer_format: Option<String>,
}

// ────────────────────────────────────────────────────────────────────────────
// OAuth2SecurityScheme
// ────────────────────────────────────────────────────────────────────────────

/// OAuth 2.0 authentication.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuth2SecurityScheme {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub flows: OAuthFlows,
}

// ────────────────────────────────────────────────────────────────────────────
// OAuthFlows
// ────────────────────────────────────────────────────────────────────────────

/// Supported OAuth 2.0 flow types.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum OAuthFlows {
    AuthorizationCode(AuthorizationCodeOAuthFlow),
    ClientCredentials(ClientCredentialsOAuthFlow),
    DeviceCode(DeviceCodeOAuthFlow),
}

// ────────────────────────────────────────────────────────────────────────────
// AuthorizationCodeOAuthFlow
// ────────────────────────────────────────────────────────────────────────────

/// OAuth 2.0 Authorization Code flow parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthorizationCodeOAuthFlow {
    pub authorization_url: String,
    pub token_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_url: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub scopes: HashMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pkce_required: Option<bool>,
}

// ────────────────────────────────────────────────────────────────────────────
// ClientCredentialsOAuthFlow
// ────────────────────────────────────────────────────────────────────────────

/// OAuth 2.0 Client Credentials flow parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientCredentialsOAuthFlow {
    pub token_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_url: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub scopes: HashMap<String, String>,
}

// ────────────────────────────────────────────────────────────────────────────
// DeviceCodeOAuthFlow
// ────────────────────────────────────────────────────────────────────────────

/// OAuth 2.0 Device Code flow parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceCodeOAuthFlow {
    pub device_authorization_url: String,
    pub token_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_url: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub scopes: HashMap<String, String>,
}

// ────────────────────────────────────────────────────────────────────────────
// OpenIdConnectSecurityScheme
// ────────────────────────────────────────────────────────────────────────────

/// OpenID Connect authentication.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenIdConnectSecurityScheme {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub open_id_connect_url: String,
}

// ────────────────────────────────────────────────────────────────────────────
// MutualTlsSecurityScheme
// ────────────────────────────────────────────────────────────────────────────

/// Mutual TLS (mTLS) authentication.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MutualTlsSecurityScheme {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

// ────────────────────────────────────────────────────────────────────────────
// SecurityRequirement
// ────────────────────────────────────────────────────────────────────────────

/// A security requirement referencing one or more named security schemes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SecurityRequirement {
    #[serde(flatten)]
    pub schemes: HashMap<String, Vec<String>>,
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_auth_bearer_round_trip() {
        let scheme = SecurityScheme::HttpAuth(HTTPAuthSecurityScheme {
            description: None,
            scheme: "Bearer".into(),
            bearer_format: Some("JWT".into()),
        });
        let json = serde_json::to_value(&scheme).unwrap();
        let back: SecurityScheme = serde_json::from_value(json).unwrap();
        match back {
            SecurityScheme::HttpAuth(h) => assert_eq!(h.scheme, "Bearer"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn api_key_round_trip() {
        let scheme = SecurityScheme::ApiKey(APIKeySecurityScheme {
            description: None,
            location: "header".into(),
            name: "X-API-Key".into(),
        });
        let json = serde_json::to_value(&scheme).unwrap();
        let back: SecurityScheme = serde_json::from_value(json).unwrap();
        match back {
            SecurityScheme::ApiKey(a) => assert_eq!(a.name, "X-API-Key"),
            _ => panic!("wrong variant"),
        }
    }
}
