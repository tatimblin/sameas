//! Resolution orchestration + resolver adapters.
//!
//! A [`Resolver`] harvests a partial [`EntityRecord`] from some input. The
//! orchestration ([`commit_record`]) then normalizes, looks up each key in the
//! crosswalk graph, attaches / unions / adopts an anchor, and returns the
//! completed entity.
//!
//! Union safety (M1 baseline): only **strong** keys drive merges. Phone is
//! recorded as a corroborating edge but never single-handedly merges two
//! otherwise-distinct entities.

use std::collections::HashSet;

use anyhow::{anyhow, Result};

use crate::anchor;
use crate::confidence::{score, ConfidenceReason};
use crate::graph::Graph;
use crate::kind::Grain;
use crate::model::{EntityRecord, ExternalId};

/// Whether a resolution created a new entity, hit an existing one, or refused
/// to resolve (no strong identifier / ambiguous).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    New,
    Hit,
    Unresolved,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::New => "new",
            Status::Hit => "hit",
            Status::Unresolved => "unresolved",
        }
    }
}

/// A distinct entity an ambiguous / refused resolution could have meant. Surfaced
/// so the caller can ask for a stronger identifier instead of guessing.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub canonical_id: String,
    pub anchor: String,
    pub name: Option<String>,
}

/// The result of resolving an input: a canonical id plus the completed
/// identifier set from the local graph. `canonical_id` is `None` when the input
/// could not be resolved (no strong key, or ambiguous among several entities).
#[derive(Clone, Debug)]
pub struct ResolveOutput {
    pub canonical_id: Option<String>,
    pub anchor: String,
    pub entity_type: Option<String>,
    pub name: Option<String>,
    pub same_as: Vec<ExternalId>,
    /// Human-readable descriptions of which key(s) matched an existing entity.
    pub matched_via: Vec<String>,
    pub status: Status,
    /// Identifiers harvested from the input record.
    pub harvested: usize,
    /// New edges written to the graph by this resolution.
    pub new_edges: usize,
    /// How well the input attaches to this entity, `0.0`–`1.0`. Reflects the
    /// weakest link (the input→entity match), not cluster richness. Always
    /// equals `score(&confidence_reason)`.
    pub confidence: f32,
    /// Why the confidence came out the way it did (what to fix if it is low).
    pub confidence_reason: ConfidenceReason,
    /// When unresolved/ambiguous, the distinct entities the input could match.
    pub candidates: Vec<Candidate>,
    /// Per-member edge provenance: `(key, source)`, e.g.
    /// `("wikidata:Q83495", Some("wikidata"))`.
    pub provenance: Vec<(String, Option<String>)>,
}

/// Anything that can produce a record to resolve.
pub trait Resolver {
    fn harvest(&self) -> Result<EntityRecord>;
}

/// Harvests identifiers directly from an already-typed record (seed ingest,
/// or a single `--flag` input built into a one-id record).
pub struct DirectRecordResolver {
    record: EntityRecord,
}

impl DirectRecordResolver {
    pub fn new(record: EntityRecord) -> Self {
        DirectRecordResolver { record }
    }
}

impl Resolver for DirectRecordResolver {
    fn harvest(&self) -> Result<EntityRecord> {
        Ok(self.record.clone())
    }
}

/// Commit a harvested record into the graph and return the completed entity.
///
/// This is the heart of "resolve = complete": whatever identifier came in, the
/// returned `same_as` is the whole cluster from the local graph. Edges written
/// here carry the provenance `"input"`; use [`commit_record_with_source`] to
/// attribute edges to a specific hub.
pub fn commit_record(graph: &Graph, record: &EntityRecord) -> Result<ResolveOutput> {
    commit_record_with_source(graph, record, "input")
}

