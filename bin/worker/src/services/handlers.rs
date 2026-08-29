//! Endpoint handlers. Each opens a [`D1Store`] over the `DB` binding and calls the
//! same `sameas-core` orchestration the CLI uses, so behavior cannot diverge
//! between the two front-ends.

use std::sync::Arc;

use sameas_core::complete::{name_hub_for, NameHub};
use sameas_core::confidence::reason_tag;
use sameas_core::json::resolve_output_json;
use sameas_core::store::d1::D1Store;
use sameas_core::transport::FetchTransport;
use sameas_core::{
    commit_record, commit_record_with_opts, load_entity, name_not_found, resolve_name_local,
    CommitOpts, CompletionCtx, EntityRecord, ExternalId, GraphStore, ResolveOutput, Status,
};
use serde_json::json;
use worker::*;

use super::budget::{self, Reservation};
use super::name_request::NameRequest;

/// A structured error document, so a client never has to parse prose.
pub fn error_json(message: &str, code: &str, status: u16) -> Result<Response> {
    let body = json!({ "error": { "code": code, "message": message } });
    Ok(Response::from_json(&body)?.with_status(status))
}

fn ok_json(value: &serde_json::Value) -> Result<Response> {
    Response::from_json(value)
}

fn store(env: &Env) -> Result<D1Store> {
    Ok(D1Store::new(env.d1("DB")?))
}

/// Map an `anyhow::Error` from the core onto a 500 with its full chain. The core's
/// errors are operational (D1 unavailable, malformed row), not user input errors —
/// those are caught before this point and return 400.
fn core_error(e: anyhow::Error) -> Result<Response> {
    error_json(&format!("{e:#}"), "resolution_failed", 500)
}

/// `GET /resolve?id=kind:value` (or `?domain=`, `?phone=`, `?place_id=`, …).
///
/// **This mutates.** Resolving attaches the input to a cluster, minting an entity
/// when the identifier is new — that *is* the operation (`resolve` is completion, per
/// PROJECT_GOALS). It stays a GET because callers treat it as a lookup and it is
/// idempotent: resolving the same identifier twice yields the same canonical id and
/// writes nothing the second time.
pub async fn resolve(req: &Request, env: &Env) -> Result<Response> {
    let url = req.url()?;
    let params: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();

    let id = match parse_identifier(&params) {
        Ok(id) => id,
        Err(msg) => return error_json(&msg, "invalid_input", 400),
    };

    let g = store(env)?;
    let input_desc = id.key();
    let record = EntityRecord {
        same_as: vec![id],
        ..Default::default()
    };
    match commit_record(&g, &record).await {
        Ok(out) => {
            log_resolution(&g, &out, &input_desc).await;
            ok_json(&resolve_output_json(&out, "resolve"))
        }
        Err(e) => core_error(e),
    }
}

/// Append one row to the `resolutions` log.
///
/// That log *is* the miss-rate metric — the documented evidence gate for ever
/// adding a fuzzy-matching layer — so a silently empty log would make the decision
/// undecidable rather than merely wrong. Mirrors the CLI's `record_outcome`.
///
/// **Which routes call this is a deliberate, per-endpoint choice**, documented in
/// `router.rs` and asserted by `test/stats.test.ts`: `/resolve` and
/// `/resolve/name` are user-facing *queries* and log; `/entity` and `/ingest` are a
/// direct id lookup and a seed load, and logging them would skew the rate.
///
/// Best-effort: a logging failure must never fail the resolution.
async fn log_resolution(g: &D1Store, out: &ResolveOutput, input_desc: &str) {
    let _ = g
        .record_resolution(
            out.status.as_str(),
            reason_tag(&out.confidence_reason),
            out.matched_via.first().map(|s| s.as_str()),
            out.confidence,
            Some(input_desc),
        )
        .await;
}

// ---------------------------------------------------------------------------
// POST /resolve/name — the disambiguation route
// ---------------------------------------------------------------------------

/// The `action` tag on this route's response documents, distinguishing them from
/// `/resolve`'s in a consumer's logs.
const NAME_ACTION: &str = "resolve_name";

