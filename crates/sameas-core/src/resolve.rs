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

use anyhow::{anyhow, Result};

use crate::anchor;
use crate::graph::Graph;
use crate::model::{EntityRecord, ExternalId};

/// Whether a resolution created a new entity or hit an existing one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    New,
    Hit,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::New => "new",
            Status::Hit => "hit",
        }
    }
}

/// The result of resolving an input: a canonical id plus the completed
/// identifier set from the local graph.
#[derive(Clone, Debug)]
pub struct ResolveOutput {
    pub canonical_id: String,
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
/// returned `same_as` is the whole cluster from the local graph.
pub fn commit_record(graph: &Graph, record: &EntityRecord) -> Result<ResolveOutput> {
    let strong_ids: Vec<&ExternalId> = record.strong_ids().collect();
    let phone_ids: Vec<&ExternalId> = record.phone_ids().collect();
    let harvested = record.same_as.len();

    let mut matched_via: Vec<String> = Vec::new();
    let mut new_edges = 0usize;

    // 1. Which existing canonicals do the strong keys already point to?
    let mut strong_hits: Vec<String> = Vec::new();
    for id in &strong_ids {
        if let Some(canon) = graph.find(&id.key())? {
            strong_hits.push(canon);
        }
    }
    dedup(&mut strong_hits);

    // 2. Which canonicals does the phone corroborate?
    let mut phone_hits: Vec<String> = Vec::new();
    for id in &phone_ids {
        phone_hits.extend(graph.find_phone(&id.key())?);
    }
    dedup(&mut phone_hits);

    // 3. Choose the target canonical.
    let (canonical_id, status) = if !strong_hits.is_empty() {
        // Strong keys matched. If they span several entities, union them
        // (strong-key-driven — legitimate). Winner = strongest anchor.
        let winner = pick_winner(graph, &strong_hits)?;
        for canon in &strong_hits {
            if canon != &winner {
                graph.merge_into(&winner, canon)?;
            }
        }
        (winner, Status::Hit)
    } else if strong_ids.is_empty() {
        // No strong keys at all: a phone-only (or empty) query.
        match phone_hits.len() {
            1 => (phone_hits[0].clone(), Status::Hit),
            0 => {
                // Lonely identifier: mint a synthetic entity so it still
                // resolves to something stable.
                let anchor = anchor::choose_anchor(&record.same_as);
                let cid = anchor::canonical_id_for(&anchor);
                graph.create_entity(
                    &cid,
                    &anchor,
                    record.entity_type.as_deref(),
                    record.name.as_deref(),
                )?;
                (cid, Status::New)
            }
            _ => {
                // Phone corroborates several distinct entities — ambiguous.
                // Never merge on phone alone; resolve to the lowest id and flag.
                matched_via.push("phone (ambiguous: corroborates multiple entities)".into());
                (phone_hits[0].clone(), Status::Hit)
            }
        }
    } else {
        // Has strong keys, but none matched an existing entity → a genuinely
        // new entity. Even if its phone matches something, we do NOT adopt that
        // entity: phone never merges distinct entities.
        let anchor = anchor::choose_anchor(&record.same_as);
        let cid = anchor::canonical_id_for(&anchor);
        graph.create_entity(
            &cid,
            &anchor,
            record.entity_type.as_deref(),
            record.name.as_deref(),
        )?;
        (cid, Status::New)
    };

    // 4. Attach every strong key to the target.
    for id in &strong_ids {
        match graph.find(&id.key())? {
            Some(c) if c == canonical_id => {
                matched_via.push(id.kind_tag().to_string());
            }
            _ => {
                graph.attach(&id.key(), &canonical_id)?;
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
            graph.add_phone_edge(&id.key(), &canonical_id)?;
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

    Ok(ResolveOutput {
        canonical_id,
        anchor: entity.anchor,
        entity_type: entity.entity_type,
        name: entity.name,
        same_as: members,
        matched_via,
        status,
        harvested,
        new_edges,
    })
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
    Ok(ResolveOutput {
        canonical_id: entity.canonical_id,
        anchor: entity.anchor,
        entity_type: entity.entity_type,
        name: entity.name,
        same_as: members,
        matched_via: Vec::new(),
        status: Status::Hit,
        harvested: 0,
        new_edges: 0,
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
        assert_eq!(
            g.find("domain:a-cafe.com").unwrap(),
            Some(out1.canonical_id.clone())
        );
        assert_eq!(
            g.find("domain:b-cafe.com").unwrap(),
            Some(out2.canonical_id.clone())
        );
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
        assert_eq!(g.find("domain:x.com").unwrap(), Some(out.canonical_id.clone()));
        assert_eq!(
            g.find("google_place_id:ChIJxyz").unwrap(),
            Some(out.canonical_id.clone())
        );
        assert_eq!(out.same_as.len(), 3);
        assert_eq!(out.anchor, "wikidata:Q1");
    }
}