/// Like [`commit_record`], but tags every edge it writes with `source`
/// (e.g. `"wikidata"`, `"google_places"`) for edge provenance.
pub fn commit_record_with_source(
    graph: &Graph,
    record: &EntityRecord,
    source: &str,
) -> Result<ResolveOutput> {
    let strong_ids: Vec<&ExternalId> = record.strong_ids().collect();
    let phone_ids: Vec<&ExternalId> = record.phone_ids().collect();
    let harvested = record.same_as.len();

    let mut matched_via: Vec<String> = Vec::new();
    let mut new_edges = 0usize;

    // Partition strong keys by grain: identity keys name one thing (drive
    // identity); affiliation keys (a shared domain) may span many things.
    let identity_ids: Vec<&ExternalId> = strong_ids
        .iter()
        .copied()
        .filter(|id| id.spec().grain == Grain::Identity)
        .collect();
    let affiliation_ids: Vec<&ExternalId> = strong_ids
        .iter()
        .copied()
        .filter(|id| id.spec().grain == Grain::Affiliation)
        .collect();

    // 1. Which existing canonicals do the identity / affiliation keys hit?
    let mut identity_hits: Vec<String> = Vec::new();
    for id in &identity_ids {
        if let Some(canon) = graph.find(&id.key())? {
            identity_hits.push(canon);
        }
    }
    dedup(&mut identity_hits);
    let mut affil_hits: Vec<String> = Vec::new();
    for id in &affiliation_ids {
        if let Some(canon) = graph.find(&id.key())? {
            affil_hits.push(canon);
        }
    }
    dedup(&mut affil_hits);

    // 2. Refuse if the record has NO strong key at all (only phone / name /
    //    empty). We never mint or attach on the strength of a phone alone.
    if strong_ids.is_empty() {
        let mut phone_canons: Vec<String> = Vec::new();
        for id in &phone_ids {
            phone_canons.extend(graph.find_phone(&id.key())?);
        }
        dedup(&mut phone_canons);
        let mut candidates: Vec<Candidate> = Vec::new();
        for c in &phone_canons {
            if let Some(e) = graph.get_entity(c)? {
                candidates.push(Candidate {
                    canonical_id: e.canonical_id,
                    anchor: e.anchor,
                    name: e.name,
                });
            }
        }
        let reason = match candidates.len() {
            0 => ConfidenceReason::NeedsStrongerIdentifier,
            1 => ConfidenceReason::PhoneOnly,
            n => ConfidenceReason::AmbiguousAmongN(n),
        };
        return Ok(ResolveOutput {
            canonical_id: None,
            anchor: String::new(),
            entity_type: record.entity_type.clone(),
            name: record.name.clone(),
            same_as: Vec::new(),
            matched_via: Vec::new(),
            status: Status::Unresolved,
            harvested,
            new_edges: 0,
            confidence: score(&reason),
            confidence_reason: reason,
            candidates,
            provenance: Vec::new(),
        });
    }

    // Incoming identity keys, for the affiliation-hit intersection test.
    let incoming_identity_keys: HashSet<String> =
        identity_ids.iter().map(|id| id.key()).collect();

    // 3. Choose the target canonical (≥1 strong key present).
    let (canonical_id, status) = if !identity_hits.is_empty() {
        // Identity keys matched: legitimately the same thing. Union the identity
        // hits, then absorb affiliation hits unless they carry a conflicting
        // identity (a distinct thing sharing only the domain).
        let winner = pick_winner(graph, &identity_hits)?;
        for canon in &identity_hits {
            if canon != &winner {
                graph.merge_into(&winner, canon)?;
            }
        }
        for canon in &affil_hits {
            if canon != &winner
                && !identity_conflict(&graph.members(&winner)?, &graph.members(canon)?)
            {
                graph.merge_into(&winner, canon)?;
            }
        }
        (winner, Status::Hit)
    } else if !affil_hits.is_empty() {
        if identity_ids.is_empty() {
            // Domain-only record. Adopt the affiliation cluster, but NEVER union
            // two owners that carry disjoint identity keys: a record listing two
            // studios' domains (or a page whose <link rel=canonical> points to a
            // second registrable domain) must not merge two distinct things.
            // Only fold in affiliation hits that are compatible with the winner
            // (same entity, or one side identity-less) — mirroring the identity-
            // hits branch. Conflicting owners stay separate and keep their own
            // domain (step 4 refuses to steal it).
            let winner = pick_winner(graph, &affil_hits)?;
            for canon in &affil_hits {
                if canon != &winner
                    && !identity_conflict(&graph.members(&winner)?, &graph.members(canon)?)
                {
                    graph.merge_into(&winner, canon)?;
                }
            }
            (winner, Status::Hit)
        } else {
            // We carry identity keys; only adopt an affiliation cluster that
            // already shares one of them, else this is a distinct thing.
            let mut target: Option<String> = None;
            for canon in &affil_hits {
                let c_identity = identity_keys(&graph.members(canon)?);
                if c_identity
                    .iter()
                    .any(|k| incoming_identity_keys.contains(k))
                {
                    target = Some(canon.clone());
                    break;
                }
            }
            match target {
                Some(c) => (c, Status::Hit),
                None => (mint_entity(graph, record)?, Status::New),
            }
        }
    } else {
        // Strong keys, but none matched an existing entity → a new entity.
        (mint_entity(graph, record)?, Status::New)
    };

    // 4. Attach every strong key to the target, except an incoming affiliation
    //    key currently owned by an entity that is a *distinct* thing (identity
    //    conflict) — don't steal a shared domain.
    for id in &strong_ids {
        let key = id.key();
        match graph.find(&key)? {
            Some(c) if c == canonical_id => {
                matched_via.push(id.kind_tag().to_string());
            }
            Some(_) => {
                // A different owner (the equal case is handled above). Don't
                // steal a shared Affiliation key (a chain/brand domain) from it.
                // If that owner were truly the same entity it would already have
                // been unioned above — merge_into re-points its keys, so `find`
                // would have returned `canonical_id` here. A *different* owner at
                // this point means we could not prove the two are the same thing
                // (their identity keys conflict, or the owner is an identity-less
                // brand org). Re-pointing the domain would be a silent merge-
                // without-cleanup that orphans that entity and leaves its anchor
                // naming a key it no longer owns. Leave the key with its owner.
                // This holds even when the incoming record carries no identity
                // key of its own (a pure domain-only record).
                if id.spec().grain == Grain::Affiliation {
                    matched_via.push("domain (shared affiliation; distinct entity)".into());
                    continue;
                }
                graph.attach_with_source(&key, &canonical_id, Some(source))?;
                new_edges += 1;
            }
            None => {
                graph.attach_with_source(&key, &canonical_id, Some(source))?;
                new_edges += 1;
            }
        }
    }

    // 5. Record phone edges (corroborators). Attaching to the target never
    //    merges anything — a phone may edge to multiple entities.
    for id in &phone_ids {
        let already = graph.find_phone(&id.key())?;
        if already.iter().any(|c| c == &canonical_id) {
            matched_via.push("phone (corroborating)".into());
        } else {
            graph.add_phone_edge_with_source(&id.key(), &canonical_id, Some(source))?;
            new_edges += 1;
            if !already.is_empty() {
                matched_via.push("phone (corroborating)".into());
            }
        }
    }

    // 6. Sharpen the anchor from the full membership (canonical id is fixed).
    let members = graph.members(&canonical_id)?;
    let current = graph
        .get_entity(&canonical_id)?
        .ok_or_else(|| anyhow!("entity {canonical_id} vanished mid-resolve"))?
        .anchor;
    let anchor = anchor::recompute_anchor(&members, &current);
    if anchor != current {
        graph.set_anchor(&canonical_id, &anchor)?;
    }
    graph.enrich_entity(
        &canonical_id,
        record.entity_type.as_deref(),
        record.name.as_deref(),
    )?;

    let entity = graph
        .get_entity(&canonical_id)?
        .ok_or_else(|| anyhow!("entity {canonical_id} not found"))?;

    dedup(&mut matched_via);

    // 7. Confidence reason for the resolved path.
    let reason = match status {
        Status::Hit => ConfidenceReason::ExactStrongKey,
        Status::New if anchor::public_anchor(&members).is_some() => {
            ConfidenceReason::NewPublicAnchor
        }
        Status::New => ConfidenceReason::SyntheticStrongKey,
        Status::Unresolved => unreachable!("unresolved handled above"),
    };
    let provenance = graph.member_sources(&canonical_id)?;

    Ok(ResolveOutput {
        canonical_id: Some(canonical_id),
        anchor: entity.anchor,
        entity_type: entity.entity_type,
        name: entity.name,
        same_as: members,
        matched_via,
        status,
        harvested,
        new_edges,
        confidence: score(&reason),
        confidence_reason: reason,
        candidates: Vec::new(),
        provenance,
    })
}

