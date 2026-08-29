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
//! **Hub completion is wired up, on one route only.** `POST /resolve/name`
//! (`services::handlers::resolve_name`) builds a `CompletionCtx` over
//! `sameas_core::transport::FetchTransport` (feature `worker-fetch`, enabled in
//! this crate's `Cargo.toml`) — a `worker::Fetch`-backed `HttpTransport`, since
//! `reqwest` cannot run in workerd. Hub API keys come from the Worker secrets
//! `GOOGLE_PLACES_API_KEY`, `TMDB_API_KEY` and `PLACEKEY_API_KEY` (set per
//! environment: `wrangler secret put --env staging` is a separate store). A hub
//! whose key is absent is simply not called, and the route says so in its `hint`.
//!
//! Every other route stays **local-graph only** — `/resolve` and `/ingest` make no
//! external call at all.
//!
//! Google Place Details billing sits in Google's Enterprise SKU, so the spending
//! route carries three brakes: the `AUTH_TOKEN` bearer gate (`workers_dev = true`
//! makes this worker publicly reachable), a per-caller daily call budget
//! (`services::budget`, `HUB_DAILY_BUDGET`; `0` disables all hub calls), and the
//! core's own local-first caching, which answers a repeated name query with zero
//! external calls.

use worker::*;

mod services;

#[event(fetch, respond_with_errors)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    console_error_panic_hook::set_once();
    services::router::route(req, env).await
}