/// Which step of the orchestration produced the answer. Reported as
/// `resolved_by`, because the two steps mean different things to the caller (an
/// identifier verdict is about the keys it sent; a name verdict is about the
/// world) and because "did the fall-through actually happen" is otherwise
/// unobservable from outside.
#[derive(Clone, Copy)]
enum Step {
    /// The strict-grain commit over the request's `identifiers`.
    Identifiers,
    /// The local graph answered the name query — zero external calls.
    NameLocal,
    /// The name search: hub-routed, or refused before reaching one.
    NameSearch,
}

impl Step {
    fn tag(self) -> &'static str {
        match self {
            Step::Identifiers => "identifiers",
            Step::NameLocal => "name_local",
            Step::NameSearch => "name_search",
        }
    }
}

/// Orchestration facts about one answer, added to the core's resolve document.
struct Meta {
    step: Step,
    /// The hub the name query routed to, when it got that far.
    hub: Option<NameHub>,
    /// Whether an outbound hub call was actually attempted. `false` with a `hub`
    /// present means the route refused to reach out (missing key).
    hub_called: bool,
    /// This bucket's hub-call count for today, when a call was reserved.
    hub_calls_today: Option<u32>,
    /// Step 1's refusal hint, carried onto a step 2 answer.
    ///
    /// It is the sentence that explains *why the caller's own identifiers were
    /// not enough* ("souvla.com is a brand/site rather than one specific
    /// thing"), which is exactly the message a consumer shows its user — and it
    /// would otherwise be discarded the moment the name search produced the
    /// candidate list. `hint` on the document itself belongs to whichever step
    /// answered, so this rides alongside rather than overwriting it.
    identifier_hint: Option<String>,
}

impl Meta {
    fn local(step: Step) -> Meta {
        Meta {
            step,
            hub: None,
            hub_called: false,
            hub_calls_today: None,
            identifier_hint: None,
        }
    }

    fn with_identifier_hint(mut self, hint: Option<String>) -> Meta {
        self.identifier_hint = hint;
        self
    }
}

/// The per-hub API keys, read from Worker secrets.
///
/// The Worker had **no key plumbing at all** before this route: keys lived only in
/// the CLI's `.env`, which is why `lib.rs` used to describe hub completion as
/// unreachable here. Set them with `wrangler secret put <NAME>` — and again with
/// `--env staging`, which is a separate secret store.
struct HubKeys {
    google: String,
    tmdb: String,
    placekey: String,
}

impl HubKeys {
    fn read(env: &Env) -> HubKeys {
        let read = |name: &str| {
            env.secret(name)
                .ok()
                .map(|v| v.to_string())
                .unwrap_or_default()
        };
        HubKeys {
            google: read("GOOGLE_PLACES_API_KEY"),
            tmdb: read("TMDB_API_KEY"),
            placekey: read("PLACEKEY_API_KEY"),
        }
    }

    /// The secret this hub cannot run without, when it is not configured.
    ///
    /// An absent key is a **deployment** state, not an error: the route answers
    /// `unresolved` with a hint naming the missing secret, and — crucially — makes
    /// no outbound call and spends no budget. The alternative (call anyway and let
    /// the hub 401) burns a subrequest and reports a hub outage for a
    /// configuration problem.
    ///
    /// Wikidata needs no key at all, so it is always available: the type-agnostic
    /// fallback stays free, exactly as `name_hub_for` intends.
    ///
    /// `PLACEKEY_API_KEY` is deliberately NOT required for `Places`: Placekey is a
    /// best-effort enrichment inside the places branch (only attempted when the
    /// query carries a street), and identity rests on the Google `place_id`
    /// either way. Absent, that one sub-call fails and is swallowed — the same
    /// contract every hub call has.
    fn missing_for(&self, hub: NameHub) -> Option<&'static str> {
        match hub {
            NameHub::Places if self.google.trim().is_empty() => Some("GOOGLE_PLACES_API_KEY"),
            NameHub::Tmdb if self.tmdb.trim().is_empty() => Some("TMDB_API_KEY"),
            _ => None,
        }
    }
}