/// Mint a fresh entity for `record`: public anchor if any, else a deterministic
/// synthetic anchor from the strongest strong key, else a local synthetic id.
fn mint_entity(graph: &Graph, record: &EntityRecord) -> Result<String> {
    let anchor = anchor::choose_anchor(&record.same_as);
    let cid = anchor::canonical_id_for(&anchor);
    graph.create_entity(
        &cid,
        &anchor,
        record.entity_type.as_deref(),
        record.name.as_deref(),
    )?;
    Ok(cid)
}

/// The identity-grain keys among `members`.
fn identity_keys(members: &[ExternalId]) -> HashSet<String> {
    members
        .iter()
        .filter(|id| id.spec().grain == Grain::Identity)
        .map(|id| id.key())
        .collect()
}

/// True iff both member sets carry identity keys and those sets are disjoint —
/// i.e. they name *distinct* things and must not be merged.
fn identity_conflict(a: &[ExternalId], b: &[ExternalId]) -> bool {
    let ka = identity_keys(a);
    let kb = identity_keys(b);
    !ka.is_empty() && !kb.is_empty() && ka.is_disjoint(&kb)
}

/// Resolve a single typed identifier (the CLI `--flag` path).
pub fn resolve_id(graph: &Graph, id: ExternalId) -> Result<ResolveOutput> {
    let record = EntityRecord {
        same_as: vec![id],
        ..Default::default()
    };
    commit_record(graph, &record)
}

