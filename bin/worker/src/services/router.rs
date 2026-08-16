//! Request routing.
//!
//! Kept inline (no `worker::Router`) to match the minimal style of the sibling
//! workers in the agent-web repo: the surface is small and stable.
//!
//! ```text
//! GET  /                       liveness — no DB access, unauthenticated
//! GET  /resolve?id=kind:value  resolve one identifier      (mutates, LOGGED)
//! GET  /resolve?<kind>=value   ...same, per-kind form
//! GET  /entity/<canonical_id>  load an entity by canonical id (read)
//! GET  /stats                  miss-rate report            (read)
//! POST /ingest                 commit a seed record        (WRITE — token)
//! POST /__conformance          run the GraphStore contract suite
//!                              (test builds only — `test-endpoints` feature)
//! ```
//!
//! Reads are open; write endpoints require a bearer token when `AUTH_TOKEN` is
//! configured. Resolution is a *write* in the general case (it mints/attaches), so
//! `/resolve` is deliberately GET-but-mutating — see [`super::handlers::resolve`],
//! which documents that trade-off.
//!
//! **Logging rule: `/resolve` only.** Every user-facing resolve appends a
//! `resolutions` row, and that log *is* the miss-rate metric — which is the
//! documented evidence gate for ever adding a fuzzy-matching layer (see
//! `ROADMAP.md`). `/entity` and `/ingest` are deliberately EXCLUDED: a direct id
//! lookup and a seed load are not user-facing *queries*, so counting them would
//! skew the rate. This mirrors the CLI, which calls `record_outcome` from exactly
//! one place (its `Resolve` arm). **A new endpoint must make this choice
//! explicitly.** Asserted by `test/stats.test.ts`.

use worker::*;

use super::handlers;

pub async fn route(mut req: Request, env: Env) -> Result<Response> {
    let url = req.url()?;
    let path = url.path().trim_end_matches('/').to_string();
    let path = if path.is_empty() { "/".to_string() } else { path };
    let method = req.method();

    // Liveness first: no DB binding touched, so it answers even if D1 is
    // misconfigured — which is exactly what you want from a health check.
    if method == Method::Get && (path == "/" || path == "/health") {
        return Response::ok("sameas worker ready");
    }

    match (method.clone(), path.as_str()) {
        (Method::Get, "/resolve") => handlers::resolve(&req, &env).await,
        (Method::Get, "/stats") => handlers::stats(&env).await,
        (Method::Post, "/ingest") => {
            if let Err(resp) = require_token(&req, &env) {
                return Ok(resp);
            }
            handlers::ingest(&mut req, &env).await
        }
        (Method::Get, p) if p.starts_with("/entity/") => {
            let cid = p.trim_start_matches("/entity/");
            handlers::entity(cid, &env).await
        }
        // POST (not GET) so no crawler or link prefetch can trigger it, `__`
        // prefixed to mark it non-public, and token-gated as well — defense in
        // depth for a route that merges and splits fixture entities.
        #[cfg(feature = "test-endpoints")]
        (Method::Post, "/__conformance") => {
            if let Err(resp) = require_token(&req, &env) {
                return Ok(resp);
            }
            handlers::conformance(&env).await
        }
        _ => handlers::error_json(
            &format!("not found: {method} {path}"),
            "not_found",
            404,
        ),
    }
}

