//! The JSON wire shape for a [`ResolveOutput`].
//!
//! Lives in the core (rather than in the CLI, where it started) so every front-end
//! — the `sameas` CLI's `--json`, and the HTTP Worker — emits the *same* document
//! for the same resolution. Two hand-rolled copies would drift.
//!
//! Deliberately hand-built rather than `#[derive(Serialize)]`: the wire shape is a
//! public contract that differs from the in-memory struct on purpose (`sameAs` is a
//! flat `kind:value` array, not typed `ExternalId`s, and is accompanied by a
//! `sameAs_urls` projection that has no in-memory counterpart; `confidence` is rounded;
//! `provenance` is an object, not a pair list; `status`/`confidence_reason` are
//! stable string tags). Deriving would leak the internal layout and make any
//! refactor a breaking API change.

use std::collections::HashSet;

use serde_json::{json, Map, Value};

use crate::confidence::reason_tag;
use crate::kind::url_for_key;
use crate::resolve::ResolveOutput;

/// Round a confidence to 2 decimals for stable, human-friendly output. The core
/// struct keeps full `f32` precision; only the wire form is rounded.
pub fn round2(x: f32) -> f64 {
    (x as f64 * 100.0).round() / 100.0
}

/// The canonical JSON document for a resolution.
///
/// `action` labels which operation produced it (`"resolve"`, `"entity"`,
/// `"ingest"`), so a client can tell a lookup from a write.
pub fn resolve_output_json(out: &ResolveOutput, action: &str) -> Value {
    let same_as: Vec<String> = out.same_as.iter().map(|i| i.key()).collect();
    // The same identifiers projected to canonical public URLs — a NEW field
    // *alongside* `sameAs`, not a replacement for it.
    //
    // `sameAs` stays the wire key: it is what a caller echoes back on a retry
    // and what `provenance` is keyed on. But a flat `kind:value` has no scheme,
    // and a consumer that writes identifiers into a schema.org record needs a
    // URL — one whose absence would silently defeat clustering (the records
    // would share an identifier and still never collapse). Hence both forms.
    //
    // Kinds with no canonical URL form (`placekey`, `domain`, `phone`) are
    // omitted rather than faked; see `KindSpec::to_url`. So this array is a
    // subset of `sameAs` and may be empty even when `sameAs` is not.
    let mut seen_urls: HashSet<String> = HashSet::new();
    let same_as_urls: Vec<String> = out
        .same_as
        .iter()
        .filter_map(|i| i.spec().to_url.and_then(|f| f(i.value())))
        .filter(|u| seen_urls.insert(u.clone()))
        .collect();
    let provenance: Map<String, Value> = out
        .provenance
        .iter()
        .map(|(key, source)| {
            (
                key.clone(),
                Value::String(source.clone().unwrap_or_else(|| "unknown".into())),
            )
        })
        .collect();
    let candidates: Vec<Value> = out
        .candidates
        .iter()
        .map(|c| {
            json!({
                "canonical_id": c.canonical_id,
                "anchor": c.anchor,
                "name": c.name,
                // The anchor projected to a URL, for the same reason as
                // `sameAs_urls`: a caller echoes `anchor` back verbatim to bind
                // to this candidate, but writes `url` into the record. `null`
                // when the anchor's kind has no URL form.
                "url": url_for_key(&c.anchor),
            })
        })
        .collect();
    json!({
        "action": action,
        "canonical_id": out.canonical_id,
        "anchor": out.anchor,
        "type": out.entity_type,
        "name": out.name,
        "status": out.status.as_str(),
        "confidence": round2(out.confidence),
        "confidence_reason": reason_tag(&out.confidence_reason),
        "matched_via": out.matched_via,
        "hint": out.hint,
        // ADDITIVE, and `null` on the overwhelming majority of answers. Present
        // means an external hub call failed while producing this document: the
        // answer stands, but it was computed on less than the full evidence.
        //
        // Emitted unconditionally (rather than omitted when absent) for the same
        // reason `sameAs_urls` is: a consumer must be able to tell "this server
        // reports hub failures and there was none" from "this server predates the
        // field", and `undefined` conflates them.
        //
        // Already secret-redacted at the transport — see `transport::redact_url`.
        "hub_error": out.hub_error,
        "sameAs": same_as,
        "sameAs_urls": same_as_urls,
        "provenance": provenance,
        "candidates": candidates,
        "completion_count": same_as.len(),
        "harvested": out.harvested,
        "new_edges": out.new_edges,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::confidence::{score, ConfidenceReason};
    use crate::model::ExternalId;
    use crate::resolve::{Candidate, Status};

    fn output(same_as: Vec<ExternalId>, candidates: Vec<Candidate>) -> ResolveOutput {
        let reason = ConfidenceReason::ExactStrongKey;
        ResolveOutput {
            canonical_id: Some("ent_1".into()),
            anchor: "google_place_id:ChIJsouvla".into(),
            entity_type: Some("restaurant".into()),
            name: Some("Souvla".into()),
            same_as,
            matched_via: vec![],
            status: Status::Hit,
            harvested: 0,
            new_edges: 0,
            confidence: score(&reason),
            confidence_reason: reason,
            candidates,
            hint: None,
            provenance: vec![],
            hub_error: None,
        }
    }

    #[test]
    fn same_as_urls_is_additive_not_a_replacement() {
        // `sameAs` stays the wire key (echoed on a retry, keyed by
        // `provenance`); `sameAs_urls` is the form a consumer writes into a
        // schema.org record. Both must be present.
        let out = output(
            vec![
                ExternalId::google_place_id("ChIJsouvla").unwrap(),
                ExternalId::yelp("souvla-hayes-valley-san-francisco").unwrap(),
                ExternalId::new("placekey", "223-227@5vg-7gq-tvz").unwrap(),
                ExternalId::domain("souvla.com").unwrap(),
            ],
            vec![],
        );
        let v = resolve_output_json(&out, "resolve");

        assert_eq!(
            v["sameAs"].as_array().unwrap().len(),
            4,
            "the kind:value array must be untouched"
        );
        assert_eq!(v["completion_count"], 4);

        // Only the two kinds with a URL form project; placekey and domain are
        // omitted by design, so this is a strict subset.
        let urls: Vec<&str> = v["sameAs_urls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|u| u.as_str().unwrap())
            .collect();
        assert_eq!(
            urls,
            vec![
                "https://www.google.com/maps/place/?q=place_id:ChIJsouvla",
                "https://www.yelp.com/biz/souvla-hayes-valley-san-francisco",
            ]
        );
    }

    #[test]
    fn same_as_urls_is_present_and_empty_when_nothing_projects() {
        // A record identified only by a brand domain yields no URLs at all —
        // the field must still exist as an empty array rather than vanish, so a
        // consumer can distinguish "nothing projected" from "old server".
        let out = output(vec![ExternalId::domain("souvla.com").unwrap()], vec![]);
        let v = resolve_output_json(&out, "resolve");
        assert_eq!(v["sameAs"].as_array().unwrap().len(), 1);
        assert_eq!(v["sameAs_urls"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn candidates_carry_both_the_echo_ref_and_a_url() {
        // A caller shows `url` to the user and echoes `anchor` back verbatim to
        // bind; a candidate whose anchor has no URL form reports `null` rather
        // than a fabricated link.
        let out = output(
            vec![],
            vec![
                Candidate {
                    canonical_id: "ent_hayes".into(),
                    anchor: "google_place_id:ChIJhayes".into(),
                    name: Some("Souvla — Hayes Valley".into()),
                },
                Candidate {
                    canonical_id: "ent_marina".into(),
                    anchor: "placekey:223-227@5vg-7gq-tvz".into(),
                    name: Some("Souvla — Marina".into()),
                },
            ],
        );
        let v = resolve_output_json(&out, "resolve");
        let c = v["candidates"].as_array().unwrap();
        assert_eq!(c[0]["anchor"], "google_place_id:ChIJhayes");
        assert_eq!(
            c[0]["url"],
            "https://www.google.com/maps/place/?q=place_id:ChIJhayes"
        );
        assert!(c[1]["url"].is_null(), "placekey has no URL form");
    }

    #[test]
    fn same_as_urls_dedupes_while_keeping_order() {
        // Two distinct keys can project to the same URL only if a kind ever
        // collapses; guard the invariant anyway so the array a consumer writes
        // into a record never carries a duplicate anchor.
        let out = output(
            vec![
                ExternalId::yelp("a-b").unwrap(),
                ExternalId::new("url", "https://guide.michelin.com/x").unwrap(),
                ExternalId::yelp("a-b").unwrap(),
            ],
            vec![],
        );
        let v = resolve_output_json(&out, "resolve");
        let urls: Vec<&str> = v["sameAs_urls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|u| u.as_str().unwrap())
            .collect();
        assert_eq!(
            urls,
            vec![
                "https://www.yelp.com/biz/a-b",
                "https://guide.michelin.com/x"
            ]
        );
    }
}