/// Load an existing entity by canonical id (the `entity <id>` path).
pub fn load_entity(graph: &Graph, canonical_id: &str) -> Result<ResolveOutput> {
    let entity = graph
        .get_entity(canonical_id)?
        .ok_or_else(|| anyhow!("no entity with canonical_id {canonical_id}"))?;
    let members = graph.members(canonical_id)?;
    let provenance = graph.member_sources(canonical_id)?;
    Ok(ResolveOutput {
        canonical_id: Some(entity.canonical_id),
        anchor: entity.anchor,
        entity_type: entity.entity_type,
        name: entity.name,
        same_as: members,
        matched_via: Vec::new(),
        status: Status::Hit,
        harvested: 0,
        new_edges: 0,
        // A direct canonical-id lookup: we were handed the identity.
        confidence: crate::confidence::DIRECT_LOOKUP,
        confidence_reason: ConfidenceReason::DirectLookup,
        candidates: Vec::new(),
        provenance,
    })
}

/// Pick the union winner among candidate canonicals: strongest anchor, ties
/// broken by canonical id for determinism.
fn pick_winner(graph: &Graph, canonicals: &[String]) -> Result<String> {
    let mut best: Option<(u8, String)> = None;
    for cid in canonicals {
        let anchor = graph
            .get_entity(cid)?
            .ok_or_else(|| anyhow!("entity {cid} not found"))?
            .anchor;
        let rank = anchor::anchor_key_rank(&anchor);
        let candidate = (rank, cid.clone());
        best = match best {
            None => Some(candidate),
            Some(ref cur) if candidate.0 < cur.0 => Some(candidate),
            Some(ref cur) if candidate.0 == cur.0 && candidate.1 < cur.1 => Some(candidate),
            other => other,
        };
    }
    best.map(|(_, cid)| cid)
        .ok_or_else(|| anyhow!("no winner among empty candidate set"))
}

fn dedup(v: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    v.retain(|x| seen.insert(x.clone()));
}

// -------------------------------------------------------------------------
// DomainResolver
// -------------------------------------------------------------------------

/// Harvests identifiers from a domain's home page: schema.org JSON-LD, then
/// OpenGraph / `<link rel=canonical>` fallbacks. Reads page HTML from a local
/// fixture (offline/deterministic) or, behind the `live-fetch` feature, over
/// HTTP.
pub struct DomainResolver {
    domain: String,
    html: String,
}

impl DomainResolver {
    /// Build a resolver from a domain and an HTML fixture file.
    pub fn from_fixture(domain: &str, fixture_path: &std::path::Path) -> Result<Self> {
        let html = std::fs::read_to_string(fixture_path)
            .map_err(|e| anyhow!("reading fixture {}: {e}", fixture_path.display()))?;
        Ok(DomainResolver {
            domain: crate::normalize::registrable_domain(domain)?,
            html,
        })
    }

    /// Build a resolver from a domain and pre-fetched HTML.
    pub fn from_html(domain: &str, html: String) -> Result<Self> {
        Ok(DomainResolver {
            domain: crate::normalize::registrable_domain(domain)?,
            html,
        })
    }

