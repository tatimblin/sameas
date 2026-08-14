//! Completion orchestrator — bootstrap missing edges from external hubs.
//!
//! `resolve_and_complete` commits the input locally first (cheap, offline), then
//! runs a bounded BFS over the growing cluster: for each identifier present, it
//! dispatches the applicable **forward** hub adapters, commits what they harvest
//! (which unions into the cluster via the echoed input id), and repeats until no
//! new edges appear or `max_hops` is reached.
//!
//! Only forward completions run speculatively (imdb/wikidata/tmdb crosswalk;
//! place_id → details). Reverse-resolvers (phone/name/address → place_id /
//! Placekey) are **entry-point only** — see [`resolve_name`] — because
//! auto-running them on every domain/phone in a cluster would risk false place
//! edges (a movie's website is not a place).
//!
//! Hub calls are **best-effort**: a hub error (unavailable, no fixture, bad
//! response) yields no completion rather than failing the whole resolution.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Result;

use crate::confidence::{score, ConfidenceReason};
use crate::graph::Graph;
use crate::hubs::{
    PlaceDetailsResolver, PlaceTextSearchResolver, TextSearchInput, TmdbResolver, WikidataResolver,
};
use crate::model::{EntityRecord, ExternalId};
use crate::resolve::{
    commit_record_with_source, load_entity, Candidate, Resolver, ResolveOutput, Status,
};
use crate::transport::HttpTransport;

/// Configuration for a completion run: the transport plus per-hub API keys and
/// the hop cap. Keys are empty in offline/fixture mode (they are stripped from
/// the fixture request signature) and populated from env vars in live mode.
pub struct CompletionCtx {
    pub transport: Arc<dyn HttpTransport>,
    pub tmdb_key: String,
    pub google_key: String,
    pub placekey_key: String,
    pub max_hops: usize,
}

impl CompletionCtx {
    /// A context with no API keys and the default hop cap (3).
    pub fn new(transport: Arc<dyn HttpTransport>) -> Self {
        CompletionCtx {
            transport,
            tmdb_key: String::new(),
            google_key: String::new(),
            placekey_key: String::new(),
            max_hops: 3,
        }
    }
}

/// Forward hubs to try for a given identifier kind (speculative BFS dispatch).
fn hubs_for(kind: &str) -> &'static [&'static str] {
    match kind {
        "imdb" => &["wikidata", "tmdb"],
        "wikidata" => &["wikidata", "tmdb"],
        "tmdb" => &["tmdb"],
        "google_place_id" => &["place_details"],
        _ => &[],
    }
}

/// The "edge is missing" gate: skip a hub when the cluster already carries its
/// target edge(s), keeping completion local-first and idempotent.
fn skip_if_present(hub: &str, members: &[ExternalId]) -> bool {
    let has = |tag: &str| members.iter().any(|m| m.kind_tag() == tag);
    match hub {
        "wikidata" => has("wikidata"),
        "tmdb" => has("tmdb"),
        "place_details" => has("domain") && has("phone"),
        _ => true,
    }
}

fn source_for(hub: &str) -> &'static str {
    match hub {
        "wikidata" => "wikidata",
        "tmdb" => "tmdb",
        "place_details" | "place_text_search" => "google_places",
        _ => "hub",
    }
}

/// Build and run one hub adapter. Returns the harvested record, or `None` when
/// the hub yields nothing or errors (best-effort — never fails the resolution).
fn run_hub(hub: &str, id: &ExternalId, ctx: &CompletionCtx) -> Option<EntityRecord> {
    let harvested: Result<EntityRecord> = match hub {
        "wikidata" => WikidataResolver::new(id.clone(), ctx.transport.clone()).harvest(),
        "tmdb" => {
            TmdbResolver::new(id.clone(), ctx.tmdb_key.clone(), ctx.transport.clone()).harvest()
        }
        "place_details" => {
            PlaceDetailsResolver::new(id.clone(), ctx.google_key.clone(), ctx.transport.clone())
                .harvest()
        }
        _ => return None,
    };
    match harvested {
        Ok(rec) if !rec.same_as.is_empty() => Some(rec),
        _ => None,
    }
}