/// The auth decision as pure data: `(http status, error code, message)` on
/// rejection. No `Request`, no `Env`, no JS.
///
/// Split out from [`require_token`] so it can be unit-tested on the **host**
/// target. Anything touching `Request`/`Env` cannot be: `wasm-bindgen` replaces
/// every extern with `panic!("function not implemented on non-wasm32 targets")`
/// off wasm32, and `Env` is an extern type with no constructor — so those types
/// only exist inside a Worker. The thin shell below is covered by the miniflare
/// suite (`test/auth.test.ts`) instead.
fn auth_outcome(
    expected: Option<&str>,
    auth_header: Option<&str>,
) -> std::result::Result<(), (u16, &'static str, &'static str)> {
    // An unconfigured secret must never silently mean "no auth": closed, not open.
    let expected = match expected {
        None => {
            return Err((
                503,
                "write_disabled",
                "write endpoints are disabled: AUTH_TOKEN is not configured",
            ))
        }
        Some("") => {
            return Err((
                503,
                "write_disabled",
                "write endpoints are disabled: AUTH_TOKEN is empty",
            ))
        }
        Some(e) => e,
    };
    let supplied = auth_header
        .unwrap_or("")
        .strip_prefix("Bearer ")
        .unwrap_or("");
    // Length check first, then a constant-ish comparison over the bytes. Not a
    // hardened primitive, but it avoids the trivial early-exit leak of `==`.
    if supplied.len() != expected.len()
        || supplied
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            != 0
    {
        return Err((401, "unauthorized", "unauthorized"));
    }
    Ok(())
}

/// Bearer-token gate for write endpoints.
///
/// When `AUTH_TOKEN` is unset the endpoint is **closed**, not open — an
/// unconfigured secret must never silently mean "no auth". Service-binding callers
/// still need the token for writes; only reads are unauthenticated.
///
/// Rejections go through [`handlers::error_json`] like every other error path: a
/// bare `Response::error` here would make the auth endpoints answer in
/// `text/plain` while the rest of the surface answers JSON, so a client could not
/// parse errors uniformly.
fn require_token(req: &Request, env: &Env) -> std::result::Result<(), Response> {
    let expected = env.secret("AUTH_TOKEN").ok().map(|v| v.to_string());
    let header = req.headers().get("authorization").ok().flatten();
    auth_outcome(expected.as_deref(), header.as_deref()).map_err(|(status, code, msg)| {
        handlers::error_json(msg, code, status)
            .unwrap_or_else(|_| Response::error(msg, status).unwrap())
    })
}

#[cfg(test)]
mod tests {
    use super::auth_outcome;

    #[test]
    fn unset_token_closes_the_endpoint() {
        // Closed, not open: an unconfigured secret is a 503, never a bypass.
        assert_eq!(auth_outcome(None, Some("Bearer x")).unwrap_err().0, 503);
    }

    #[test]
    fn empty_token_closes_the_endpoint() {
        assert_eq!(auth_outcome(Some(""), Some("Bearer ")).unwrap_err().0, 503);
    }

    #[test]
    fn correct_token_passes() {
        assert!(auth_outcome(Some("s3cret"), Some("Bearer s3cret")).is_ok());
    }

    #[test]
    fn wrong_token_is_401() {
        let (status, code, _) = auth_outcome(Some("s3cret"), Some("Bearer wrong!")).unwrap_err();
        assert_eq!(status, 401);
        assert_eq!(code, "unauthorized");
    }

    #[test]
    fn a_length_prefix_is_not_enough() {
        // Guards the length check specifically: without it, `zip` would stop at
        // the shorter input and a prefix would authenticate.
        assert_eq!(
            auth_outcome(Some("s3cret"), Some("Bearer s3c")).unwrap_err().0,
            401
        );
    }

    #[test]
    fn missing_or_unprefixed_header_is_401() {
        assert_eq!(auth_outcome(Some("s3cret"), None).unwrap_err().0, 401);
        // No "Bearer " prefix — the raw token must not be accepted.
        assert_eq!(
            auth_outcome(Some("s3cret"), Some("s3cret")).unwrap_err().0,
            401
        );
    }

    #[test]
    fn rejects_a_difference_at_either_end() {
        // The byte fold must not short-circuit: a first-byte and a last-byte
        // difference are both rejected.
        assert!(auth_outcome(Some("abcdef"), Some("Bearer Xbcdef")).is_err());
        assert!(auth_outcome(Some("abcdef"), Some("Bearer abcdeX")).is_err());
    }
}