/// `POST /resolve/name` — resolve one entity reference, refusing to guess.
///
/// **Token-gated.** Reads are otherwise open here, but this route reaches paid
/// hubs and `workers_dev = true` makes the worker publicly reachable, so an
/// unauthenticated caller could spend real money. See `router.rs`.
///
/// **Logs to `resolutions`** — the explicit per-endpoint choice `router.rs`
/// requires. This *is* a user-facing query, and its miss rate is the evidence gate
/// for what to build next, so excluding it would hide the one number the feature
/// exists to produce. Only answered resolutions log; a 400/401/429/503 is a
/// rejected request, not a resolution outcome, and counting those would inflate
/// the miss bucket with client bugs.
///
/// # Orchestration
///
/// 1. **Identifiers, strict grain.** Build an [`EntityRecord`] from the request's
///    `identifiers` and commit it with `allow_affiliation_only: false`.
///    - `Hit`/`New` → answer. A record carrying a brand domain *plus* a Michelin
///      deep link resolves right here; that asymmetry is the design.
///    - `Unresolved` **with** candidates (`ambiguous_among_n`) → answer; the caller
///      asks its user to pick one.
///    - `Unresolved` with an empty candidate list (`needs_stronger_identifier`) →
///      fall through to (2). Nothing was written.
/// 2. **Name search** — local graph first, hub on miss, result cached. This is
///    where a chain's location list actually comes from.
///
/// Two traps, both load-bearing:
///
/// * `CommitOpts::default()` is **permissive** (`allow_affiliation_only: true`),
///   which is what keeps `/ingest` and seeding working. This route must pass
///   `false` explicitly; forgetting the field is a silent regression to the
///   original bug (a chain's brand domain minting a brand-level entity), not a
///   compile error.
/// * Step 1 **cannot** produce a multi-location list. `GraphStore::find` returns
///   `Option<String>` — a strong key has exactly one owner — and "don't steal a
///   shared domain" means a second location never acquires the brand domain. So
///   one brand domain reaches at most one cluster and the realistic chain case is
///   `AmbiguousAmongN(1)`. The location picker comes from step 2.
pub async fn resolve_name(req: &mut Request, env: &Env) -> Result<Response> {
    let body = req.text().await?;
    let parsed = match NameRequest::parse(&body) {
        Ok(p) => p,
        Err(msg) => return error_json(&msg, "invalid_input", 400),
    };
    let g = store(env)?;
    let input_desc = parsed.describe();

    // --- Step 1: the request's own identifiers, under the strict grain rule.
    let mut refused_by_identifiers: Option<ResolveOutput> = None;
    if !parsed.ids.is_empty() {
        let record = EntityRecord {
            entity_type: parsed.query.entity_type.clone(),
            name: parsed.query.name.clone(),
            same_as: parsed.ids.clone(),
        };
        let out = match commit_record_with_opts(
            &g,
            &record,
            "resolve_name",
            // EXPLICITLY strict. See the trap note above.
            CommitOpts {
                allow_affiliation_only: false,
            },
        )
        .await
        {
            Ok(out) => out,
            Err(e) => return core_error(e),
        };
        // "Unresolved with an empty candidate list" is the frozen fall-through
        // signal: both `needs_stronger_identifier` shapes (an identity-less
        // affiliation cluster, and no cluster at all) demand the same caller
        // action, and `AmbiguousAmongN` is only ever constructed with a non-empty
        // list — so this test is exhaustive by construction.
        let fall_through = out.status == Status::Unresolved && out.candidates.is_empty();
        if !fall_through {
            return answer(&g, &parsed, out, Meta::local(Step::Identifiers), &input_desc).await;
        }
        refused_by_identifiers = Some(out);
    }
    // Carried onto whatever step 2 answers with: it is the "your identifiers name
    // a brand, not one thing" sentence, and the consumer's user-facing message
    // needs it even when the candidates come from the name search.
    let identifier_hint = refused_by_identifiers
        .as_ref()
        .and_then(|out| out.hint.clone());

    // --- Step 2: the name search.
    if parsed.query.match_name().is_empty() {
        // No usable name to search by (absent, or punctuation that normalizes to
        // nothing). Hand back step 1's refusal rather than inventing a verdict.
        return match refused_by_identifiers {
            Some(out) => answer(&g, &parsed, out, Meta::local(Step::Identifiers), &input_desc).await,
            // Unreachable: `NameRequest::parse` rejects a body with neither a name
            // nor a usable identifier. Kept total rather than `unreachable!()` —
            // a panic in wasm is a 500 with no body.
            None => error_json(
                "nothing to resolve: `name` did not survive normalization and no usable \
                 `identifiers` were supplied",
                "invalid_input",
                400,
            ),
        };
    }

    // 2a. Local graph first — a repeat query costs zero external calls. Run
    //     explicitly (rather than leaning on `resolve_name`'s own local-first
    //     step) so the budget is spent only when we are genuinely about to reach
    //     out. `resolve_name` repeats this lookup; that is a couple of indexed D1
    //     reads against the alternative of billing a cached query.
    match resolve_name_local(&g, &parsed.query).await {
        Ok(Some(out)) => {
            let meta = Meta::local(Step::NameLocal).with_identifier_hint(identifier_hint);
            return answer(&g, &parsed, out, meta, &input_desc).await;
        }
        Ok(None) => {}
        Err(e) => return core_error(e),
    }

    // 2b. A hub call is now on the table. Route by entity type, then check that
    //     the hub is actually configured — before touching the budget.
    let hub = name_hub_for(parsed.query.entity_type.as_deref());
    let keys = HubKeys::read(env);
    if let Some(secret) = keys.missing_for(hub) {
        let mut out = name_not_found(&parsed.query);
        out.hint = Some(format!(
            "not in the local graph, and the {} name search is not configured on this \
             deployment (missing secret {secret}), so no lookup was attempted. Supply an \
             identifier for the individual thing: a location-specific page URL with a path, a \
             Yelp /biz/ link, or a Google Maps place URL.",
            hub.tag()
        ));
        return answer(
            &g,
            &parsed,
            out,
            Meta {
                step: Step::NameSearch,
                hub: Some(hub),
                hub_called: false,
                hub_calls_today: None,
                identifier_hint,
            },
            &input_desc,
        )
        .await;
    }

    // 2c. Per-caller daily budget. Fail CLOSED on any failure to account: an
    //     unreadable budget table means unmetered spend on a billable hub.
    //
    //     **A failed hub call is NOT refunded.** Now that `hub_error` exists the
    //     refund is finally implementable — the failure is observable at exactly
    //     the point the reservation is held — so this is a decision, not an
    //     omission. Three reasons it stays a charge:
    //
    //       * The counter meters *attempts to spend*, not answers received. It
    //         cannot know what was billed: Google bills Text Search per accepted
    //         request, the classes we would refund (403, 429, 5xx, decode) are the
    //         ones whose billing status we are least sure of, and a decode error
    //         is a call that was served and paid for. Refunding on our own read of
    //         the response is guessing at Google's invoice.
    //       * Refunding makes a *broken* hub the cheapest thing to call. Every
    //         retry against a 403ing key would cost zero budget, so the one
    //         failure mode where a client will retry hardest is the one with no
    //         backpressure at all — burning subrequests and `FetchTransport`'s
    //         retry/backoff wall clock inside every request, unbounded.
    //       * The cost of NOT refunding is bounded and now diagnosable: the caller
    //         loses some of a daily allowance, sees `hub_error` in the same
    //         response saying why, and the operator sees the `console_warn!`. The
    //         cost of refunding is an unbounded loop against a hub we cannot bill
    //         for. Bounded-and-visible beats unbounded-and-quiet.
    //
    //     The lever for the "our bug ate the user's quota" case is deliberately
    //     operational rather than automatic: fix the key, or set
    //     `HUB_DAILY_BUDGET = "0"` to stop the bleeding, then let the UTC-day
    //     rollover restore the allowance.
    let configured = env.var("HUB_DAILY_BUDGET").ok().map(|v| v.to_string());
    let limit = budget::parse_limit(configured.as_deref());
    let day = budget::utc_day(Date::now().as_millis() as f64);
    let used = match budget::reserve(&env.d1("DB")?, &parsed.bucket, day, limit).await {
        Ok(Reservation::Granted { used }) => used,
        Ok(Reservation::Exhausted) => {
            return error_json(
                &format!(
                    "daily hub-call budget exhausted for this caller ({limit}/day). Retry after \
                     00:00 UTC, or resolve with a stronger identifier — identifier lookups and \
                     repeat name queries are served locally and are never budgeted."
                ),
                "quota_exhausted",
                429,
            )
        }
        Err(e) => {
            return error_json(
                &format!("hub-call budget unavailable, so no hub call was made: {e:#}"),
                "quota_unavailable",
                503,
            )
        }
    };

    // 2d. Reach out. `resolve_name` writes the verdict into the local name index
    //     and cardinality memory, so the next identical query is local.
    let mut ctx = CompletionCtx::new(Arc::new(FetchTransport::new()));
    ctx.google_key = keys.google;
    ctx.tmdb_key = keys.tmdb;
    ctx.placekey_key = keys.placekey;

    match sameas_core::resolve_name(&g, &parsed.query, &ctx).await {
        Ok(out) => {
            // A hub failure comes back as `Ok` — it is non-fatal by contract — so
            // without this line the *only* record of an outage would be in the
            // response body of whoever happened to trigger it. Warn-level because
            // it is actionable and rare: a burst of these means a key is dead, an
            // egress IP is blocked, or a hub is down, and every one of them burned
            // a caller's budget for nothing.
            //
            // `hub_calls_today` is included deliberately: it is the number that
            // says how much of this bucket's day our outage has already consumed,
            // and it is what justifies reaching for the `HUB_DAILY_BUDGET = "0"`
            // kill switch while the hub is broken.
            //
            // The message is pre-redacted in `sameas-core` (`transport::redact_url`),
            // so no API key can reach the log. `input_desc` is the same string
            // already written to `resolutions`, so this adds no new PII surface.
            if let Some(err) = &out.hub_error {
                console_warn!(
                    "hub_error hub={} bucket={} calls_today={} input=[{}] status={} err={}",
                    hub.tag(),
                    parsed.bucket,
                    used,
                    input_desc,
                    out.status.as_str(),
                    err
                );
            }
            answer(
                &g,
                &parsed,
                out,
                Meta {
                    step: Step::NameSearch,
                    hub: Some(hub),
                    hub_called: true,
                    hub_calls_today: Some(used),
                    identifier_hint,
                },
                &input_desc,
            )
            .await
        }
        Err(e) => core_error(e),
    }
}