    /// Fetch the page over HTTP (requires the `live-fetch` feature).
    #[cfg(feature = "live-fetch")]
    pub fn from_live(domain: &str) -> Result<Self> {
        let reg = crate::normalize::registrable_domain(domain)?;
        let url = format!("https://{reg}/");
        let body = reqwest::blocking::get(&url)
            .and_then(|r| r.error_for_status())
            .and_then(|r| r.text())
            .map_err(|e| anyhow!("fetching {url}: {e}"))?;
        Ok(DomainResolver { domain: reg, html: body })
    }
}

impl Resolver for DomainResolver {
    fn harvest(&self) -> Result<EntityRecord> {
        use scraper::{Html, Selector};

        let doc = Html::parse_document(&self.html);
        let mut record = EntityRecord::default();
        // The domain itself is always an identifier (already normalized).
        record.same_as.push(ExternalId::domain(&self.domain)?);

        // 1. schema.org JSON-LD blocks.
        let script_sel = Selector::parse(r#"script[type="application/ld+json"]"#).unwrap();
        for el in doc.select(&script_sel) {
            let text: String = el.text().collect();
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
                harvest_jsonld(&value, &mut record);
            }
        }

        // 2. Fallbacks when JSON-LD yielded nothing beyond the domain.
        if record.entity_type.is_none() {
            let og_type = meta_content(&doc, r#"meta[property="og:type"]"#);
            if let Some(t) = og_type {
                record.entity_type = Some(t);
            }
        }
        if record.name.is_none() {
            if let Some(t) = meta_content(&doc, r#"meta[property="og:title"]"#) {
                record.name = Some(t);
            }
        }
        // <link rel="canonical"> → confirm/augment the domain.
        if let Some(href) = link_href(&doc, r#"link[rel="canonical"]"#) {
            if let Ok(id) = ExternalId::domain(&href) {
                push_unique(&mut record.same_as, id);
            }
        }

        Ok(record)
    }
}

/// Extract identifiers from a JSON-LD value (object, array, or @graph).
fn harvest_jsonld(value: &serde_json::Value, record: &mut EntityRecord) {
    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                harvest_jsonld(item, record);
            }
        }
        serde_json::Value::Object(map) => {
            if let Some(graph) = map.get("@graph") {
                harvest_jsonld(graph, record);
            }
            if record.entity_type.is_none() {
                if let Some(t) = map.get("@type").and_then(|v| v.as_str()) {
                    record.entity_type = Some(t.to_string());
                }
            }
            if record.name.is_none() {
                if let Some(n) = map.get("name").and_then(|v| v.as_str()) {
                    record.name = Some(n.to_string());
                }
            }
            if let Some(tel) = map.get("telephone").and_then(|v| v.as_str()) {
                if let Ok(id) = ExternalId::phone(tel) {
                    push_unique(&mut record.same_as, id);
                }
            }
            if let Some(url) = map.get("url").and_then(|v| v.as_str()) {
                if let Ok(id) = ExternalId::domain(url) {
                    push_unique(&mut record.same_as, id);
                }
            }
            if let Some(same_as) = map.get("sameAs") {
                harvest_same_as(same_as, record);
            }
        }
        _ => {}
    }
}

fn harvest_same_as(value: &serde_json::Value, record: &mut EntityRecord) {
    match value {
        serde_json::Value::String(s) => {
            if let Some(id) = guess_id_from_url(s) {
                push_unique(&mut record.same_as, id);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                harvest_same_as(item, record);
            }
        }
        _ => {}
    }
}

