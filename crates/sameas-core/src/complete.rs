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
//! Placekey) are **entry-point only** — see [`complete_place_query`] — because
//! auto-running them on every domain/phone in a cluster would risk false place
//! edges (a movie's website is not a place).
//!
//! Hub calls are **best-effort**: a hub error (unavailable, no fixture, bad
//! response) yields no completion rather than failing the whole resolution.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Result;

use crate::graph::Graph;
use crate::hubs::{
    PlaceDetailsResolver, PlaceTextSearchResolver, TextSearchInput, TmdbResolver, WikidataResolver,
};
use crate::model::{EntityRecord, ExternalId};
use crate::resolve::{commit_record_with_source, load_entity, Resolver, ResolveOutput};
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
    let canonical_id = seed.canonical_id.clone();
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
    out.confidence = seed.confidence;
    Ok(out)
}

/// Resolve a name/address query: reverse-resolve to a Placekey anchor **and** a
/// Google place_id (via text search), merge both into one record so they land in
/// a single cluster, commit, then forward-complete (place_id → website + phone).
///
/// Confidence is bounded by the coarseness of the query (`city_only`), even
/// though completion may be rich — the identity match is only as good as the
/// text/address lookup.
pub fn complete_place_query(
    graph: &Graph,
    query: &PlaceQuery,
    ctx: &CompletionCtx,
) -> Result<ResolveOutput> {
    let mut record = EntityRecord {
        entity_type: query.entity_type.clone(),
        name: query.name.clone(),
        same_as: Vec::new(),
    };

    // Placekey anchor (best-effort).
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

    // Google place_id via text search (best-effort).
    let text = query.text_query();
    let ts = PlaceTextSearchResolver::new(
        TextSearchInput::Text(text),
        ctx.google_key.clone(),
        ctx.transport.clone(),
    );
    if let Ok(r) = ts.harvest() {
        for id in r.same_as {
            if !record.same_as.iter().any(|e| e == &id) {
                record.same_as.push(id);
            }
        }
    }

    // Commit whatever we found (never fork; if both failed, this mints a
    // name-only synthetic entity), then forward-complete via the place_id.
    let mut out = resolve_and_complete(graph, &record, ctx)?;
    out.confidence = if query.is_city_only() {
        crate::confidence::PLACEKEY_CITY
    } else {
        crate::confidence::PLACEKEY_ADDRESS
    };
    Ok(out)
}

/// A transient place query for the reverse-resolvers. Never persisted — only the
/// resulting IDs (Placekey, place_id, …) are stored.
#[derive(Clone, Debug, Default)]
pub struct PlaceQuery {
    pub name: Option<String>,
    pub street: Option<String>,
    pub city: Option<String>,
    pub region: Option<String>,
    pub country: Option<String>,
    pub entity_type: Option<String>,
}

impl PlaceQuery {
    /// A free-text query for Find-Place: `"name, street, city region country"`.
    pub fn text_query(&self) -> String {
        [
            self.name.as_deref(),
            self.street.as_deref(),
            self.city.as_deref(),
            self.region.as_deref(),
            self.country.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ")
    }

    /// Coarse query: a name and a city (or less) but no street address.
    pub fn is_city_only(&self) -> bool {
        self.street.as_deref().map(|s| s.trim().is_empty()).unwrap_or(true)
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
        let details = "https://maps.googleapis.com/maps/api/place/details/json?place_id=ChIJN1&fields=website,international_phone_number,name&key=";
        let transport = FixtureTransport::from_pairs(vec![(
            "GET",
            details,
            json!({"status": "OK", "result": {
                "name": "Blue Bottle Coffee",
                "website": "https://bluebottlecoffee.com/",
                "international_phone_number": "+1 510-653-3394"
            }}),
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
        let details = "https://maps.googleapis.com/maps/api/place/details/json?place_id=ChIJN1&fields=website,international_phone_number,name&key=";
        let transport = FixtureTransport::from_pairs(vec![(
            "GET",
            details,
            json!({"status": "OK", "result": {
                "website": "https://bluebottlecoffee.com/",
                "international_phone_number": "+1 510-653-3394"
            }}),
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
    fn name_city_resolves_via_placekey_and_completes_via_place_id() {
        use crate::hubs::{PlaceDetailsResolver, PlaceTextSearchResolver, TextSearchInput};

        let g = Graph::open_in_memory().unwrap();
        let query = PlaceQuery {
            name: Some("Blue Bottle Coffee".into()),
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
            ("GET", &text_url, json!({"status": "OK", "candidates": [{"place_id": "EXAMPLE_blue_bottle_oakland"}]})),
            (
                "GET",
                &details_url,
                json!({"status": "OK", "result": {
                    "website": "https://bluebottlecoffee.com/",
                    "international_phone_number": "+1 510-653-3394"
                }}),
            ),
        ]);
        let ctx = CompletionCtx::new(Arc::new(transport));

        let out = complete_place_query(&g, &query, &ctx).unwrap();
        let keys: Vec<String> = out.same_as.iter().map(|i| i.key()).collect();
        assert!(keys.contains(&"placekey:227-223@5vg-7gq-tvz".to_string()), "keys={keys:?}");
        assert!(keys.contains(&"google_place_id:EXAMPLE_blue_bottle_oakland".to_string()), "keys={keys:?}");
        assert!(keys.contains(&"domain:bluebottlecoffee.com".to_string()), "keys={keys:?}");
        assert!(keys.contains(&"phone:+15106533394".to_string()), "keys={keys:?}");
        // Placekey (rank 1) is the anchor; the coarse city-only query is low-confidence.
        assert_eq!(out.anchor, "placekey:227-223@5vg-7gq-tvz");
        assert!((out.confidence - crate::confidence::PLACEKEY_CITY).abs() < 1e-6);
    }

    #[test]
    fn reverse_place_id_does_not_merge_into_phone_only_entity() {
        // A phone corroborating both a phone-only entity and a place entity must
        // not merge them (union-find is strong-keys-only; phone never merges).
        let g = Graph::open_in_memory().unwrap();

        // A phone-only entity (no strong key).
        let phone_only = commit_record_with_source(
            &g,
            &EntityRecord {
                same_as: vec![ExternalId::phone("+1-510-653-3394").unwrap()],
                ..Default::default()
            },
            "input",
        )
        .unwrap();

        // A place entity that shares the phone (as a reverse-resolved place_id would).
        let place = commit_record_with_source(
            &g,
            &EntityRecord {
                same_as: vec![
                    ExternalId::google_place_id("EXAMPLE_blue_bottle_oakland").unwrap(),
                    ExternalId::phone("+1-510-653-3394").unwrap(),
                ],
                ..Default::default()
            },
            "reverse_search",
        )
        .unwrap();

        assert_ne!(phone_only.canonical_id, place.canonical_id);
        // The phone corroborates both, without merging them.
        assert_eq!(g.find_phone("phone:+15106533394").unwrap().len(), 2);
    }
}