/// Log the outcome and render it: the core's resolve document plus this route's
/// orchestration facts.
async fn answer(
    g: &D1Store,
    parsed: &NameRequest,
    out: ResolveOutput,
    meta: Meta,
    input_desc: &str,
) -> Result<Response> {
    log_resolution(g, &out, input_desc).await;
    let mut doc = resolve_output_json(&out, NAME_ACTION);
    if let Some(obj) = doc.as_object_mut() {
        obj.insert("resolved_by".into(), json!(meta.step.tag()));
        obj.insert("name_hub".into(), json!(meta.hub.map(|h| h.tag())));
        obj.insert("hub_called".into(), json!(meta.hub_called));
        obj.insert("identifier_hint".into(), json!(meta.identifier_hint));
        // Which `identifiers` entries no kind recognized. Reported rather than
        // dropped silently, so a caller sending an unsupported link can see why
        // it did not count toward the verdict.
        obj.insert("ignored_identifiers".into(), json!(parsed.ignored));
        if let Some(used) = meta.hub_calls_today {
            obj.insert("hub_calls_today".into(), json!(used));
        }
    }
    ok_json(&doc)
}

/// Build the single [`ExternalId`] a request is asking about.
///
/// Accepts the generic `?id=kind:value` form plus a per-kind shorthand for every
/// registered kind (`?domain=`, `?wikidata=`, …). The shorthand is derived from the
/// `KINDS` registry rather than hard-coded, so a new identifier kind works here with
/// no change to this file — the same property the CLI's `--id` flag has.
fn parse_identifier(params: &[(String, String)]) -> std::result::Result<ExternalId, String> {
    if let Some((_, raw)) = params.iter().find(|(k, _)| k == "id") {
        let (tag, value) = raw
            .split_once(':')
            .ok_or_else(|| format!("`id` must be KIND:VALUE, got {raw:?}"))?;
        return ExternalId::new(tag, value).map_err(|e| format!("{e:#}"));
    }
    // Per-kind shorthand, driven by the registry.
    for spec in sameas_core::KINDS {
        if let Some((_, raw)) = params.iter().find(|(k, _)| k == spec.tag) {
            return ExternalId::new(spec.tag, raw).map_err(|e| format!("{e:#}"));
        }
    }
    // `place_id` is the CLI's spelling for `google_place_id`; accept both.
    if let Some((_, raw)) = params.iter().find(|(k, _)| k == "place_id") {
        return ExternalId::new("google_place_id", raw).map_err(|e| format!("{e:#}"));
    }
    let kinds: Vec<&str> = sameas_core::KINDS.iter().map(|k| k.tag).collect();
    Err(format!(
        "supply an identifier: ?id=KIND:VALUE, or ?<kind>=VALUE for one of: {}",
        kinds.join(", ")
    ))
}