/// Best-effort mapping of a `sameAs` URL to a typed identifier. Untyped social
/// links (facebook, twitter, …) are intentionally skipped.
///
/// Registry-driven: every kind with a `url_match` participates automatically
/// (first match wins), so a new kind gains page-harvesting support just by
/// setting `url_match` in its [`crate::kind::KindSpec`]. Anything else that is
/// URL-shaped falls back to a `domain` edge.
fn guess_id_from_url(raw: &str) -> Option<ExternalId> {
    let lower = raw.to_ascii_lowercase();

    const SOCIAL: &[&str] = &[
        "facebook.com",
        "twitter.com",
        "x.com",
        "instagram.com",
        "youtube.com",
        "linkedin.com",
        "tiktok.com",
        "pinterest.com",
    ];
    if SOCIAL.iter().any(|s| lower.contains(s)) {
        return None;
    }

    // Try each registered kind's URL matcher; first match wins.
    for spec in crate::kind::KINDS {
        if let Some(url_match) = spec.url_match {
            if let Some(raw_value) = url_match(raw) {
                if let Ok(id) = ExternalId::new(spec.tag, &raw_value) {
                    return Some(id);
                }
            }
        }
    }

    // Anything else URL-shaped is treated as another domain edge.
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return ExternalId::domain(raw).ok();
    }
    None
}

fn push_unique(ids: &mut Vec<ExternalId>, id: ExternalId) {
    if !ids.iter().any(|existing| existing == &id) {
        ids.push(id);
    }
}

fn meta_content(doc: &scraper::Html, selector: &str) -> Option<String> {
    let sel = scraper::Selector::parse(selector).ok()?;
    doc.select(&sel)
        .next()
        .and_then(|el| el.value().attr("content"))
        .map(|s| s.to_string())
}

