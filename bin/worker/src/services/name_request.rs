//! The `POST /resolve/name` request DTO, and its translation into core types.
//!
//! **Why the DTO lives here and not in `sameas-core`.** The core's
//! [`NameQuery`](sameas_core::NameQuery) deliberately derives neither
//! `Serialize` nor `Deserialize`: a wire format is a front-end contract, and
//! deriving one on a domain type makes every field rename a breaking API change
//! (the same reasoning as `json::resolve_output_json`, which is hand-built rather
//! than derived). It also has **no `sameAs` field** — identifiers do not belong to
//! a *name* query at all. They ride separately in this DTO because they drive a
//! different, earlier step of the route's orchestration (the strict-grain commit),
//! and only a miss there falls through to the name search.
//!
//! ## Identifier translation
//!
//! agent-web speaks schema.org `sameAs` **URLs**; sameas speaks `kind:value`. The
//! translation lives on this side because sameas owns the kind registry
//! ([`sameas_core::KINDS`]). Two input forms are accepted, in this order:
//!
//! 1. `kind:value` — a *registered* kind tag before the colon. This is the form a
//!    candidate hands back as `ref`, so echoing a `ref` verbatim on a retry
//!    resolves. Unregistered prefixes (`https`, `mailto`, …) fall through to (2)
//!    rather than erroring, which is what keeps `https://…` from being read as
//!    kind `https`.
//! 2. A raw URL — [`guess_id_from_url`], the same registry-driven mapping the
//!    harvester uses. It is `pub` and NOT behind the `harvest` feature precisely
//!    so this worker (built without `harvest`, which would drag `scraper`'s parser
//!    tree into wasm) can call it.
//!
//! Anything neither form recognizes is **dropped, not rejected**: one unusable
//! `sameAs` entry (a Facebook page, say) must not fail a request that also carries
//! a Yelp link. Dropped entries are reported back in `ignored_identifiers` so the
//! caller can see what was not used instead of guessing.

use sameas_core::resolve::guess_id_from_url;
use sameas_core::{spec_for_tag, ExternalId, NameQuery};
use serde::Deserialize;

/// Caps on the request. Not tuning knobs — they bound the work one unauthenticated-
/// shaped body can ask for (the route is token-gated, but a compromised or buggy
/// consumer is still the threat model, and every identifier is a D1 round trip).
pub const MAX_IDENTIFIERS: usize = 32;
pub const MAX_QUALIFIERS: usize = 16;
/// Per-string cap, applied to every text field. Generous enough for a long URL.
pub const MAX_STRING: usize = 512;

/// The raw request body.
///
/// Unknown fields are ignored rather than rejected: the consumer and this worker
/// deploy independently (agent-web auto-deploys on merge; sameas is deployed by
/// hand), so a consumer that starts sending a field this version does not know
/// must not start failing.
#[derive(Debug, Default, Deserialize)]
struct RawNameRequest {
    #[serde(default)]
    name: Option<String>,
    /// The NSID **leaf** (`restaurant`, `movie`); a full NSID is accepted too and
    /// `name_hub_for` takes the leaf. `type` is accepted as an alias because that
    /// is what the record itself calls it.
    #[serde(default, alias = "type")]
    entity_type: Option<String>,
    /// The quota bucket. Opaque — see [`NameRequest::bucket`].
    #[serde(default)]
    publisher_did: Option<String>,
    #[serde(default)]
    street: Option<String>,
    #[serde(default)]
    city: Option<String>,
    #[serde(default)]
    region: Option<String>,
    #[serde(default)]
    country: Option<String>,
    #[serde(default)]
    qualifiers: Vec<String>,
    /// Raw `sameAs` URLs **or** `kind:value` refs, mixed freely.
    #[serde(default)]
    identifiers: Vec<String>,
}

/// A validated request: the core query, the translated identifiers, and the
/// opaque quota bucket.
#[derive(Debug)]
pub struct NameRequest {
    pub query: NameQuery,
    /// Translated, deduped, order-preserving. May be empty (a pure name query).
    pub ids: Vec<ExternalId>,
    /// Identifier strings no kind recognized, echoed back for transparency.
    pub ignored: Vec<String>,
    /// The daily hub-call quota key. **Opaque**: sameas neither parses nor
    /// validates it, and stores it in `hub_budget` and nowhere else. The consumer
    /// happens to pass a DID; this system must not know what a DID is
    /// (PROJECT_GOALS non-goal #3).
    pub bucket: String,
}