/// `GET /entity/<canonical_id>` — read-only cluster load.
pub async fn entity(canonical_id: &str, env: &Env) -> Result<Response> {
    if canonical_id.is_empty() {
        return error_json("missing canonical id", "invalid_input", 400);
    }
    let g = store(env)?;
    match load_entity(&g, canonical_id).await {
        Ok(out) => ok_json(&resolve_output_json(&out, "entity")),
        // An unknown id is a 404, not a 500. `load_entity` reports it as
        // "no entity with canonical_id {id}" (resolve.rs).
        Err(e) => {
            let msg = format!("{e:#}");
            if msg.contains("no entity with canonical_id") {
                error_json(&msg, "not_found", 404)
            } else {
                core_error(e)
            }
        }
    }
}

/// `GET /stats` — the exact/hub/miss breakdown backing the miss-rate metric.
pub async fn stats(env: &Env) -> Result<Response> {
    let g = store(env)?;
    match g.stats().await {
        Ok(r) => {
            let by_reason: serde_json::Map<String, serde_json::Value> = r
                .by_reason
                .iter()
                .map(|(tag, n)| (tag.clone(), json!(n)))
                .collect();
            ok_json(&json!({
                "total": r.total,
                "exact": r.exact,
                "hub": r.hub,
                "miss": r.miss,
                "miss_rate": sameas_core::json::round2(r.miss_rate() as f32),
                "by_reason": by_reason,
                "entities": r.entities,
                "edges": r.edges,
            }))
        }
        Err(e) => core_error(e),
    }
}