fn link_href(doc: &scraper::Html, selector: &str) -> Option<String> {
    let sel = scraper::Selector::parse(selector).ok()?;
    doc.select(&sel)
        .next()
        .and_then(|el| el.value().attr("href"))
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"
    <html><head>
    <script type="application/ld+json">
    {
      "@context": "https://schema.org",
      "@type": "LocalBusiness",
      "name": "Blue Bottle Coffee",
      "url": "https://bluebottlecoffee.com",
      "telephone": "+1-510-653-3394",
      "sameAs": [
        "https://www.wikidata.org/wiki/Q4926426",
        "https://www.facebook.com/bluebottlecoffee"
      ]
    }
    </script>
    </head><body>hi</body></html>
    "#;

    #[test]
    fn jsonld_sameas_extraction() {
        let r = DomainResolver::from_html("bluebottlecoffee.com", FIXTURE.to_string()).unwrap();
        let rec = r.harvest().unwrap();
        assert_eq!(rec.entity_type.as_deref(), Some("LocalBusiness"));
        assert_eq!(rec.name.as_deref(), Some("Blue Bottle Coffee"));
        let keys: Vec<String> = rec.same_as.iter().map(|i| i.key()).collect();
        assert!(keys.contains(&"domain:bluebottlecoffee.com".to_string()));
        assert!(keys.contains(&"wikidata:Q4926426".to_string()));
        assert!(keys.contains(&"phone:+15106533394".to_string()));
        // facebook is a social link and is skipped.
        assert!(!keys.iter().any(|k| k.contains("facebook")));
    }

    #[test]
    fn guess_id_from_url_recognizes_yelp() {
        let id =
            guess_id_from_url("https://www.yelp.com/biz/blue-bottle-coffee-san-francisco").unwrap();
        assert_eq!(id.key(), "yelp:blue-bottle-coffee-san-francisco");
        // Non-biz yelp URLs and social links do not produce a yelp id.
        assert!(guess_id_from_url("https://www.facebook.com/x").is_none());
    }

    #[test]
    fn yelp_harvested_from_jsonld_sameas() {
        let html = r#"
        <html><head>
        <script type="application/ld+json">
        {
          "@type": "LocalBusiness",
          "name": "Blue Bottle Coffee",
          "url": "https://bluebottlecoffee.com",
          "sameAs": ["https://www.yelp.com/biz/blue-bottle-coffee-san-francisco"]
        }
        </script>
        </head><body></body></html>
        "#;
        let r = DomainResolver::from_html("bluebottlecoffee.com", html.to_string()).unwrap();
        let rec = r.harvest().unwrap();
        let keys: Vec<String> = rec.same_as.iter().map(|i| i.key()).collect();
        assert!(keys.contains(&"yelp:blue-bottle-coffee-san-francisco".to_string()));
    }

    #[test]
    fn resolve_by_yelp_hits_same_entity() {
        let g = Graph::open_in_memory().unwrap();
        // Seed a cluster that includes a yelp id + a wikidata anchor.
        let out = commit_record(
            &g,
            &EntityRecord {
                same_as: vec![
                    ExternalId::domain("bluebottlecoffee.com").unwrap(),
                    ExternalId::wikidata("Q4926426").unwrap(),
                    ExternalId::yelp("https://www.yelp.com/biz/blue-bottle-coffee-san-francisco")
                        .unwrap(),
                ],
                ..Default::default()
            },
        )
        .unwrap();
        // Resolving by the generic yelp key lands on the same entity.
        let hit = resolve_id(
            &g,
            ExternalId::new("yelp", "blue-bottle-coffee-san-francisco").unwrap(),
        )
        .unwrap();
        assert_eq!(hit.canonical_id, out.canonical_id);
        assert_eq!(hit.status, Status::Hit);
        assert_eq!(hit.anchor, "wikidata:Q4926426");
    }

    #[test]
    fn phone_alone_does_not_merge_distinct_entities() {
        let g = Graph::open_in_memory().unwrap();

        // Entity 1: domain A + shared phone.
        let rec1 = EntityRecord {
            entity_type: Some("LocalBusiness".into()),
            name: Some("Cafe A".into()),
            same_as: vec![
                ExternalId::domain("a-cafe.com").unwrap(),
                ExternalId::phone("+1-510-653-3394").unwrap(),
            ],
        };
        let out1 = commit_record(&g, &rec1).unwrap();

        // Entity 2: DIFFERENT domain B + the SAME phone.
        let rec2 = EntityRecord {
            entity_type: Some("LocalBusiness".into()),
            name: Some("Cafe B".into()),
            same_as: vec![
                ExternalId::domain("b-cafe.com").unwrap(),
                ExternalId::phone("+1-510-653-3394").unwrap(),
            ],
        };
        let out2 = commit_record(&g, &rec2).unwrap();

        // They must remain DISTINCT despite sharing a phone.
        assert_ne!(out1.canonical_id, out2.canonical_id);
        assert_eq!(g.find("domain:a-cafe.com").unwrap(), out1.canonical_id.clone());
        assert_eq!(g.find("domain:b-cafe.com").unwrap(), out2.canonical_id.clone());
        // The phone corroborates both.
        let phone_canons = g.find_phone("phone:+15106533394").unwrap();
        assert_eq!(phone_canons.len(), 2);
    }

    #[test]
    fn strong_keys_union_transitively() {
        let g = Graph::open_in_memory().unwrap();
        // Record 1: domain + wikidata
        commit_record(
            &g,
            &EntityRecord {
                same_as: vec![
                    ExternalId::domain("x.com").unwrap(),
                    ExternalId::wikidata("Q1").unwrap(),
                ],
                ..Default::default()
            },
        )
        .unwrap();
        // Record 2: place_id + wikidata (same QID) → should union.
        let out = commit_record(
            &g,
            &EntityRecord {
                same_as: vec![
                    ExternalId::google_place_id("ChIJxyz").unwrap(),
                    ExternalId::wikidata("Q1").unwrap(),
                ],
                ..Default::default()
            },
        )
        .unwrap();
        // Now domain, place_id, wikidata all resolve to the same canonical.
        assert_eq!(g.find("domain:x.com").unwrap(), out.canonical_id.clone());
        assert_eq!(
            g.find("google_place_id:ChIJxyz").unwrap(),
            out.canonical_id.clone()
        );
        assert_eq!(out.same_as.len(), 3);
        assert_eq!(out.anchor, "wikidata:Q1");
    }

    // --- C1: an affiliation-only record must not merge distinct identities ---
    #[test]
    fn affiliation_only_record_does_not_merge_distinct_identities() {
        let g = Graph::open_in_memory().unwrap();

        // P and Q are distinct things (disjoint IMDb identity) that each carry
        // their own studio domain.
        let p = commit_record(
            &g,
            &EntityRecord {
                same_as: vec![
                    ExternalId::imdb("tt1111111").unwrap(),
                    ExternalId::domain("p-studio.com").unwrap(),
                ],
                ..Default::default()
            },
        )
        .unwrap();
        let q = commit_record(
            &g,
            &EntityRecord {
                same_as: vec![
                    ExternalId::imdb("tt2222222").unwrap(),
                    ExternalId::domain("q-studio.com").unwrap(),
                ],
                ..Default::default()
            },
        )
        .unwrap();
        assert_ne!(p.canonical_id, q.canonical_id);

        // A record listing BOTH studio domains and no identity key at all
        // (e.g. a page whose <link rel=canonical> points at a second domain).
        // It must NOT union the two distinct clusters.
        commit_record(
            &g,
            &EntityRecord {
                same_as: vec![
                    ExternalId::domain("p-studio.com").unwrap(),
                    ExternalId::domain("q-studio.com").unwrap(),
                ],
                ..Default::default()
            },
        )
        .unwrap();

        // The two IMDb identities must STILL resolve to different canonicals.
        let after_p = g.find("imdb:tt1111111").unwrap();
        let after_q = g.find("imdb:tt2222222").unwrap();
        assert!(after_p.is_some() && after_q.is_some());
        assert_ne!(
            after_p, after_q,
            "an affiliation-only record must not merge distinct-identity entities"
        );
        // Each domain stays with its own identity's owner (not stolen).
        assert_eq!(g.find("domain:p-studio.com").unwrap(), after_p);
        assert_eq!(g.find("domain:q-studio.com").unwrap(), after_q);
    }

    // --- H1: stealing a domain from an identity-less brand must not orphan it ---
    #[test]
    fn store_does_not_orphan_identity_less_brand_owning_the_domain() {
        let g = Graph::open_in_memory().unwrap();

        // A domain-only brand org: no identity key, anchored on its domain.
        let brand = commit_record(
            &g,
            &EntityRecord {
                name: Some("Acme".into()),
                same_as: vec![
                    ExternalId::domain("acme.com").unwrap(),
                    ExternalId::phone("+1-800-555-2000").unwrap(),
                ],
                ..Default::default()
            },
        )
        .unwrap();
        let brand_id = brand.canonical_id.clone().unwrap();
        assert_eq!(brand.anchor, "domain:acme.com");

        // A specific store: its own place_id identity + the same brand domain.
        let store = commit_record(
            &g,
            &EntityRecord {
                name: Some("Acme Store #1".into()),
                same_as: vec![
                    ExternalId::google_place_id("STORE1").unwrap(),
                    ExternalId::domain("acme.com").unwrap(),
                ],
                ..Default::default()
            },
        )
        .unwrap();
        let store_id = store.canonical_id.clone().unwrap();
        assert_ne!(brand_id, store_id);

        // The domain must NOT be stolen: it stays with the brand.
        assert_eq!(g.find("domain:acme.com").unwrap().as_deref(), Some(brand_id.as_str()));
        assert_eq!(
            g.find("google_place_id:STORE1").unwrap().as_deref(),
            Some(store_id.as_str())
        );

        // The brand entity survives AND its anchor still names a key it owns
        // (no stale-anchor orphan).
        let brand_row = g.get_entity(&brand_id).unwrap().expect("brand must survive");
        assert_eq!(brand_row.anchor, "domain:acme.com");
        assert_eq!(
            g.find(&brand_row.anchor).unwrap().as_deref(),
            Some(brand_id.as_str()),
            "an entity's anchor must always name a key it actually owns"
        );
    }

    // --- guard against over-correction: legitimate affiliation attach still works ---
    #[test]
    fn legitimate_domain_attaches_to_same_identity_entity() {
        let g = Graph::open_in_memory().unwrap();

        // Seed an entity by its identity key alone.
        let seed = commit_record(
            &g,
            &EntityRecord {
                same_as: vec![ExternalId::wikidata("Q1").unwrap()],
                ..Default::default()
            },
        )
        .unwrap();
        let seed_id = seed.canonical_id.clone().unwrap();

        // A page carrying that SAME identity plus a new domain: the domain must
        // attach to the existing entity (not be refused as a "shared" domain).
        let out = commit_record(
            &g,
            &EntityRecord {
                same_as: vec![
                    ExternalId::wikidata("Q1").unwrap(),
                    ExternalId::domain("e.com").unwrap(),
                ],
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(out.canonical_id.as_deref(), Some(seed_id.as_str()));
        assert_eq!(out.status, Status::Hit);
        assert_eq!(g.find("domain:e.com").unwrap().as_deref(), Some(seed_id.as_str()));
        let keys: Vec<String> = out.same_as.iter().map(|i| i.key()).collect();
        assert!(keys.contains(&"wikidata:Q1".to_string()));
        assert!(keys.contains(&"domain:e.com".to_string()));
    }
}
