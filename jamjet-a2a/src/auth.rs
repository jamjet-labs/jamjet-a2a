//! Authentication hook for the A2A server.
//!
//! Plug an [`Authenticator`] into [`A2aServer::with_authenticator`] to make the
//! server multi-tenant. Every JSON-RPC request is then authenticated before
//! its body is parsed, every new task is bound to the [`Principal`] that
//! created it, and other principals get `TaskNotFoundError` for that task, as
//! A2A v1.0 section 13.1 requires.
//!
//! Without an authenticator the server is single-tenant. See the
//! [`server`](crate::server) module docs.
//!
//! [`A2aServer::with_authenticator`]: crate::server::A2aServer::with_authenticator

use axum::{
    http::{header, StatusCode},
    response::{IntoResponse, Json, Response},
};

/// Request headers, as passed to [`Authenticator::authenticate`]. Re-exported
/// so implementations do not need a direct `axum` dependency.
pub use axum::http::HeaderMap;
use jamjet_a2a_types::A2aError;
use std::collections::BTreeSet;

/// JSON-RPC error code sent with HTTP 401 when credentials are missing or
/// invalid.
///
/// A2A v1.0 leaves authentication errors to a "JSON-RPC custom error". This
/// code sits outside the range JSON-RPC reserves (-32768 to -32000), so it
/// cannot collide with an A2A error code.
pub const UNAUTHENTICATED_ERROR_CODE: i32 = -31401;

/// JSON-RPC error code sent with HTTP 403 when an authenticated caller may not
/// call the requested method.
pub const FORBIDDEN_ERROR_CODE: i32 = -31403;

/// An authenticated caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    /// Stable identifier for the caller. Tasks are bound to it, so two
    /// callers with the same id see each other's tasks. The server rejects
    /// a principal whose id is empty.
    pub id: String,
    /// Scopes granted to the caller, for [`Authenticator::authorize`].
    pub scopes: BTreeSet<String>,
}

impl Principal {
    /// A principal with the given id and no scopes.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            scopes: BTreeSet::new(),
        }
    }

    /// Add scopes to the principal.
    pub fn with_scopes<I, S>(mut self, scopes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.scopes.extend(scopes.into_iter().map(Into::into));
        self
    }
}

/// Validates request credentials for the A2A server.
///
/// ```rust
/// use jamjet_a2a::auth::{Authenticator, Principal};
/// use jamjet_a2a_types::A2aError;
///
/// struct OneToken;
///
/// #[async_trait::async_trait]
/// impl Authenticator for OneToken {
///     async fn authenticate(
///         &self,
///         headers: &jamjet_a2a::auth::HeaderMap,
///     ) -> Result<Principal, A2aError> {
///         let presented = headers
///             .get("authorization")
///             .and_then(|v| v.to_str().ok())
///             .and_then(|v| v.strip_prefix("Bearer "));
///         match presented {
///             Some("s3cret") => Ok(Principal::new("client-a")),
///             _ => Err(A2aError::Auth {
///                 reason: "invalid token".into(),
///             }),
///         }
///     }
/// }
/// ```
#[async_trait::async_trait]
pub trait Authenticator: Send + Sync {
    /// Validate the credentials carried by a request.
    ///
    /// Return the caller's [`Principal`], or an error to reject the request
    /// with HTTP 401. The error text is logged, not sent to the caller.
    async fn authenticate(&self, headers: &HeaderMap) -> Result<Principal, A2aError>;

    /// Decide whether `principal` may call `method`.
    ///
    /// `method` is always the v1.0 name (`SendMessage`, `GetTask`, ...), also
    /// when the caller used a v0.3 alias such as `message/send`. Returning
    /// `false` rejects the request with HTTP 403. The default allows every
    /// method.
    fn authorize(&self, principal: &Principal, method: &str) -> bool {
        let _ = (principal, method);
        true
    }
}

/// HTTP 401 response with a JSON-RPC error body and a Bearer challenge.
pub(crate) fn unauthenticated_response() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        Json(serde_json::json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": {
                "code": UNAUTHENTICATED_ERROR_CODE,
                "message": "Unauthenticated: missing or invalid credentials",
            }
        })),
    )
        .into_response()
}

/// HTTP 403 response naming the method the caller may not use.
pub(crate) fn forbidden_response(id: serde_json::Value, method: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": FORBIDDEN_ERROR_CODE,
                "message": format!("Forbidden: caller is not allowed to call {method}"),
            }
        })),
    )
        .into_response()
}
