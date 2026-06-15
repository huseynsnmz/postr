//! Bearer-token check for the CLI client. Port of the bearer arm of
//! `worker/workers/lib/api-auth.ts:38-50` (the CF Access JWT arm is
//! deferred to a later workflow — see TODO below).
//!
//! Failure modes (mirrored from the TS handler):
//!   * Missing `CLI_TOKEN` secret -> 500 server_misconfigured
//!   * Bearer token mismatch       -> 401 unauthorized
//!   * No Authorization header AND ENVIRONMENT != "production" -> pass
//!     through (dev bypass)
//!   * No Authorization header in production -> 401 unauthorized
//!     (the TS worker would reach the CF Access path here; we don't
//!     verify JWTs yet, so we fail closed.)

use worker::{Env, Error, Request, Response, Result};

/// Tag values carried inside `Error::RustError(...)` so the route
/// handler can map them to the right HTTP response without parsing
/// freeform strings.
pub const ERR_SERVER_MISCONFIGURED: &str = "server_misconfigured";
pub const ERR_UNAUTHORIZED: &str = "unauthorized";

pub async fn check_auth(req: &Request, env: &Env) -> Result<()> {
    let auth = req.headers().get("Authorization")?.unwrap_or_default();

    // Path 1: bearer token (CLI client).
    if let Some(presented) = auth.strip_prefix("Bearer ") {
        let expected = env
            .secret("CLI_TOKEN")
            .map_err(|_| Error::RustError(ERR_SERVER_MISCONFIGURED.into()))?
            .to_string();
        if expected.is_empty() {
            return Err(Error::RustError(ERR_SERVER_MISCONFIGURED.into()));
        }
        if !constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
            return Err(Error::RustError(ERR_UNAUTHORIZED.into()));
        }
        return Ok(());
    }

    // Path 2: dev bypass.
    let env_kind = env
        .var("ENVIRONMENT")
        .map(|v| v.to_string())
        .unwrap_or_default();
    if env_kind != "production" {
        return Ok(());
    }

    // Path 3: CF Access JWT verification — not implemented yet.
    // TODO(next workflow): port `jwtVerify` against the JWKS at
    // `{TEAM_DOMAIN}/cdn-cgi/access/certs` with audience POLICY_AUD.
    // For now production requests without a Bearer fail closed.
    Err(Error::RustError(ERR_UNAUTHORIZED.into()))
}

/// Map an `Error::RustError` from [`check_auth`] to the appropriate
/// HTTP response. Route handlers funnel auth failures through here so
/// the JSON shape stays consistent.
pub fn auth_error_response(err: Error) -> Result<Response> {
    let msg = err.to_string();
    match msg.as_str() {
        s if s.ends_with(ERR_SERVER_MISCONFIGURED) => Ok(Response::from_json(
            &serde_json::json!({"error": ERR_SERVER_MISCONFIGURED}),
        )?
        .with_status(500)),
        s if s.ends_with(ERR_UNAUTHORIZED) => Ok(Response::from_json(
            &serde_json::json!({"error": ERR_UNAUTHORIZED}),
        )?
        .with_status(401)),
        // Anything else is a bug in the caller — surface as 500 with
        // the underlying message so we don't swallow it.
        _ => Ok(Response::from_json(&serde_json::json!({"error": msg}))?.with_status(500)),
    }
}

/// Length-checked XOR compare. Same algorithm as the TS `timingSafeEqual`
/// in `worker/workers/lib/api-auth.ts:20-25`.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}