/// `POST /__conformance` — run the backend-agnostic [`GraphStore`] contract suite
/// against the live D1 binding.
///
/// `sameas_core::store::conformance` is declared unconditionally in `store/mod.rs`
/// (unlike its `d1`/`sqlite` siblings) and its body calls only trait methods, so it
/// compiles under `default-features = false, features = ["d1"]` and is reachable
/// from here with no change to the core.
///
/// **The suite requires an EMPTY store** — its cases delete and split their own
/// fixtures, so a second run against dirty state fails. The caller is responsible:
/// `test/helpers.ts`'s `resetDb()` in a `beforeEach`. That, plus the fact that it
/// writes, is why this route is `#[cfg]`-gated out of every deploy.
#[cfg(feature = "test-endpoints")]
pub async fn conformance(env: &Env) -> Result<Response> {
    let g = store(env)?;
    match sameas_core::store::conformance::run_all(&g).await {
        Ok(()) => ok_json(&json!({ "conformance": "ok" })),
        // Carry the assertion message through, or a failure is an opaque 500.
        Err(e) => error_json(&format!("{e:#}"), "conformance_failed", 500),
    }
}

/// `POST /ingest` — commit a schema.org-style seed record (typed `sameAs` array).
///
/// The write endpoint, so it is token-gated. Body shape matches the CLI's seed
/// files, which is what `EntityRecord`'s `Deserialize` already accepts.
pub async fn ingest(req: &mut Request, env: &Env) -> Result<Response> {
    let body = req.text().await?;
    // `from_json_str` applies the same registry dispatch + validation the CLI's seed
    // loader uses, so a `{"yelp": "..."}` entry works here with no per-kind code.
    let record = match EntityRecord::from_json_str(&body) {
        Ok(r) => r,
        Err(e) => return error_json(&format!("invalid record JSON: {e:#}"), "invalid_input", 400),
    };
    if record.same_as.is_empty() {
        return error_json(
            "record carries no `sameAs` identifiers — nothing to resolve",
            "invalid_input",
            400,
        );
    }
    let g = store(env)?;
    match commit_record(&g, &record).await {
        Ok(out) => ok_json(&resolve_output_json(&out, "ingest")),
        Err(e) => core_error(e),
    }
}