/// Resolve an input record and bootstrap-complete it from external hubs.
pub fn resolve_and_complete(
    graph: &Graph,
    input: &EntityRecord,
    ctx: &CompletionCtx,
) -> Result<ResolveOutput> {
    // 1. Local-first commit — establishes the canonical id + input confidence.
    let seed = commit_record_with_source(graph, input, "input")?;
    // Refused (no strong key) → nothing resolvable to complete.
    let canonical_id = match &seed.canonical_id {
        Some(c) => c.clone(),
        None => return Ok(seed),
    };
    let mut total_new_edges = seed.new_edges;

    // 2. Bounded BFS over the cluster.
    let mut visited: HashSet<(&'static str, String)> = HashSet::new();
    for _hop in 0..ctx.max_hops {
        let members = graph.members(&canonical_id)?;
        let mut did_work = false;

        for id in &members {
            for &hub in hubs_for(id.kind_tag()) {
                let vkey = (hub, id.key());
                if visited.contains(&vkey) {
                    continue;
                }
                visited.insert(vkey);
                if skip_if_present(hub, &members) {
                    continue;
                }
                if let Some(record) = run_hub(hub, id, ctx) {
                    let out = commit_record_with_source(graph, &record, source_for(hub))?;
                    total_new_edges += out.new_edges;
                    if out.new_edges > 0 {
                        did_work = true;
                    }
                }
            }
        }
        if !did_work {
            break;
        }
    }

    // 3. Reload the final cluster, carrying the input-attachment metadata.
    let mut out = load_entity(graph, &canonical_id)?;
    out.status = seed.status;
    out.matched_via = seed.matched_via;
    out.harvested = seed.harvested;
    out.new_edges = total_new_edges;

    // When a hub crosslink grew the cluster, the completion was driven by a
    // strong-key hub crosswalk (imdb/wikidata/tmdb/place_details). Otherwise
    // keep the seed's own attachment reason.
    let hub_added = total_new_edges > seed.new_edges;
    let reason = if hub_added
        && matches!(
            seed.confidence_reason,
            ConfidenceReason::ExactStrongKey
                | ConfidenceReason::SyntheticStrongKey
                | ConfidenceReason::NewPublicAnchor
        ) {
        ConfidenceReason::HubCrosswalk
    } else {
        seed.confidence_reason.clone()
    };
    out.confidence = score(&reason);
    out.confidence_reason = reason;
    Ok(out)
}

/// Local-only name resolution: answer a name + qualifiers query **from the graph
/// alone, with zero external calls**. Returns `Some(hit)` for a unique match,
/// `Some(unresolved+candidates)` when several distinct entities match (ambiguous
/// — reaching out wouldn't disambiguate what we already know is plural), or
/// `None` on a miss (the caller may then reach out via [`resolve_name`]).
pub fn resolve_name_local(graph: &Graph, query: &NameQuery) -> Result<Option<ResolveOutput>> {
    let name = query.match_name();
    if name.is_empty() {
        return Ok(None);
    }
    let quals = query.qualifier_tokens();
    let hits = graph.find_by_name(&name, &quals)?;
    if hits.len() == 1 {
        let mut out = load_entity(graph, &hits[0])?;
        out.confidence_reason = ConfidenceReason::LocalNameMatch;
        out.confidence = score(&out.confidence_reason);
        return Ok(Some(out));
    }
    if hits.len() > 1 {
        let mut candidates: Vec<Candidate> = Vec::new();
        for cid in &hits {
            if let Some(e) = graph.get_entity(cid)? {
                candidates.push(Candidate {
                    canonical_id: e.canonical_id,
                    anchor: e.anchor,
                    name: e.name,
                });
            }
        }
        let reason = ConfidenceReason::AmbiguousAmongN(candidates.len());
        return Ok(Some(ResolveOutput {
            canonical_id: None,
            anchor: String::new(),
            entity_type: query.entity_type.clone(),
            name: query.name.clone(),
            same_as: Vec::new(),
            matched_via: Vec::new(),
            status: Status::Unresolved,
            harvested: 0,
            new_edges: 0,
            confidence: score(&reason),
            confidence_reason: reason,
            candidates,
            provenance: Vec::new(),
        }));
    }
    Ok(None)
}

/// A structured "not found in the local graph" result — used when a name query
/// is run local-only (no `--complete`) and misses. Signals the caller to reach
/// out (or supply a stronger identifier).
pub fn name_not_found(query: &NameQuery) -> ResolveOutput {
    let reason = ConfidenceReason::NeedsStrongerIdentifier;
    ResolveOutput {
        canonical_id: None,
        anchor: String::new(),
        entity_type: query.entity_type.clone(),
        name: query.name.clone(),
        same_as: Vec::new(),
        matched_via: vec![
            "not in local graph — re-run with --complete to reach external hubs".into(),
        ],
        status: Status::Unresolved,
        harvested: 0,
        new_edges: 0,
        confidence: score(&reason),
        confidence_reason: reason,
        candidates: Vec::new(),
        provenance: Vec::new(),
    }
}

/// Resolve a name/address query: **graph-first** (see [`resolve_name_local`]),
/// then, on a miss, reverse-resolve via the hubs (Placekey when a street is
/// present + Google Text Search → place_id → website/phone) and **write the
/// name+qualifiers into the local index** so the next identical query is local.
pub fn resolve_name(
    graph: &Graph,
    query: &NameQuery,
    ctx: &CompletionCtx,
) -> Result<ResolveOutput> {
    // 0. Graph-first: zero external calls when we've seen this name before.
    if let Some(out) = resolve_name_local(graph, query)? {
        return Ok(out);
    }
    let name = query.match_name();
    let quals = query.qualifier_tokens();

    // Google place_id candidates via text search (best-effort). We look at
    // *all* candidates: more than one means the query is ambiguous.
    let ts = PlaceTextSearchResolver::new(
        TextSearchInput::Text(query.text_query()),
        ctx.google_key.clone(),
        ctx.transport.clone(),
    );
    let place_ids: Vec<String> = ts.candidates().unwrap_or_default();

    // Ambiguous on the place itself: refuse to commit a place_id (and keep the
    // Placekey out, to avoid a half-built entity). Ask for a stronger query.
    //
    // DEFERRED (candidate enrichment): for candidates not already in the graph we
    // emit only the bare place_id key with `name: None` — enough for a machine to
    // detect ambiguity, but NOT enough for a caller (e.g. an AI agent) to turn the
    // list into a human-answerable "which one?" question with meaningful options.
    // Making candidates choosable means fetching Place Details (name + formatted
    // address) per un-graphed candidate here. That costs one external call per
    // candidate on an ambiguous query — a deliberate exception to "reach out as
    // little as possible" — so it's deferred until a consumer actually needs it.
    if place_ids.len() > 1 {
        let mut candidates: Vec<Candidate> = Vec::new();
        for pid in &place_ids {
            let key = ExternalId::google_place_id(pid)?.key();
            match graph.find(&key)? {
                Some(cid) => {
                    if let Some(e) = graph.get_entity(&cid)? {
                        candidates.push(Candidate {
                            canonical_id: e.canonical_id,
                            anchor: e.anchor,
                            name: e.name,
                        });
                        continue;
                    }
                    candidates.push(Candidate {
                        canonical_id: String::new(),
                        anchor: key,
                        name: None,
                    });
                }
                None => candidates.push(Candidate {
                    canonical_id: String::new(),
                    anchor: key,
                    name: None,
                }),
            }
        }
        let reason = ConfidenceReason::AmbiguousAmongN(candidates.len());
        return Ok(ResolveOutput {
            canonical_id: None,
            anchor: String::new(),
            entity_type: query.entity_type.clone(),
            name: query.name.clone(),
            same_as: Vec::new(),
            matched_via: Vec::new(),
            status: Status::Unresolved,
            harvested: 0,
            new_edges: 0,
            confidence: score(&reason),
            confidence_reason: reason,
            candidates,
            provenance: Vec::new(),
        });
    }

    // Unambiguous (exactly one place_id, or none + a Placekey). Build one record
    // and commit + forward-complete via the place_id.
    let mut record = EntityRecord {
        entity_type: query.entity_type.clone(),
        name: query.name.clone(),
        same_as: Vec::new(),
    };

    // Placekey anchor (best-effort) — only when we have a street address, since
    // Placekey's minimum inputs require a street (or lat/long): a name+city query
    // can't produce a Placekey, so skip the guaranteed-failing round-trip and let
    // the Google place_id carry the identity.
    if query.has_street() {
        if let Some(pk) = crate::hubs::placekey::PlacekeyResolver::new(
            query.clone(),
            ctx.placekey_key.clone(),
            ctx.transport.clone(),
        )
        .harvest()
        .ok()
        .and_then(|r| r.same_as.into_iter().next())
        {
            record.same_as.push(pk);
        }
    }
    // At this point the text search returned 0 or 1 candidate (>1 already refused
    // above). Exactly one is a confident, unique hub match.
    let unique_place = place_ids.len() == 1;
    if let Some(pid) = place_ids.into_iter().next() {
        let id = ExternalId::google_place_id(&pid)?;
        if !record.same_as.iter().any(|e| e == &id) {
            record.same_as.push(id);
        }
    }

    // If neither a Placekey nor a place_id was found, the record has no strong
    // key: the commit refuses (Unresolved / NeedsStrongerIdentifier). Return it.
    let mut out = resolve_and_complete(graph, &record, ctx)?;
    if out.canonical_id.is_none() {
        return Ok(out);
    }

    // Confidence by evidence: a full street address yields a precise Placekey;
    // otherwise a UNIQUE text-search match is a confident (delegated) match —
    // the candidate count is the signal (several would have refused above).
    let reason = if query.has_street() {
        ConfidenceReason::PlacekeyAddress
    } else if unique_place {
        ConfidenceReason::PlaceUniqueMatch
    } else {
        ConfidenceReason::PlacekeyCityOnly
    };
    out.confidence = score(&reason);
    out.confidence_reason = reason;

    // Write-through the local name index so the next identical name+qualifier
    // query is served locally (zero external calls). Index both the query name
    // and the resolved display name (alias) under the same qualifiers.
    if let Some(cid) = out.canonical_id.clone() {
        if !name.is_empty() {
            graph.index_name(&name, &quals, &cid, Some("name_query"))?;
        }
        if let Some(display) = out.name.clone() {
            let alias = crate::normalize::name_key(&display);
            if !alias.is_empty() && alias != name {
                graph.index_name(&alias, &quals, &cid, Some("name_query"))?;
            }
        }
    }
    Ok(out)
}

/// A transient, **type-agnostic** query: a name plus free-form qualifier tokens
/// (city / state / borough / year / …). Never persisted as content — only the
/// resulting IDs (place_id, Placekey, …) and a minimal name/qualifier match key
/// are stored. The `street`/`city`/`region`/`country` fields are place-hub
/// specifics used only by the Placekey adapter; a movie/park/etc. uses `name` +
/// `qualifiers` and leaves them `None`.
#[derive(Clone, Debug, Default)]
pub struct NameQuery {
    pub name: Option<String>,
    /// Free-form disambiguating facets — the engine treats these as opaque
    /// tokens, never as typed city/state/year.
    pub qualifiers: Vec<String>,
    pub entity_type: Option<String>,
    pub street: Option<String>,
    pub city: Option<String>,
    pub region: Option<String>,
    pub country: Option<String>,
}

impl NameQuery {
    /// A free-text query for the hub Text Search: name, address parts, and any
    /// extra qualifier tokens joined with commas.
    pub fn text_query(&self) -> String {
        let mut parts: Vec<&str> = [
            self.name.as_deref(),
            self.street.as_deref(),
            self.city.as_deref(),
            self.region.as_deref(),
            self.country.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect();
        parts.extend(self.qualifiers.iter().map(|q| q.as_str()));
        parts.join(", ")
    }

    /// Normalized name for the local name index (empty if no usable name).
    pub fn match_name(&self) -> String {
        self.name
            .as_deref()
            .map(crate::normalize::name_key)
            .unwrap_or_default()
    }

    /// Normalized qualifier tokens for the local name index: the free-form
    /// `qualifiers` plus `city`/`region`/`country` when present. `street` is
    /// excluded (too specific; it only feeds Placekey). Deduped, non-empty.
    pub fn qualifier_tokens(&self) -> Vec<String> {
        let mut toks: Vec<String> = self
            .qualifiers
            .iter()
            .map(|q| q.as_str())
            .chain([self.city.as_deref(), self.region.as_deref(), self.country.as_deref()]
                .into_iter()
                .flatten())
            .map(crate::normalize::name_key)
            .filter(|t| !t.is_empty())
            .collect();
        toks.sort();
        toks.dedup();
        toks
    }

    /// A non-empty street address is present — the minimum Placekey needs.
    pub fn has_street(&self) -> bool {
        self.street.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::FixtureTransport;
    use serde_json::json;

    // Build the URL the Wikidata adapter would call so the fixture matches.
    fn wikidata_url(imdb: &str) -> String {
        WikidataResolver::new(
            ExternalId::imdb(imdb).unwrap(),
            Arc::new(FixtureTransport::from_pairs(vec![])),
        )
        .url()
        .unwrap()
    }

    #[test]
    fn imdb_completes_from_empty_graph() {
        // Exit criterion 1: an IMDb id resolves to a QID and completes to
        // website + TMDb with no prior graph state.
        let g = Graph::open_in_memory().unwrap();

        let wd = wikidata_url("tt0133093");
        let find = "https://api.themoviedb.org/3/find/tt0133093?external_source=imdb_id&api_key=";
        let ext = "https://api.themoviedb.org/3/movie/603/external_ids?api_key=";
        let transport = FixtureTransport::from_pairs(vec![
            (
                "GET",
                &wd,
                json!({"results": {"bindings": [{
                    "item": {"value": "http://www.wikidata.org/entity/Q83495"},
                    "website": {"value": "https://www.warnerbros.com/movies/matrix"},
                    "tmdb": {"value": "603"}
                }]}}),
            ),
            ("GET", find, json!({"movie_results": [{"id": 603}]})),
            ("GET", ext, json!({"imdb_id": "tt0133093", "wikidata_id": "Q83495"})),
        ]);
        let ctx = CompletionCtx::new(Arc::new(transport));

        let input = EntityRecord {
            same_as: vec![ExternalId::imdb("tt0133093").unwrap()],
            ..Default::default()
        };
        let out = resolve_and_complete(&g, &input, &ctx).unwrap();
        let keys: Vec<String> = out.same_as.iter().map(|i| i.key()).collect();
        assert!(keys.contains(&"wikidata:Q83495".to_string()), "keys={keys:?}");
        assert!(keys.contains(&"tmdb:603".to_string()), "keys={keys:?}");
        assert!(keys.contains(&"domain:warnerbros.com".to_string()), "keys={keys:?}");
        assert_eq!(out.anchor, "wikidata:Q83495");
        assert!(out.confidence >= 0.9, "confidence={}", out.confidence);
    }

    #[test]
    fn place_id_completes_to_website_and_phone() {
        // Exit criterion 2: a place_id completes to website + phone.
        let g = Graph::open_in_memory().unwrap();
        let details = "https://places.googleapis.com/v1/places/ChIJN1";
        let transport = FixtureTransport::from_pairs(vec![(
            "GET",
            details,
            json!({
                "displayName": { "text": "Blue Bottle Coffee" },
                "websiteUri": "https://bluebottlecoffee.com/",
                "internationalPhoneNumber": "+1 510-653-3394"
            }),
        )]);
        let ctx = CompletionCtx::new(Arc::new(transport));

        let input = EntityRecord {
            same_as: vec![ExternalId::google_place_id("ChIJN1").unwrap()],
            ..Default::default()
        };
        let out = resolve_and_complete(&g, &input, &ctx).unwrap();
        let keys: Vec<String> = out.same_as.iter().map(|i| i.key()).collect();
        assert!(keys.contains(&"domain:bluebottlecoffee.com".to_string()), "keys={keys:?}");
        assert!(keys.contains(&"phone:+15106533394".to_string()), "keys={keys:?}");
    }

    #[test]
    fn completion_is_idempotent() {
        // Re-running completion on an already-complete cluster adds no edges.
        let g = Graph::open_in_memory().unwrap();
        let details = "https://places.googleapis.com/v1/places/ChIJN1";
        let transport = FixtureTransport::from_pairs(vec![(
            "GET",
            details,
            json!({
                "websiteUri": "https://bluebottlecoffee.com/",
                "internationalPhoneNumber": "+1 510-653-3394"
            }),
        )]);
        let ctx = CompletionCtx::new(Arc::new(transport));
        let input = EntityRecord {
            same_as: vec![ExternalId::google_place_id("ChIJN1").unwrap()],
            ..Default::default()
        };
        resolve_and_complete(&g, &input, &ctx).unwrap();
        let second = resolve_and_complete(&g, &input, &ctx).unwrap();
        assert_eq!(second.new_edges, 0, "second run should add no edges");
    }

    #[test]
    fn full_address_resolves_via_placekey_and_completes_via_place_id() {
        use crate::hubs::{PlaceDetailsResolver, PlaceTextSearchResolver, TextSearchInput};

        let g = Graph::open_in_memory().unwrap();
        // A full street address → Placekey runs (its minimum inputs are met).
        let query = NameQuery {
            name: Some("Blue Bottle Coffee".into()),
            street: Some("300 Webster St".into()),
            city: Some("Oakland".into()),
            region: Some("CA".into()),
            country: Some("US".into()),
            ..Default::default()
        };

        // Build the exact URLs the reverse + forward adapters will call.
        let text_url = PlaceTextSearchResolver::new(
            TextSearchInput::Text(query.text_query()),
            String::new(),
            Arc::new(FixtureTransport::from_pairs(vec![])),
        )
        .url();
        let details_url = PlaceDetailsResolver::new(
            ExternalId::google_place_id("EXAMPLE_blue_bottle_oakland").unwrap(),
            String::new(),
            Arc::new(FixtureTransport::from_pairs(vec![])),
        )
        .url();

        let transport = FixtureTransport::from_pairs(vec![
            ("POST", "https://api.placekey.io/v1/placekey", json!({"placekey": "227-223@5vg-7gq-tvz"})),
            ("POST", &text_url, json!({"places": [{"id": "EXAMPLE_blue_bottle_oakland"}]})),
            (
                "GET",
                &details_url,
                json!({
                    "websiteUri": "https://bluebottlecoffee.com/",
                    "internationalPhoneNumber": "+1 510-653-3394"
                }),
            ),
        ]);
        let ctx = CompletionCtx::new(Arc::new(transport));

        let out = resolve_name(&g, &query, &ctx).unwrap();
        let keys: Vec<String> = out.same_as.iter().map(|i| i.key()).collect();
        assert!(keys.contains(&"placekey:227-223@5vg-7gq-tvz".to_string()), "keys={keys:?}");
        assert!(keys.contains(&"google_place_id:EXAMPLE_blue_bottle_oakland".to_string()), "keys={keys:?}");
        assert!(keys.contains(&"domain:bluebottlecoffee.com".to_string()), "keys={keys:?}");
        assert!(keys.contains(&"phone:+15106533394".to_string()), "keys={keys:?}");
        // Placekey (rank 1) is the anchor; a full-address match is higher confidence.
        assert_eq!(out.anchor, "placekey:227-223@5vg-7gq-tvz");
        assert!((out.confidence - crate::confidence::PLACEKEY_ADDRESS).abs() < 1e-6);
    }

    #[test]
    fn reverse_place_id_does_not_merge_into_phone_only_entity() {
        // Two place-bearing entities sharing the SAME phone but distinct
        // place_ids stay distinct (phone never merges), and a phone-only commit
        // refuses to mint anything at all (no strong key).
        let g = Graph::open_in_memory().unwrap();

        let place_a = commit_record_with_source(
            &g,
            &EntityRecord {
                same_as: vec![
                    ExternalId::google_place_id("PLACE_A").unwrap(),
                    ExternalId::phone("+1-510-653-3394").unwrap(),
                ],
                ..Default::default()
            },
            "reverse_search",
        )
        .unwrap();

        let place_b = commit_record_with_source(
            &g,
            &EntityRecord {
                same_as: vec![
                    ExternalId::google_place_id("PLACE_B").unwrap(),
                    ExternalId::phone("+1-510-653-3394").unwrap(),
                ],
                ..Default::default()
            },
            "reverse_search",
        )
        .unwrap();

        assert_ne!(place_a.canonical_id, place_b.canonical_id);
        // The shared phone corroborates both, without merging them.
        assert_eq!(g.find_phone("phone:+15106533394").unwrap().len(), 2);

        // A phone-only commit has no strong key → refuse (no entity minted).
        let phone_only = commit_record_with_source(
            &g,
            &EntityRecord {
                same_as: vec![ExternalId::phone("+1-510-653-3394").unwrap()],
                ..Default::default()
            },
            "input",
        )
        .unwrap();
        assert_eq!(phone_only.status, Status::Unresolved);
        assert!(phone_only.canonical_id.is_none());
    }

    #[test]
    fn name_query_caches_second_lookup_is_local() {
        use crate::hubs::{PlaceDetailsResolver, PlaceTextSearchResolver, TextSearchInput};

        let g = Graph::open_in_memory().unwrap();
        let query = NameQuery {
            name: Some("Blue Bottle Coffee".into()),
            city: Some("Oakland".into()),
            region: Some("CA".into()),
            country: Some("US".into()),
            ..Default::default()
        };
        let text_url = PlaceTextSearchResolver::new(
            TextSearchInput::Text(query.text_query()),
            String::new(),
            Arc::new(FixtureTransport::from_pairs(vec![])),
        )
        .url();
        let details_url = PlaceDetailsResolver::new(
            ExternalId::google_place_id("PID1").unwrap(),
            String::new(),
            Arc::new(FixtureTransport::from_pairs(vec![])),
        )
        .url();
        let transport = FixtureTransport::from_pairs(vec![
            ("POST", &text_url, json!({"places": [{"id": "PID1"}]})),
            ("GET", &details_url, json!({"websiteUri": "https://bluebottlecoffee.com/"})),
        ]);
        let ctx = CompletionCtx::new(Arc::new(transport));

        // First resolution reaches the (fixture) hub and records the name index.
        let first = resolve_name(&g, &query, &ctx).unwrap();
        let cid = first.canonical_id.clone().expect("first resolve should succeed");

        // Local-only lookup takes NO transport at all → proves zero external calls.
        let second = resolve_name_local(&g, &query).unwrap().expect("cached local hit");
        assert_eq!(second.canonical_id.as_deref(), Some(cid.as_str()));
        assert_eq!(second.confidence_reason, ConfidenceReason::LocalNameMatch);
    }

    #[test]
    fn name_index_ambiguous_returns_candidates() {
        let g = Graph::open_in_memory().unwrap();
        g.create_entity("cx_sf", "google_place_id:SF", None, Some("Basecamp")).unwrap();
        g.create_entity("cx_ny", "google_place_id:NY", None, Some("Basecamp")).unwrap();
        g.index_name("basecamp", &["san francisco".into()], "cx_sf", Some("t")).unwrap();
        g.index_name("basecamp", &["new york".into()], "cx_ny", Some("t")).unwrap();

        // Bare name matches both → ambiguous (definitive; no external call).
        let bare = NameQuery { name: Some("Basecamp".into()), ..Default::default() };
        let out = resolve_name_local(&g, &bare).unwrap().expect("ambiguous is definitive");
        assert_eq!(out.status, Status::Unresolved);
        assert_eq!(out.candidates.len(), 2);

        // A qualifier narrows to the one entity.
        let sf = NameQuery {
            name: Some("Basecamp".into()),
            qualifiers: vec!["San Francisco".into()],
            ..Default::default()
        };
        let hit = resolve_name_local(&g, &sf).unwrap().expect("unique");
        assert_eq!(hit.canonical_id.as_deref(), Some("cx_sf"));
    }

    #[test]
    fn name_index_is_type_agnostic_about_the_facet() {
        // The qualifier is a state here (a national park), not a city — same
        // machinery, no place-specific assumptions.
        let g = Graph::open_in_memory().unwrap();
        g.create_entity("cx_park", "wikidata:Q180402", Some("Park"), Some("Yosemite"))
            .unwrap();
        g.index_name("yosemite", &["california".into()], "cx_park", Some("t")).unwrap();

        let q = NameQuery {
            name: Some("Yosemite".into()),
            qualifiers: vec!["California".into()],
            ..Default::default()
        };
        let hit = resolve_name_local(&g, &q).unwrap().expect("hit");
        assert_eq!(hit.canonical_id.as_deref(), Some("cx_park"));
    }
}