/// Trim, and treat an all-whitespace string as absent — a consumer templating an
/// empty city into the body must not create a qualifier token of `""`.
fn clean(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn too_long(v: &Option<String>) -> bool {
    v.as_deref().map(|s| s.len() > MAX_STRING).unwrap_or(false)
}

impl NameRequest {
    /// Parse and validate a request body. `Err` carries a caller-facing message
    /// for a `400 invalid_input`.
    pub fn parse(body: &str) -> Result<NameRequest, String> {
        let raw: RawNameRequest =
            serde_json::from_str(body).map_err(|e| format!("invalid request JSON: {e}"))?;

        let bucket = clean(raw.publisher_did).ok_or_else(|| {
            "`publisher_did` is required: it is the per-caller daily hub-call quota bucket \
             (an opaque string — any stable per-caller value works)"
                .to_string()
        })?;
        if bucket.len() > MAX_STRING {
            return Err(format!("`publisher_did` exceeds {MAX_STRING} characters"));
        }

        if raw.identifiers.len() > MAX_IDENTIFIERS {
            return Err(format!(
                "too many `identifiers`: {} (max {MAX_IDENTIFIERS})",
                raw.identifiers.len()
            ));
        }
        if raw.qualifiers.len() > MAX_QUALIFIERS {
            return Err(format!(
                "too many `qualifiers`: {} (max {MAX_QUALIFIERS})",
                raw.qualifiers.len()
            ));
        }

        let name = clean(raw.name);
        let entity_type = clean(raw.entity_type);
        let street = clean(raw.street);
        let city = clean(raw.city);
        let region = clean(raw.region);
        let country = clean(raw.country);
        for (field, value) in [
            ("name", &name),
            ("entity_type", &entity_type),
            ("street", &street),
            ("city", &city),
            ("region", &region),
            ("country", &country),
        ] {
            if too_long(value) {
                return Err(format!("`{field}` exceeds {MAX_STRING} characters"));
            }
        }

        let mut qualifiers: Vec<String> = Vec::new();
        for q in raw.qualifiers {
            let q = q.trim().to_string();
            if q.is_empty() {
                continue;
            }
            if q.len() > MAX_STRING {
                return Err(format!("a `qualifiers` entry exceeds {MAX_STRING} characters"));
            }
            qualifiers.push(q);
        }

        let mut ids: Vec<ExternalId> = Vec::new();
        let mut ignored: Vec<String> = Vec::new();
        for raw_id in raw.identifiers {
            let trimmed = raw_id.trim();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed.len() > MAX_STRING {
                return Err(format!("an `identifiers` entry exceeds {MAX_STRING} characters"));
            }
            match parse_identifier_ref(trimmed) {
                // Deduped: two spellings of one identifier (a Yelp URL and the
                // `yelp:` ref for the same slug) normalize to one key, and a
                // duplicate would only buy an extra `graph.find` round trip.
                Some(id) => {
                    if !ids.contains(&id) {
                        ids.push(id);
                    }
                }
                None => ignored.push(trimmed.to_string()),
            }
        }

        if name.is_none() && ids.is_empty() {
            return Err(
                "nothing to resolve: supply `name`, or at least one `identifiers` entry that is \
                 a URL or a kind:value ref"
                    .to_string(),
            );
        }

        Ok(NameRequest {
            query: NameQuery {
                name,
                qualifiers,
                entity_type,
                street,
                city,
                region,
                country,
            },
            ids,
            ignored,
            bucket,
        })
    }

    /// A bounded, human-readable description of the input for the `resolutions`
    /// log's `input_desc`. Deliberately excludes `bucket`: the log is a metrics
    /// table, not an audit trail of who asked (PROJECT_GOALS — IDs only).
    pub fn describe(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(n) = &self.query.name {
            parts.push(format!("name={n}"));
        }
        for id in self.ids.iter().take(4) {
            parts.push(id.key());
        }
        let mut desc = parts.join(" ");
        if desc.len() > 200 {
            desc.truncate(200);
        }
        desc
    }
}

/// One identifier string → a typed [`ExternalId`], or `None` when no kind claims
/// it. See the module docs for the two accepted forms and their order.
pub fn parse_identifier_ref(raw: &str) -> Option<ExternalId> {
    if let Some((tag, value)) = raw.split_once(':') {
        // Only a REGISTERED tag is treated as a ref; anything else is a URL
        // scheme (or noise) and belongs to `guess_id_from_url`.
        if spec_for_tag(tag).is_some() {
            // A registered tag whose value the normalizer rejects is a genuine
            // caller error, but dropping it (rather than failing the request)
            // keeps one bad entry from sinking a request that also carries a good
            // one. It surfaces in `ignored_identifiers`.
            return ExternalId::new(tag, value).ok();
        }
    }
    guess_id_from_url(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &str) -> Result<NameRequest, String> {
        NameRequest::parse(body)
    }

    /// The wire example from the frozen contract, verbatim.
    #[test]
    fn parses_the_contract_example() {
        let r = parse(
            r#"{ "name": "Souvla", "entity_type": "restaurant",
                 "publisher_did": "did:plc:abc",
                 "city": "San Francisco", "region": "CA", "country": "US",
                 "identifiers": ["https://souvla.com"],
                 "qualifiers": [] }"#,
        )
        .unwrap();
        assert_eq!(r.query.name.as_deref(), Some("Souvla"));
        assert_eq!(r.query.entity_type.as_deref(), Some("restaurant"));
        assert_eq!(r.bucket, "did:plc:abc");
        assert_eq!(r.ids.len(), 1);
        // A bare origin demotes to the Affiliation-grain `domain` kind — the
        // whole reason the strict commit refuses it.
        assert_eq!(r.ids[0].key(), "domain:souvla.com");
        assert!(r.ignored.is_empty());
    }

    #[test]
    fn a_candidate_ref_round_trips() {
        // The retry path: the caller echoes a candidate's `ref` verbatim, and it
        // must resolve to the SAME key the candidate advertised — otherwise the
        // retry mints a second entity instead of binding to the chosen one.
        let r = parse(
            r#"{"publisher_did":"b","identifiers":["google_place_id:ChIJN1t_tDeuEmsRUsoyG83frY4"]}"#,
        )
        .unwrap();
        assert_eq!(
            r.ids[0].key(),
            "google_place_id:ChIJN1t_tDeuEmsRUsoyG83frY4"
        );
    }

    #[test]
    fn a_candidate_url_round_trips_to_the_same_key_as_its_ref() {
        // U4's Maps `url_match` is what makes these two forms agree. If they ever
        // diverge, a caller that wrote the candidate's `url` into a record and
        // sent it back would mint a NEW entity keyed `url:` beside the real one.
        let by_url = parse(
            r#"{"publisher_did":"b","identifiers":
               ["https://www.google.com/maps/place/?q=place_id:ChIJN1t_tDeuEmsRUsoyG83frY4"]}"#,
        )
        .unwrap();
        assert_eq!(
            by_url.ids[0].key(),
            "google_place_id:ChIJN1t_tDeuEmsRUsoyG83frY4"
        );
    }

    #[test]
    fn urls_and_refs_mix_freely_and_dedupe() {
        let r = parse(
            r#"{"publisher_did":"b","identifiers":[
                 "https://www.yelp.com/biz/souvla-hayes-valley-san-francisco",
                 "yelp:souvla-hayes-valley-san-francisco",
                 "https://souvla.com"]}"#,
        )
        .unwrap();
        let keys: Vec<String> = r.ids.iter().map(|i| i.key()).collect();
        assert_eq!(
            keys,
            vec![
                "yelp:souvla-hayes-valley-san-francisco",
                "domain:souvla.com"
            ]
        );
    }

    #[test]
    fn an_https_url_is_never_read_as_a_kind_named_https() {
        // The ordering trap: `split_once(':')` on a URL yields ("https", "//…").
        // Only a REGISTERED tag may win, or every URL would become garbage.
        let r = parse(r#"{"publisher_did":"b","identifiers":["https://example.com/a/page"]}"#)
            .unwrap();
        assert_eq!(r.ids[0].key(), "url:example.com/a/page");
    }

    #[test]
    fn unrecognized_identifiers_are_reported_not_fatal() {
        // A social link alongside a real one: the request succeeds using the real
        // one, and the caller is told which entry was not used.
        let r = parse(
            r#"{"publisher_did":"b","identifiers":[
                 "https://www.facebook.com/souvla", "not a url at all",
                 "yelp:souvla-hayes-valley-san-francisco"]}"#,
        )
        .unwrap();
        assert_eq!(r.ids.len(), 1);
        assert_eq!(r.ignored.len(), 2);
    }

    #[test]
    fn a_bad_value_for_a_real_kind_is_ignored_not_fatal() {
        let r = parse(
            r#"{"publisher_did":"b","name":"x","identifiers":["phone:not-a-phone"]}"#,
        )
        .unwrap();
        assert!(r.ids.is_empty());
        assert_eq!(r.ignored, vec!["phone:not-a-phone"]);
    }

    #[test]
    fn publisher_did_is_required() {
        let err = parse(r#"{"name":"Souvla"}"#).unwrap_err();
        assert!(err.contains("publisher_did"), "{err}");
        // Whitespace is not a bucket.
        assert!(parse(r#"{"name":"Souvla","publisher_did":"   "}"#).is_err());
    }

    #[test]
    fn a_request_with_neither_a_name_nor_a_usable_identifier_is_rejected() {
        let err = parse(
            r#"{"publisher_did":"b","identifiers":["https://twitter.com/souvla"]}"#,
        )
        .unwrap_err();
        assert!(err.contains("nothing to resolve"), "{err}");
    }

    #[test]
    fn identifiers_alone_are_enough() {
        assert!(parse(r#"{"publisher_did":"b","identifiers":["wikidata:Q42"]}"#).is_ok());
    }

    #[test]
    fn empty_strings_do_not_become_qualifier_tokens() {
        // A consumer templating a missing city into the body would otherwise
        // establish the entity under a junk token and poison the name index.
        let r = parse(
            r#"{"publisher_did":"b","name":"Souvla","city":"  ","qualifiers":["", " "]}"#,
        )
        .unwrap();
        assert!(r.query.city.is_none());
        assert!(r.query.qualifiers.is_empty());
        assert!(r.query.establishing_qualifiers().is_empty());
    }

    #[test]
    fn caps_are_enforced() {
        let many = (0..MAX_IDENTIFIERS + 1)
            .map(|i| format!("\"wikidata:Q{i}\""))
            .collect::<Vec<_>>()
            .join(",");
        let err = parse(&format!(
            r#"{{"publisher_did":"b","identifiers":[{many}]}}"#
        ))
        .unwrap_err();
        assert!(err.contains("too many `identifiers`"), "{err}");

        let long = "x".repeat(MAX_STRING + 1);
        assert!(parse(&format!(r#"{{"publisher_did":"b","name":"{long}"}}"#)).is_err());
    }

    #[test]
    fn unknown_fields_are_ignored() {
        // Independent deploy cadences: a newer consumer must not 400 here.
        assert!(parse(r#"{"publisher_did":"b","name":"x","future_field":[1,2]}"#).is_ok());
    }

    #[test]
    fn type_is_an_accepted_alias_for_entity_type() {
        let r = parse(r#"{"publisher_did":"b","name":"Avatar","type":"movie"}"#).unwrap();
        assert_eq!(r.query.entity_type.as_deref(), Some("movie"));
    }

    #[test]
    fn describe_is_bounded_and_omits_the_bucket() {
        let r = parse(
            r#"{"publisher_did":"did:plc:secret","name":"Souvla",
                "identifiers":["yelp:souvla-hayes-valley-san-francisco"]}"#,
        )
        .unwrap();
        let d = r.describe();
        assert!(d.contains("name=Souvla"));
        assert!(d.contains("yelp:"));
        assert!(!d.contains("did:plc:secret"), "the log must not carry the bucket");
        assert!(d.len() <= 200);
    }
}
