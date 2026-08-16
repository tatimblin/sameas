//! Endpoint handlers. Each opens a [`D1Store`] over the `DB` binding and calls the
//! same `sameas-core` orchestration the CLI uses, so behavior cannot diverge
//! between the two front-ends.

use sameas_core::confidence::reason_tag;
use sameas_core::json::resolve_output_json;
use sameas_core::store::d1::D1Store;
use sameas_core::{commit_record, load_entity, EntityRecord, ExternalId, GraphStore};
use serde_json::json;
use worker::*;

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
            // Log the outcome so `/stats` can report a real miss rate — that metric
            // is the evidence gate for ever adding a fuzzy-matching layer, so a
            // silently empty log would make the decision undecidable. Mirrors the
            // CLI's `record_outcome`: `/entity` and `/ingest` are excluded because a
            // direct id lookup and a seed load are not user-facing *queries* and
            // would skew the rate.
            //
            // Best-effort: a logging failure must never fail the resolution.
            let _ = g
                .record_resolution(
                    out.status.as_str(),
                    reason_tag(&out.confidence_reason),
                    out.matched_via.first().map(|s| s.as_str()),
                    out.confidence,
                    Some(&input_desc),
                )
                .await;
            ok_json(&resolve_output_json(&out, "resolve"))
        }
        Err(e) => core_error(e),
    }
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