#[cfg(test)]
mod parse_identifier_tests {
    use super::parse_identifier;

    /// Terse wrapper: `parse_identifier` takes owned pairs as they arrive from a
    /// query string.
    fn p(pairs: &[(&str, &str)]) -> std::result::Result<String, String> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        parse_identifier(&owned).map(|id| id.key())
    }

    /// A value each registered kind's normalizer accepts, so the registry sweep
    /// below tests reachability rather than normalization.
    fn sample_for(tag: &str) -> &'static str {
        match tag {
            "domain" => "example.com",
            "google_place_id" => "ChIJN1t_tDeuEmsRUsoyG83frY4",
            "imdb" => "tt0133093",
            "phone" => "+15106533394",
            "wikidata" => "Q42",
            "tmdb" => "603",
            "yelp" => "blue-bottle-coffee-san-francisco",
            "placekey" => "227-222@5vg-82n-kzz",
            // Path-bearing on purpose: `normalize::specific_url` rejects a bare
            // host, which is the whole point of the `url` kind being Identity.
            "url" => "https://example.com/some/page",
            other => panic!("no sample value for kind {other:?} — add one"),
        }
    }

    #[test]
    fn generic_id_form() {
        assert_eq!(p(&[("id", "wikidata:Q42")]).unwrap(), "wikidata:Q42");
    }

    #[test]
    fn id_without_a_colon_is_rejected() {
        assert!(p(&[("id", "Q42")]).unwrap_err().contains("KIND:VALUE"));
    }

    #[test]
    fn per_kind_shorthand() {
        assert_eq!(p(&[("wikidata", "Q42")]).unwrap(), "wikidata:Q42");
    }

    #[test]
    fn place_id_aliases_google_place_id() {
        // The CLI spells it `--place-id`; both must work.
        assert_eq!(
            p(&[("place_id", "ChIJN1t_tDeuEmsRUsoyG83frY4")]).unwrap(),
            "google_place_id:ChIJN1t_tDeuEmsRUsoyG83frY4"
        );
    }

    #[test]
    fn explicit_id_wins_over_shorthand() {
        let got = p(&[("wikidata", "Q1"), ("id", "wikidata:Q42")]).unwrap();
        assert_eq!(got, "wikidata:Q42");
    }

    #[test]
    fn missing_identifier_lists_every_kind() {
        let err = p(&[("nope", "x")]).unwrap_err();
        for spec in sameas_core::KINDS {
            assert!(err.contains(spec.tag), "the error must name {}", spec.tag);
        }
    }

    #[test]
    fn shorthand_is_registry_driven_for_every_kind() {
        // The property the doc comment claims: EVERY registered kind is reachable
        // by shorthand with no per-kind code here. Fails if someone hard-codes a
        // subset, and fails if a new kind is added without a sample above.
        for spec in sameas_core::KINDS {
            let got = p(&[(spec.tag, sample_for(spec.tag))]);
            assert!(
                got.is_ok(),
                "kind {} is unreachable by shorthand: {:?}",
                spec.tag,
                got.unwrap_err()
            );
            assert!(got.unwrap().starts_with(spec.tag));
        }
    }

    #[test]
    fn an_invalid_value_reports_the_normalizer_error() {
        // Not a parse failure — a normalization failure, surfaced verbatim.
        let err = p(&[("phone", "not-a-phone")]).unwrap_err();
        assert!(err.contains("phone"), "unexpected error: {err}");
    }
}
