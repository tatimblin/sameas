//! `sameas` as a Cloudflare Worker.
//!
//! Exposes the crosswalk graph over HTTP, backed by D1
//! ([`sameas_core::store::d1::D1Store`]). Reachable two ways, deliberately:
//!
//! * **Service binding** — another Worker in the same account calls
//!   `env.SAMEAS.fetch(...)`. Never touches the internet, no egress cost. Account-
//!   internal traffic is implicitly trusted, so it needs no token.
//! * **Public HTTPS** — a custom domain / `workers.dev` route for outside callers.
//!
//! **Local-graph resolution only.** Hub completion (Wikidata / TMDb / Google
//! Places) is NOT wired up here: `sameas-core`'s live transport is
//! `reqwest::blocking`, unusable in a Worker, so it needs a `fetch`-based
//! `HttpTransport` first. That is also the right order of operations — Google Place
//! Details billing sits in Google's Enterprise SKU, so an open `complete=true`
//! endpoint would let a stranger spend real money. When it lands it must be
//! key-gated with a per-key quota.

use worker::*;

mod services;

#[event(fetch, respond_with_errors)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    console_error_panic_hook::set_once();
    services::router::route(req, env).await
}
