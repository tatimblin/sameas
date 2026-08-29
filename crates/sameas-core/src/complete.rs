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
use crate::hubs::{
    HubCandidate, PlaceCandidate, PlaceDetailsResolver, PlaceTextSearchResolver, TextSearchInput,
    TmdbResolver, TmdbSearchResolver, WikidataResolver, WikidataSearchResolver,
};
use crate::model::{EntityRecord, ExternalId};
use crate::resolve::{
    commit_record_with_source, load_entity, Candidate, Resolver, ResolveOutput, Status,
};
use crate::store::{GraphStore, NameCardinality};
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

/// The "edge is missing" gate: skip a hub when the cluster already carries the
/// edge(s) that hub would ADD, keeping completion local-first and idempotent.
///
/// The gate keys on each hub's OUTPUT kinds, never the input tag. Keying on the
/// input tag self-skips a hub when the input *is* that kind (e.g. a `wikidata:`
/// input echoes a `wikidata` edge, which would make a `has("wikidata")` gate
/// true at hop 0), so the hub never runs and the crosswalk it exists to perform
/// never happens. Keying on the crosswalk target instead lets a `wikidata:` id
/// harvest website+tmdb+imdb and a `tmdb:` id crosswalk to imdb/wikidata.
fn skip_if_present(hub: &str, members: &[ExternalId]) -> bool {
    let has = |tag: &str| members.iter().any(|m| m.kind_tag() == tag);
    match hub {
        // Wikidata crosswalks a movie out to its TMDb id (+ website/imdb); once a
        // tmdb edge exists the crosswalk has been done.
        "wikidata" => has("tmdb"),
        // TMDb crosswalks out to the Wikidata QID (+ imdb).
        "tmdb" => has("wikidata"),
        // Place Details yields a website and/or phone. Gate on either: a
        // phone-less (website-only) place still counts as completed, so it is not
        // re-fetched every run.
        "place_details" => has("domain") || has("phone"),
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
async fn run_hub(hub: &str, id: &ExternalId, ctx: &CompletionCtx) -> Option<EntityRecord> {
    let harvested: Result<EntityRecord> = match hub {
        "wikidata" => {
            WikidataResolver::new(id.clone(), ctx.transport.clone())
                .harvest()
                .await
        }
        "tmdb" => {
            TmdbResolver::new(id.clone(), ctx.tmdb_key.clone(), ctx.transport.clone())
                .harvest()
                .await
        }
        "place_details" => {
            PlaceDetailsResolver::new(id.clone(), ctx.google_key.clone(), ctx.transport.clone())
                .harvest()
                .await
        }
        _ => return None,
    };
    match harvested {
        Ok(rec) if !rec.same_as.is_empty() => Some(rec),
        _ => None,
    }
}

/// Resolve an input record and bootstrap-complete it from external hubs.
pub async fn resolve_and_complete(
    graph: &dyn GraphStore,
    input: &EntityRecord,
    ctx: &CompletionCtx,
) -> Result<ResolveOutput> {
    // 1. Local-first commit — establishes the canonical id + input confidence.
    let seed = commit_record_with_source(graph, input, "input").await?;
    // Refused (no strong key) → nothing resolvable to complete.
    let canonical_id = match &seed.canonical_id {
        Some(c) => c.clone(),
        None => return Ok(seed),
    };
    let mut total_new_edges = seed.new_edges;

    // 2. Bounded BFS over the cluster.
    let mut visited: HashSet<(&'static str, String)> = HashSet::new();
    for _hop in 0..ctx.max_hops {
        let members = graph.members(&canonical_id).await?;
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
                if let Some(record) = run_hub(hub, id, ctx).await {
                    let out = commit_record_with_source(graph, &record, source_for(hub)).await?;
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
    let mut out = load_entity(graph, &canonical_id).await?;
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
///
/// The rule is purely (name, qualifier-token-set) based and **type-agnostic** —
/// street/city/region/country/`--qualifier`/year are all opaque normalized
/// tokens (see [`NameQuery::establishing_qualifiers`]). Specificity is monotonic:
/// a cached entity with establishing set S is a confident, unique hit ONLY for a
/// query whose token set Q ⊇ S. A query that under-specifies (S ⊄ Q) is never
/// answered by confidently returning that entity — conservative misses are
/// acceptable (worst case a hub re-call); a wrong confident hit is not.
pub async fn resolve_name_local(graph: &dyn GraphStore, query: &NameQuery) -> Result<Option<ResolveOutput>> {
    let name = query.match_name();
    if name.is_empty() {
        return Ok(None);
    }
    let quals = query.establishing_qualifiers();

    // 1. Cardinality memory: what a prior hub search proved about (name, Q).
    //    Exact qualifier-set match only, so a coarse fact never masks a finer,
    //    resolvable query.
    //      * Ambiguous is definitive — return it now (zero external) rather than
    //        re-calling the hub or wrong-binding to one candidate.
    //      * Unique is held as a fallback: it must win over a graph MISS (a coarse
    //        repeat of a hub-confirmed single-location entity), but must NOT
    //        override a genuine multi-entity ambiguity the graph itself reveals.
    //        So we consult the establishing-set scan first and only fall back to
    //        the unique fact when that scan misses.
    let mut unique_fallback: Option<String> = None;
    match graph.name_cardinality(&name, &quals).await? {
        Some(NameCardinality::Ambiguous(stored)) => {
            let candidates: Vec<Candidate> = stored
                .into_iter()
                .map(|(canonical_id, anchor, name)| Candidate {
                    canonical_id,
                    anchor,
                    name,
                })
                .collect();
            return Ok(Some(ambiguous_output(query, candidates)));
        }
        // The stored id was already validated live by `name_cardinality` (a
        // merged/deleted id reads back as `None`).
        Some(NameCardinality::Unique(cid)) => unique_fallback = Some(cid),
        None => {}
    }

    // 2. Gather same-name entities with their establishing sets S_i. (An empty
    //    set here is a graph miss — the unique fallback below still applies.)
    let entities = graph.name_entities(&name).await?;
    let qset: std::collections::HashSet<&str> = quals.iter().map(|s| s.as_str()).collect();

    // Bare query (no qualifiers): name-only semantics — every same-name entity is
    // a candidate (one → hit, several → ambiguous). This is what makes a bare
    // `--name Nova` over two qualified "Nova" entities ambiguous, not a wrong pick.
    let matched: Vec<String> = if quals.is_empty() {
        entities.into_iter().map(|(cid, _)| cid).collect()
    } else {
        // SupersetMatches = { E_i : S_i ⊆ Q }. A bare-established entity (S_i = {})
        // is a subset of any Q, so it still hits (empty ⊆ anything).
        let superset: Vec<String> = entities
            .iter()
            .filter(|(_, s)| s.iter().all(|t| qset.contains(t.as_str())))
            .map(|(cid, _)| cid.clone())
            .collect();
        if !superset.is_empty() {
            superset
        } else {
            // No entity is fully covered by Q, but if ≥2 same-name entities are
            // each under-specified by Q while sharing ≥1 token with it (i.e. Q is
            // a coarse query over several known specifics), surface them as
            // candidates rather than missing (contract point 4).
            let overlapping: Vec<String> = entities
                .iter()
                .filter(|(_, s)| s.iter().any(|t| qset.contains(t.as_str())))
                .map(|(cid, _)| cid.clone())
                .collect();
            if overlapping.len() >= 2 {
                overlapping
            } else {
                Vec::new()
            }
        }
    };

    if matched.len() == 1 {
        let mut out = load_entity(graph, &matched[0]).await?;
        out.confidence_reason = ConfidenceReason::LocalNameMatch;
        out.confidence = score(&out.confidence_reason);
        return Ok(Some(out));
    }
    if matched.len() > 1 {
        let mut candidates: Vec<Candidate> = Vec::new();
        for cid in &matched {
            if let Some(e) = graph.get_entity(cid).await? {
                candidates.push(Candidate {
                    canonical_id: e.canonical_id,
                    anchor: e.anchor,
                    name: e.name,
                });
            }
        }
        return Ok(Some(ambiguous_output(query, candidates)));
    }

    // 3. Graph miss. A hub-confirmed unique memory hit wins over a miss: a coarse
    //    repeat of a genuinely single-location entity resolves locally (zero
    //    external) instead of re-calling the hub every time.
    if let Some(cid) = unique_fallback {
        let mut out = load_entity(graph, &cid).await?;
        out.confidence_reason = ConfidenceReason::LocalNameMatch;
        out.confidence = score(&out.confidence_reason);
        return Ok(Some(out));
    }
    Ok(None)
}

/// Build an `Unresolved` + `AmbiguousAmongN` output carrying `candidates`.
///
/// The list is capped at [`CANDIDATE_CAP`] for the *caller*, but the count in
/// `AmbiguousAmongN(n)` is the TRUE n and the overflow is reported in `hint` — a
/// silently truncated list would read as "there are exactly 8 of these", which is
/// a different (and wrong) fact about the world. The full list is what gets
/// written to the cardinality memory (see [`remember_ambiguity`]), so the display
/// cap never hardens into permanent local memory.
fn ambiguous_output(query: &NameQuery, mut candidates: Vec<Candidate>) -> ResolveOutput {
    let total = candidates.len();
    let reason = ConfidenceReason::AmbiguousAmongN(total);
    let hint = if total > CANDIDATE_CAP {
        let dropped = total - CANDIDATE_CAP;
        candidates.truncate(CANDIDATE_CAP);
        Some(format!(
            "showing the {CANDIDATE_CAP} highest-ranked of {total} candidates ({dropped} not shown); narrow the query with a qualifier"
        ))
    } else {
        None
    };
    ResolveOutput {
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
        hint,
    }
}

/// Append a sentence to `out.hint`, keeping whatever is already there.
fn append_hint(out: &mut ResolveOutput, extra: String) {
    out.hint = Some(match out.hint.take() {
        Some(existing) => format!("{existing}; {extra}"),
        None => extra,
    });
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
        matched_via: Vec::new(),
        status: Status::Unresolved,
        harvested: 0,
        new_edges: 0,
        confidence: score(&reason),
        confidence_reason: reason,
        candidates: Vec::new(),
        provenance: Vec::new(),
        hint: Some(
            "not in local graph — re-run with --complete to reach external hubs".into(),
        ),
    }
}

/// How many candidates an ambiguous verdict hands back. The *stored* cardinality
/// keeps the full list — this is a display budget, not a fact about the world.
pub const CANDIDATE_CAP: usize = 8;

/// How many Place Details calls may be spent labelling un-graphed place
/// candidates on ONE ambiguous query. Google Places is the only billable hub
/// here, and this is the only per-candidate fan-out in the system, so it is
/// explicitly budgeted; candidates past the budget keep their bare key and the
/// shortfall is reported, never hidden.
pub const PLACE_DETAILS_FANOUT_CAP: usize = 5;

/// Which hub answers a name query, decided by the entity type.
///
/// Routing exists because the hubs are not interchangeable: asking *Google
/// Places* about a film is nonsense, and it is also the only hub that costs
/// money. The unknown/absent case therefore routes to the **free** hub — a
/// mis-typed query should waste nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameHub {
    /// Google Places Text Search. Billable; candidates need addresses attached.
    Places,
    /// TMDb `/search/multi`. Free; candidates are self-describing (title + year).
    Tmdb,
    /// Wikidata `wbsearchentities`. Free; self-describing (label + description).
    /// The type-agnostic fallback for everything else, including no type at all.
    Wikidata,
}

impl NameHub {
    /// A stable tag for hints/messages.
    pub fn tag(self) -> &'static str {
        match self {
            NameHub::Places => "google_places",
            NameHub::Tmdb => "tmdb",
            NameHub::Wikidata => "wikidata",
        }
    }
}

/// Route an entity type to its name-search hub.
///
/// The key is the **NSID leaf**, matched case-insensitively — agent-web's
/// collections are `info.cursive.creativeWork.movie`, `…organization.restaurant`
/// and friends, and the leaf is the only part that carries type meaning. A full
/// NSID is accepted (everything before the last `.` is dropped) so a caller can
/// pass either form.
pub fn name_hub_for(entity_type: Option<&str>) -> NameHub {
    let leaf = entity_type
        .unwrap_or_default()
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    match leaf.as_str() {
        "place" | "localbusiness" | "foodestablishment" | "restaurant" => NameHub::Places,
        "movie" | "tvseries" => NameHub::Tmdb,
        // Everything else — and an ABSENT type — falls back to Wikidata. It is
        // free and type-agnostic, so an unrecognized type degrades to a worse
        // answer, never to a surprise bill.
        _ => NameHub::Wikidata,
    }
}

/// Resolve a name query: **graph-first** (see [`resolve_name_local`]), then, on a
/// miss, reverse-resolve through the hub its `entity_type` routes to
/// ([`name_hub_for`]) and **write the name+qualifiers into the local index** so
/// the next identical query is local.
pub async fn resolve_name(
    graph: &dyn GraphStore,
    query: &NameQuery,
    ctx: &CompletionCtx,
) -> Result<ResolveOutput> {
    // 0. Graph-first: zero external calls when we've seen this name before.
    if let Some(out) = resolve_name_local(graph, query).await? {
        return Ok(out);
    }
    match name_hub_for(query.entity_type.as_deref()) {
        NameHub::Places => resolve_name_places(graph, query, ctx).await,
        hub => resolve_name_search(graph, query, ctx, hub).await,
    }
}

/// The places branch: Google Text Search (+ Placekey when a street is present).
///
/// Unlike the free hubs, a Text Search result is not necessarily choosable on its
/// own, so ambiguous candidates are labelled from the search response where
/// possible and from a *budgeted* Place Details fan-out otherwise.
async fn resolve_name_places(
    graph: &dyn GraphStore,
    query: &NameQuery,
    ctx: &CompletionCtx,
) -> Result<ResolveOutput> {
    let name = query.match_name();
    let quals = query.establishing_qualifiers();

    // Google place candidates via text search (best-effort). We look at *all*
    // candidates: more than one means the query is ambiguous. Qualifiers ride
    // inside the text query itself, so Google does the narrowing here.
    let ts = PlaceTextSearchResolver::new(
        TextSearchInput::Text(query.text_query()),
        ctx.google_key.clone(),
        ctx.transport.clone(),
    );
    let places: Vec<PlaceCandidate> = ts.search().await.unwrap_or_default();

    // Ambiguous on the place itself: refuse to commit a place_id (and keep the
    // Placekey out, to avoid a half-built entity). Ask for a stronger query.
    if places.len() > 1 {
        let (candidates, undescribed) = place_candidates(graph, &places, ctx).await?;
        // Cardinality memory: record that (name, Q) is ambiguous, so a later
        // identical coarse query is answered from local memory (zero external)
        // instead of re-calling the hub or wrong-binding to one candidate. The
        // FULL list is stored; only the returned view is capped.
        remember_ambiguity(graph, &name, &quals, &candidates).await?;
        let mut out = ambiguous_output(query, candidates);
        if undescribed > 0 {
            append_hint(
                &mut out,
                format!(
                    "{undescribed} candidate(s) left unlabelled by the Place Details budget (cap {PLACE_DETAILS_FANOUT_CAP})"
                ),
            );
        }
        return Ok(out);
    }

    // Unambiguous (exactly one place_id, or none + a Placekey). Build one record
    // and commit + forward-complete via the place_id.
    //
    // Leave `name` empty here: the entity is minted name-less so the resolved
    // place's displayName (harvested by Place Details during completion) fills
    // it via `enrich_entity`'s NULL→name path. Seeding the user's query string
    // as the name would block that (enrich never clobbers). If no hub name
    // arrives, we fall back to the query name below so the entity isn't nameless.
    let mut record = EntityRecord {
        entity_type: query.entity_type.clone(),
        name: None,
        same_as: Vec::new(),
    };

    // Placekey anchor (best-effort) — only when we have a street address, since
    // Placekey's minimum inputs require a street (or lat/long): a name+city query
    // can't produce a Placekey, so skip the guaranteed-failing round-trip and let
    // the Google place_id carry the identity.
    if query.has_street() {
        let harvested = crate::hubs::placekey::PlacekeyResolver::new(
            query.clone(),
            ctx.placekey_key.clone(),
            ctx.transport.clone(),
        )
        .harvest()
        .await;
        if let Some(pk) = harvested.ok().and_then(|r| r.same_as.into_iter().next()) {
            record.same_as.push(pk);
        }
    }
    // At this point the text search returned 0 or 1 candidate (>1 already refused
    // above). Exactly one is a confident, unique hub match.
    let unique_place = places.len() == 1;
    if let Some(place) = places.into_iter().next() {
        let id = ExternalId::google_place_id(&place.place_id)?;
        if !record.same_as.iter().any(|e| e == &id) {
            record.same_as.push(id);
        }
    }

    // If neither a Placekey nor a place_id was found, the record has no strong
    // key: the commit refuses (Unresolved / NeedsStrongerIdentifier). Return it.
    let mut out = resolve_and_complete(graph, &record, ctx).await?;
    if out.canonical_id.is_none() {
        return Ok(out);
    }

    // Confidence by EVIDENCE, not intent: report PlacekeyAddress only when a
    // Placekey edge actually landed in the cluster. A street was supplied but the
    // Placekey hub returned nothing → identity rests on the place_id, so a unique
    // text-search match is a PlaceUniqueMatch (the candidate count is the signal;
    // several would have refused above).
    let has_placekey = out.same_as.iter().any(|id| id.kind_tag() == "placekey");
    let reason = if has_placekey {
        ConfidenceReason::PlacekeyAddress
    } else if unique_place {
        ConfidenceReason::PlaceUniqueMatch
    } else {
        // Unreachable in practice: reaching here means a strong key was committed
        // (else the early `canonical_id.is_none()` return fired) that is neither a
        // Placekey nor a unique place_id — impossible on this path. Retained
        // defensively so the match stays exhaustive over the evidence tiers.
        ConfidenceReason::PlacekeyCityOnly
    };
    out.confidence = score(&reason);
    out.confidence_reason = reason;

    index_resolved_name(graph, query, &mut out, unique_place).await?;
    Ok(out)
}

/// The free-hub branch (TMDb / Wikidata), where every candidate describes itself.
///
/// Two things differ from places: the hub query is the **name alone** (neither
/// hub takes a year or a city usefully in its search string — TMDb wants a
/// `year` param, Wikidata wants nothing), and the qualifiers are therefore
/// applied *locally* against each candidate's own self-description. That is what
/// turns `Avatar --qualifier 2009` into a single answer without a second call.
async fn resolve_name_search(
    graph: &dyn GraphStore,
    query: &NameQuery,
    ctx: &CompletionCtx,
    hub: NameHub,
) -> Result<ResolveOutput> {
    let name = query.match_name();
    let quals = query.establishing_qualifiers();
    let text = query.name.clone().unwrap_or_default();

    // Best-effort, like every other hub call: a hub failure is a miss, not an
    // error that fails the whole resolution.
    let found: Vec<HubCandidate> = match hub {
        NameHub::Tmdb => {
            TmdbSearchResolver::new(text, ctx.tmdb_key.clone(), ctx.transport.clone())
                .candidates()
                .await
        }
        _ => {
            WikidataSearchResolver::new(text, ctx.transport.clone())
                .candidates()
                .await
        }
    }
    .unwrap_or_default();

    let found = narrow_by_qualifiers(found, &quals);

    if found.is_empty() {
        return Ok(hub_miss(query, hub));
    }

    if found.len() > 1 {
        let candidates = search_candidates(graph, &found).await?;
        remember_ambiguity(graph, &name, &quals, &candidates).await?;
        return Ok(ambiguous_output(query, candidates));
    }

    // Exactly one candidate — a confident (if delegated) hub match. Unlike the
    // places path we DO seed the name: the hub already told us the canonical
    // title, so there is no later enrichment call that would fill it in.
    let only = found.into_iter().next().expect("len == 1");
    let record = EntityRecord {
        entity_type: query.entity_type.clone(),
        name: only.name.clone(),
        same_as: vec![only.id],
    };
    let mut out = resolve_and_complete(graph, &record, ctx).await?;
    if out.canonical_id.is_none() {
        return Ok(out);
    }
    // `PlaceUniqueMatch` is the "a text query resolved to a SINGLE hub result"
    // tier; the name is a leftover from when places were the only name hub (the
    // gradient itself is type-agnostic — see `confidence.rs`).
    let reason = ConfidenceReason::PlaceUniqueMatch;
    out.confidence = score(&reason);
    out.confidence_reason = reason;

    index_resolved_name(graph, query, &mut out, true).await?;
    Ok(out)
}

/// Narrow a self-describing candidate list by the query's qualifier tokens.
///
/// Kept **fail-open**: a token that matches nothing (a city, on a film query)
/// would otherwise erase the whole list, and returning no candidates is strictly
/// worse for the caller than returning the hub's own ranking. Only a strictly
/// smaller, non-empty subset wins.
fn narrow_by_qualifiers(found: Vec<HubCandidate>, quals: &[String]) -> Vec<HubCandidate> {
    if quals.is_empty() || found.len() < 2 {
        return found;
    }
    let narrowed: Vec<usize> = found
        .iter()
        .enumerate()
        .filter(|(_, c)| c.matches_all(quals))
        .map(|(i, _)| i)
        .collect();
    if narrowed.is_empty() || narrowed.len() == found.len() {
        return found;
    }
    found
        .into_iter()
        .enumerate()
        .filter(|(i, _)| narrowed.contains(i))
        .map(|(_, c)| c)
        .collect()
}

/// Turn self-describing hub candidates into wire [`Candidate`]s, binding each to
/// the entity that already holds its key when there is one.
async fn search_candidates(
    graph: &dyn GraphStore,
    found: &[HubCandidate],
) -> Result<Vec<Candidate>> {
    let mut out = Vec::with_capacity(found.len());
    for c in found {
        let key = c.id.key();
        let mut candidate = Candidate {
            canonical_id: String::new(),
            anchor: key.clone(),
            name: c.label(),
        };
        if let Some(cid) = graph.find(&key).await? {
            if let Some(e) = graph.get_entity(&cid).await? {
                candidate.canonical_id = e.canonical_id;
                candidate.anchor = e.anchor;
                // The hub label carries the disambiguator (the year); the stored
                // name usually does not. Prefer the label, fall back to the row.
                candidate.name = candidate.name.or(e.name);
            }
        }
        out.push(candidate);
    }
    Ok(out)
}

/// Turn Text Search results into wire [`Candidate`]s, labelling each one.
///
/// Label sources, cheapest first: the entity we already have, then the search
/// response's own `displayName`/`formattedAddress`, then — only if both are
/// silent — a Place Details call, of which at most [`PLACE_DETAILS_FANOUT_CAP`]
/// are made per query. Returns the number left unlabelled by that budget, which
/// the caller reports rather than swallowing.
async fn place_candidates(
    graph: &dyn GraphStore,
    places: &[PlaceCandidate],
    ctx: &CompletionCtx,
) -> Result<(Vec<Candidate>, usize)> {
    let mut out = Vec::with_capacity(places.len());
    let mut spent = 0usize;
    let mut undescribed = 0usize;
    for place in places {
        // A place_id that will not normalize is not choosable — skip it rather
        // than failing the whole (already degraded) ambiguous answer.
        let id = match ExternalId::google_place_id(&place.place_id) {
            Ok(id) => id,
            Err(_) => continue,
        };
        let key = id.key();
        let mut candidate = Candidate {
            canonical_id: String::new(),
            anchor: key.clone(),
            name: place_label(place.name.as_deref(), place.address.as_deref()),
        };
        if let Some(cid) = graph.find(&key).await? {
            if let Some(e) = graph.get_entity(&cid).await? {
                candidate.canonical_id = e.canonical_id;
                candidate.anchor = e.anchor;
                candidate.name = candidate.name.or(e.name);
            }
        }
        if candidate.name.is_none() {
            if spent < PLACE_DETAILS_FANOUT_CAP {
                spent += 1;
                let details = PlaceDetailsResolver::new(
                    id,
                    ctx.google_key.clone(),
                    ctx.transport.clone(),
                );
                // Best-effort: a failed description leaves the bare key.
                if let Ok((name, address)) = details.describe().await {
                    candidate.name = place_label(name.as_deref(), address.as_deref());
                }
            }
            if candidate.name.is_none() {
                undescribed += 1;
            }
        }
        out.push(candidate);
    }
    Ok((out, undescribed))
}

/// `"Souvla (517 Hayes St, San Francisco)"` — a place's name plus the only thing
/// that tells two branches of one chain apart.
fn place_label(name: Option<&str>, address: Option<&str>) -> Option<String> {
    match (name, address) {
        (Some(n), Some(a)) => Some(format!("{n} ({a})")),
        (Some(n), None) => Some(n.to_string()),
        (None, Some(a)) => Some(a.to_string()),
        (None, None) => None,
    }
}

/// Write the FULL candidate list into the (name, Q) cardinality memory.
async fn remember_ambiguity(
    graph: &dyn GraphStore,
    name: &str,
    quals: &[String],
    candidates: &[Candidate],
) -> Result<()> {
    if name.is_empty() {
        return Ok(());
    }
    let triples: Vec<(String, String, Option<String>)> = candidates
        .iter()
        .map(|c| (c.canonical_id.clone(), c.anchor.clone(), c.name.clone()))
        .collect();
    graph.record_name_cardinality(name, quals, &triples).await
}

/// A hub search that returned nothing usable. Distinct from [`name_not_found`]:
/// there the caller had not reached out yet, here the hub has spoken.
fn hub_miss(query: &NameQuery, hub: NameHub) -> ResolveOutput {
    let reason = ConfidenceReason::NeedsStrongerIdentifier;
    ResolveOutput {
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
        candidates: Vec::new(),
        provenance: Vec::new(),
        hint: Some(format!(
            "{} returned no match for this name — supply an identifier (a URL or a {}: id)",
            hub.tag(),
            hub.tag()
        )),
    }
}

/// Persist the display name and write-through the local name index so the next
/// identical name+qualifier query is served locally (zero external calls).
/// Shared by both hub branches — the name index is type-agnostic.
async fn index_resolved_name(
    graph: &dyn GraphStore,
    query: &NameQuery,
    out: &mut ResolveOutput,
    unique: bool,
) -> Result<()> {
    let name = query.match_name();
    let quals = query.establishing_qualifiers();
    let cid = match out.canonical_id.clone() {
        Some(cid) => cid,
        None => return Ok(()),
    };
    // If completion produced no displayName, keep the entity from being nameless
    // by filling the query name (enrich only fills a NULL name).
    if out.name.is_none() {
        if let Some(qname) = query.name.clone() {
            graph.enrich_entity(&cid, None, Some(&qname)).await?;
            out.name = Some(qname);
        }
    }
    // Index the query name (what the user typed) and the resolved display name
    // (the hub's canonical name, an alias) under the same qualifiers, so a later
    // query by EITHER string resolves locally.
    if !name.is_empty() {
        graph.index_name(&name, &quals, &cid, Some("name_query")).await?;
    }
    if let Some(display) = out.name.clone() {
        let alias = crate::normalize::name_key(&display);
        if !alias.is_empty() && alias != name {
            graph.index_name(&alias, &quals, &cid, Some("name_query")).await?;
        }
    }
    // Record the UNIQUE side of the cardinality memory: the hub search returned
    // exactly one result AND it resolved. A later coarse repeat of this (name, Q)
    // — which under-specifies the entity's establishing set and so misses the
    // superset scan — then hits locally instead of re-calling the hub. INSERT OR
    // REPLACE on (name, Q): if a later hub call for the same (name, Q) returns
    // MULTIPLE, it overwrites this with an ambiguous row (and vice-versa). Keyed
    // identically to the ambiguous side (same `name`, same `quals`) so lookups
    // line up. Type-agnostic — Q is any qualifier set.
    if unique && !name.is_empty() {
        graph.record_name_unique(&name, &quals, &cid).await?;
    }
    Ok(())
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

    /// The FULL set of normalized qualifier tokens this query carries — the
    /// generic union of `qualifiers ∪ city ∪ region ∪ country ∪ street`, each run
    /// through `normalize::name_key`. Deduped, sorted, non-empty.
    ///
    /// This is the establishing set an entity is cached under, and the match key
    /// a later query is tested against. Every facet — including the street — is
    /// treated as an opaque token: the matcher is purely (name, token-set) based
    /// and type-agnostic. The street is NOT special-cased here (it only plays a
    /// distinct role in `has_street`, the Placekey gate). Folding it in is the fix
    /// for the coarse-cache mis-bind: a name+street entity is no longer served to
    /// a later name+city (no street) query, because its establishing set is not a
    /// subset of the coarser query's tokens.
    pub fn establishing_qualifiers(&self) -> Vec<String> {
        let mut toks: Vec<String> = self
            .qualifiers
            .iter()
            .map(|q| q.as_str())
            .chain(
                [
                    self.city.as_deref(),
                    self.region.as_deref(),
                    self.country.as_deref(),
                    self.street.as_deref(),
                ]
                .into_iter()
                .flatten(),
            )
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
    use crate::store::SqliteStore;
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

    #[tokio::test]
    async fn imdb_completes_from_empty_graph() {
        // Exit criterion 1: an IMDb id resolves to a QID and completes to
        // website + TMDb with no prior graph state.
        let g = SqliteStore::open_in_memory().unwrap();

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
        let out = resolve_and_complete(&g, &input, &ctx).await.unwrap();
        let keys: Vec<String> = out.same_as.iter().map(|i| i.key()).collect();
        assert!(keys.contains(&"wikidata:Q83495".to_string()), "keys={keys:?}");
        assert!(keys.contains(&"tmdb:603".to_string()), "keys={keys:?}");
        assert!(keys.contains(&"domain:warnerbros.com".to_string()), "keys={keys:?}");
        assert_eq!(out.anchor, "wikidata:Q83495");
        assert!(out.confidence >= 0.9, "confidence={}", out.confidence);
    }

    #[tokio::test]
    async fn place_id_completes_to_website_and_phone() {
        // Exit criterion 2: a place_id completes to website + phone.
        let g = SqliteStore::open_in_memory().unwrap();
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
        let out = resolve_and_complete(&g, &input, &ctx).await.unwrap();
        let keys: Vec<String> = out.same_as.iter().map(|i| i.key()).collect();
        assert!(keys.contains(&"domain:bluebottlecoffee.com".to_string()), "keys={keys:?}");
        assert!(keys.contains(&"phone:+15106533394".to_string()), "keys={keys:?}");
    }

    #[tokio::test]
    async fn completion_is_idempotent() {
        // Re-running completion on an already-complete cluster adds no edges.
        let g = SqliteStore::open_in_memory().unwrap();
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
        resolve_and_complete(&g, &input, &ctx).await.unwrap();
        let second = resolve_and_complete(&g, &input, &ctx).await.unwrap();
        assert_eq!(second.new_edges, 0, "second run should add no edges");
    }

    #[tokio::test]
    async fn full_address_resolves_via_placekey_and_completes_via_place_id() {
        use crate::hubs::{PlaceDetailsResolver, PlaceTextSearchResolver, TextSearchInput};

        let g = SqliteStore::open_in_memory().unwrap();
        // A full street address → Placekey runs (its minimum inputs are met).
        let query = NameQuery {
            entity_type: Some("restaurant".into()),
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

        let out = resolve_name(&g, &query, &ctx).await.unwrap();
        let keys: Vec<String> = out.same_as.iter().map(|i| i.key()).collect();
        assert!(keys.contains(&"placekey:227-223@5vg-7gq-tvz".to_string()), "keys={keys:?}");
        assert!(keys.contains(&"google_place_id:EXAMPLE_blue_bottle_oakland".to_string()), "keys={keys:?}");
        assert!(keys.contains(&"domain:bluebottlecoffee.com".to_string()), "keys={keys:?}");
        assert!(keys.contains(&"phone:+15106533394".to_string()), "keys={keys:?}");
        // Placekey (rank 1) is the anchor; a full-address match is higher confidence.
        assert_eq!(out.anchor, "placekey:227-223@5vg-7gq-tvz");
        assert!((out.confidence - crate::confidence::PLACEKEY_ADDRESS).abs() < 1e-6);
    }

    #[tokio::test]
    async fn reverse_place_id_does_not_merge_into_phone_only_entity() {
        // Two place-bearing entities sharing the SAME phone but distinct
        // place_ids stay distinct (phone never merges), and a phone-only commit
        // refuses to mint anything at all (no strong key).
        let g = SqliteStore::open_in_memory().unwrap();

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
        ).await
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
        ).await
        .unwrap();

        assert_ne!(place_a.canonical_id, place_b.canonical_id);
        // The shared phone corroborates both, without merging them.
        assert_eq!(g.find_phone("phone:+15106533394").await.unwrap().len(), 2);

        // A phone-only commit has no strong key → refuse (no entity minted).
        let phone_only = commit_record_with_source(
            &g,
            &EntityRecord {
                same_as: vec![ExternalId::phone("+1-510-653-3394").unwrap()],
                ..Default::default()
            },
            "input",
        ).await
        .unwrap();
        assert_eq!(phone_only.status, Status::Unresolved);
        assert!(phone_only.canonical_id.is_none());
    }

    #[tokio::test]
    async fn name_query_caches_second_lookup_is_local() {
        use crate::hubs::{PlaceDetailsResolver, PlaceTextSearchResolver, TextSearchInput};

        let g = SqliteStore::open_in_memory().unwrap();
        let query = NameQuery {
            entity_type: Some("restaurant".into()),
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
        let first = resolve_name(&g, &query, &ctx).await.unwrap();
        let cid = first.canonical_id.clone().expect("first resolve should succeed");

        // Local-only lookup takes NO transport at all → proves zero external calls.
        let second = resolve_name_local(&g, &query).await.unwrap().expect("cached local hit");
        assert_eq!(second.canonical_id.as_deref(), Some(cid.as_str()));
        assert_eq!(second.confidence_reason, ConfidenceReason::LocalNameMatch);
    }

    #[tokio::test]
    async fn name_index_ambiguous_returns_candidates() {
        let g = SqliteStore::open_in_memory().unwrap();
        g.create_entity("cx_sf", "google_place_id:SF", None, Some("Basecamp")).await.unwrap();
        g.create_entity("cx_ny", "google_place_id:NY", None, Some("Basecamp")).await.unwrap();
        g.index_name("basecamp", &["san francisco".into()], "cx_sf", Some("t")).await.unwrap();
        g.index_name("basecamp", &["new york".into()], "cx_ny", Some("t")).await.unwrap();

        // Bare name matches both → ambiguous (definitive; no external call).
        let bare = NameQuery { name: Some("Basecamp".into()), ..Default::default() };
        let out = resolve_name_local(&g, &bare).await.unwrap().expect("ambiguous is definitive");
        assert_eq!(out.status, Status::Unresolved);
        assert_eq!(out.candidates.len(), 2);

        // A qualifier narrows to the one entity.
        let sf = NameQuery {
            name: Some("Basecamp".into()),
            qualifiers: vec!["San Francisco".into()],
            ..Default::default()
        };
        let hit = resolve_name_local(&g, &sf).await.unwrap().expect("unique");
        assert_eq!(hit.canonical_id.as_deref(), Some("cx_sf"));
    }

    #[tokio::test]
    async fn name_index_is_type_agnostic_about_the_facet() {
        // The qualifier is a state here (a national park), not a city — same
        // machinery, no place-specific assumptions.
        let g = SqliteStore::open_in_memory().unwrap();
        g.create_entity("cx_park", "wikidata:Q180402", Some("Park"), Some("Yosemite")).await
            .unwrap();
        g.index_name("yosemite", &["california".into()], "cx_park", Some("t")).await.unwrap();

        let q = NameQuery {
            name: Some("Yosemite".into()),
            qualifiers: vec!["California".into()],
            ..Default::default()
        };
        let hit = resolve_name_local(&g, &q).await.unwrap().expect("hit");
        assert_eq!(hit.canonical_id.as_deref(), Some("cx_park"));
    }

    #[tokio::test]
    async fn wikidata_input_completes_via_output_keyed_gate() {
        // M2: a direct wikidata id must still harvest website+tmdb+imdb. The hub
        // gate keys on OUTPUT kinds, so a wikidata input no longer self-skips.
        let g = SqliteStore::open_in_memory().unwrap();
        let wd = WikidataResolver::new(
            ExternalId::wikidata("Q83495").unwrap(),
            Arc::new(FixtureTransport::from_pairs(vec![])),
        )
        .url()
        .unwrap();
        let transport = FixtureTransport::from_pairs(vec![(
            "GET",
            &wd,
            json!({"results": {"bindings": [{
                "item": {"value": "http://www.wikidata.org/entity/Q83495"},
                "imdb": {"value": "tt0133093"},
                "website": {"value": "https://www.warnerbros.com/movies/matrix"},
                "tmdb": {"value": "603"}
            }]}}),
        )]);
        let ctx = CompletionCtx::new(Arc::new(transport));
        let input = EntityRecord {
            same_as: vec![ExternalId::wikidata("Q83495").unwrap()],
            ..Default::default()
        };
        let out = resolve_and_complete(&g, &input, &ctx).await.unwrap();
        let keys: Vec<String> = out.same_as.iter().map(|i| i.key()).collect();
        assert!(keys.contains(&"domain:warnerbros.com".to_string()), "{keys:?}");
        assert!(keys.contains(&"tmdb:603".to_string()), "{keys:?}");
        assert!(keys.contains(&"imdb:tt0133093".to_string()), "{keys:?}");

        // Idempotent: a second run adds no edges (all output edges present).
        let second = resolve_and_complete(&g, &input, &ctx).await.unwrap();
        assert_eq!(second.new_edges, 0, "second run should add no edges");
    }

    #[tokio::test]
    async fn tmdb_input_crosswalks_to_imdb_and_wikidata() {
        // M2: a direct tmdb id crosswalks out to imdb + wikidata.
        let g = SqliteStore::open_in_memory().unwrap();
        let ext = "https://api.themoviedb.org/3/movie/603/external_ids?api_key=";
        let transport = FixtureTransport::from_pairs(vec![(
            "GET",
            ext,
            json!({"imdb_id": "tt0133093", "wikidata_id": "Q83495"}),
        )]);
        let ctx = CompletionCtx::new(Arc::new(transport));
        let input = EntityRecord {
            same_as: vec![ExternalId::new("tmdb", "603").unwrap()],
            ..Default::default()
        };
        let out = resolve_and_complete(&g, &input, &ctx).await.unwrap();
        let keys: Vec<String> = out.same_as.iter().map(|i| i.key()).collect();
        assert!(keys.contains(&"imdb:tt0133093".to_string()), "{keys:?}");
        assert!(keys.contains(&"wikidata:Q83495".to_string()), "{keys:?}");

        let second = resolve_and_complete(&g, &input, &ctx).await.unwrap();
        assert_eq!(second.new_edges, 0, "second run should add no edges");
    }

    #[tokio::test]
    async fn street_without_placekey_reports_place_unique_not_placekey() {
        // M3: a street was supplied but the Placekey hub returned nothing, so the
        // reason must reflect the ACTUAL evidence (a unique place_id), not the
        // intent (PlacekeyAddress).
        let g = SqliteStore::open_in_memory().unwrap();
        let query = NameQuery {
            entity_type: Some("restaurant".into()),
            name: Some("Blue Bottle Coffee".into()),
            street: Some("300 Webster St".into()),
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
            ExternalId::google_place_id("PID_X").unwrap(),
            String::new(),
            Arc::new(FixtureTransport::from_pairs(vec![])),
        )
        .url();
        // Placekey returns an empty object → no placekey edge.
        let transport = FixtureTransport::from_pairs(vec![
            ("POST", "https://api.placekey.io/v1/placekey", json!({})),
            ("POST", &text_url, json!({"places": [{"id": "PID_X"}]})),
            (
                "GET",
                &details_url,
                json!({"displayName": {"text": "Blue Bottle Coffee"}, "websiteUri": "https://bluebottlecoffee.com/"}),
            ),
        ]);
        let ctx = CompletionCtx::new(Arc::new(transport));
        let out = resolve_name(&g, &query, &ctx).await.unwrap();
        assert!(
            !out.same_as.iter().any(|i| i.kind_tag() == "placekey"),
            "no placekey expected: {:?}",
            out.same_as
        );
        assert_eq!(out.confidence_reason, ConfidenceReason::PlaceUniqueMatch);
        assert!((out.confidence - crate::confidence::PLACE_UNIQUE).abs() < 1e-6);
    }

    #[tokio::test]
    async fn resolve_name_persists_display_name_and_indexes_alias() {
        // M4 + M6: the hub displayName becomes the stored name, and BOTH the query
        // name and the resolved displayName are indexed under the qualifiers, so a
        // later LOCAL query by either string hits the same entity.
        let g = SqliteStore::open_in_memory().unwrap();
        let query = NameQuery {
            entity_type: Some("restaurant".into()),
            name: Some("Nickname".into()),
            city: Some("Portland".into()),
            ..Default::default()
        };
        let text_url = PlaceTextSearchResolver::new(
            TextSearchInput::Text(query.text_query()),
            String::new(),
            Arc::new(FixtureTransport::from_pairs(vec![])),
        )
        .url();
        let details_url = PlaceDetailsResolver::new(
            ExternalId::google_place_id("PID_OFFICIAL").unwrap(),
            String::new(),
            Arc::new(FixtureTransport::from_pairs(vec![])),
        )
        .url();
        let transport = FixtureTransport::from_pairs(vec![
            ("POST", &text_url, json!({"places": [{"id": "PID_OFFICIAL"}]})),
            (
                "GET",
                &details_url,
                json!({"displayName": {"text": "Official Name"}, "websiteUri": "https://official.example/"}),
            ),
        ]);
        let ctx = CompletionCtx::new(Arc::new(transport));
        let first = resolve_name(&g, &query, &ctx).await.unwrap();
        let cid = first.canonical_id.clone().expect("resolve should succeed");
        // Stored name is the hub displayName, not the query nickname.
        assert_eq!(first.name.as_deref(), Some("Official Name"));

        // A LOCAL query for the OFFICIAL name + same qualifier hits the entity.
        let official = NameQuery {
            entity_type: Some("restaurant".into()),
            name: Some("Official Name".into()),
            city: Some("Portland".into()),
            ..Default::default()
        };
        let hit = resolve_name_local(&g, &official).await
            .unwrap()
            .expect("alias local hit");
        assert_eq!(hit.canonical_id.as_deref(), Some(cid.as_str()));
        assert_eq!(hit.confidence_reason, ConfidenceReason::LocalNameMatch);

        // The original nickname still resolves locally too.
        let nick = resolve_name_local(&g, &query).await
            .unwrap()
            .expect("query-name local hit");
        assert_eq!(nick.canonical_id.as_deref(), Some(cid.as_str()));
    }

    #[tokio::test]
    async fn local_name_does_not_serve_wrong_entity_on_coarse_overlap() {
        // H5: an entity cached under {boston, us} must NOT be returned for a
        // {seattle, us} query sharing only the coarse country — it misses locally
        // (so the caller reaches out) rather than confidently serving Boston.
        let g = SqliteStore::open_in_memory().unwrap();
        g.create_entity("cx_bos", "google_place_id:BOS", None, Some("Acme")).await
            .unwrap();
        g.index_name("acme", &["boston".into(), "us".into()], "cx_bos", Some("t")).await
            .unwrap();

        let seattle = NameQuery {
            name: Some("Acme".into()),
            city: Some("Seattle".into()),
            country: Some("US".into()),
            ..Default::default()
        };
        assert!(resolve_name_local(&g, &seattle).await.unwrap().is_none());

        let boston = NameQuery {
            name: Some("Acme".into()),
            city: Some("Boston".into()),
            country: Some("US".into()),
            ..Default::default()
        };
        let hit = resolve_name_local(&g, &boston).await
            .unwrap()
            .expect("boston hits");
        assert_eq!(hit.canonical_id.as_deref(), Some("cx_bos"));
    }

    #[tokio::test]
    async fn t1_identical_repeat_with_street_hits_locally_zero_external() {
        // T1 (regression): name+street+city establishes E; the IDENTICAL repeat
        // (same street) still hits locally with zero external calls — the cache
        // value prop is preserved for a same-or-more-specific query.
        let g = SqliteStore::open_in_memory().unwrap();
        let query = NameQuery {
            entity_type: Some("restaurant".into()),
            name: Some("Blue Bottle Coffee".into()),
            street: Some("1 Ferry Building".into()),
            city: Some("San Francisco".into()),
            ..Default::default()
        };
        let text_url = PlaceTextSearchResolver::new(
            TextSearchInput::Text(query.text_query()),
            String::new(),
            Arc::new(FixtureTransport::from_pairs(vec![])),
        )
        .url();
        let details_url = PlaceDetailsResolver::new(
            ExternalId::google_place_id("PID_FERRY").unwrap(),
            String::new(),
            Arc::new(FixtureTransport::from_pairs(vec![])),
        )
        .url();
        let transport = FixtureTransport::from_pairs(vec![
            ("POST", "https://api.placekey.io/v1/placekey", json!({"placekey": "222-227@5vg-7gr-abc"})),
            ("POST", &text_url, json!({"places": [{"id": "PID_FERRY"}]})),
            ("GET", &details_url, json!({"websiteUri": "https://bluebottlecoffee.com/"})),
        ]);
        let ctx = CompletionCtx::new(Arc::new(transport));
        let first = resolve_name(&g, &query, &ctx).await.unwrap();
        let cid = first.canonical_id.clone().expect("establish E");

        // Local-only (no transport at all) → proves zero external calls.
        let second = resolve_name_local(&g, &query).await.unwrap().expect("cached local hit");
        assert_eq!(second.canonical_id.as_deref(), Some(cid.as_str()));
        assert_eq!(second.confidence_reason, ConfidenceReason::LocalNameMatch);
    }

    #[tokio::test]
    async fn t2_local_name_street_entity_not_served_to_coarser_city_query() {
        // THE FIX (local half): an entity established under {street, city} must NOT
        // be confidently returned to a later name+city (no street) query — its
        // establishing set is not a subset of the coarser query's tokens, so the
        // lookup misses locally (deferring to a hub) rather than wrong-binding.
        let g = SqliteStore::open_in_memory().unwrap();
        g.create_entity("cx_ferry", "google_place_id:FERRY", None, Some("Blue Bottle Coffee")).await
            .unwrap();
        // Established with the full set (street folded in), as resolve_name now does.
        g.index_name(
            "blue bottle coffee",
            &["1 ferry building".into(), "san francisco".into()],
            "cx_ferry",
            Some("t"),
        ).await
        .unwrap();

        // Coarser query (no street) under-specifies the establishing set → miss.
        let coarse = NameQuery {
            name: Some("Blue Bottle Coffee".into()),
            city: Some("San Francisco".into()),
            ..Default::default()
        };
        assert!(
            resolve_name_local(&g, &coarse).await.unwrap().is_none(),
            "a name+city query must not be confidently served a name+street entity"
        );

        // The same-or-more-specific query still hits (Q ⊇ S).
        let specific = NameQuery {
            name: Some("Blue Bottle Coffee".into()),
            street: Some("1 Ferry Building".into()),
            city: Some("San Francisco".into()),
            ..Default::default()
        };
        let hit = resolve_name_local(&g, &specific).await.unwrap().expect("specific hits");
        assert_eq!(hit.canonical_id.as_deref(), Some("cx_ferry"));
    }

    #[tokio::test]
    async fn t5_coarse_query_over_multiple_known_specifics_returns_candidates() {
        // T5: two specific entities under the SAME name+city but different streets.
        // A coarse name+city query (no street) under-specifies both → returns BOTH
        // as candidates locally, zero external (not a pick, not a miss).
        let g = SqliteStore::open_in_memory().unwrap();
        g.create_entity("cx_a", "google_place_id:A", None, Some("Blue Bottle Coffee")).await
            .unwrap();
        g.create_entity("cx_b", "google_place_id:B", None, Some("Blue Bottle Coffee")).await
            .unwrap();
        g.index_name(
            "blue bottle coffee",
            &["100 a st".into(), "san francisco".into()],
            "cx_a",
            Some("t"),
        ).await
        .unwrap();
        g.index_name(
            "blue bottle coffee",
            &["200 b st".into(), "san francisco".into()],
            "cx_b",
            Some("t"),
        ).await
        .unwrap();

        let coarse = NameQuery {
            name: Some("Blue Bottle Coffee".into()),
            city: Some("San Francisco".into()),
            ..Default::default()
        };
        let out = resolve_name_local(&g, &coarse).await.unwrap().expect("ambiguous");
        assert_eq!(out.status, Status::Unresolved);
        assert_eq!(out.confidence_reason, ConfidenceReason::AmbiguousAmongN(2));
        assert_eq!(out.candidates.len(), 2);
    }

    #[tokio::test]
    async fn t6_type_agnostic_qualifier_only_no_confident_pick() {
        // T6: type-agnostic proof using ONLY --qualifier tokens (no street/city).
        // "Nova" established under {2019} and under {2021}. A bare `--name Nova`
        // must NOT confidently return one — the rule is generic, not geo.
        let g = SqliteStore::open_in_memory().unwrap();
        g.create_entity("cx_2019", "wikidata:Q19", Some("Movie"), Some("Nova")).await
            .unwrap();
        g.create_entity("cx_2021", "wikidata:Q21", Some("Movie"), Some("Nova")).await
            .unwrap();
        g.index_name("nova", &["2019".into()], "cx_2019", Some("t")).await.unwrap();
        g.index_name("nova", &["2021".into()], "cx_2021", Some("t")).await.unwrap();

        let bare = NameQuery { name: Some("Nova".into()), ..Default::default() };
        let out = resolve_name_local(&g, &bare).await.unwrap().expect("ambiguous, not a pick");
        assert_eq!(out.status, Status::Unresolved);
        assert_eq!(out.candidates.len(), 2);

        // A disambiguating qualifier narrows to the one entity (same machinery).
        let y2019 = NameQuery {
            name: Some("Nova".into()),
            qualifiers: vec!["2019".into()],
            ..Default::default()
        };
        let hit = resolve_name_local(&g, &y2019).await.unwrap().expect("unique");
        assert_eq!(hit.canonical_id.as_deref(), Some("cx_2019"));
    }

    #[tokio::test]
    async fn cardinality_memory_serves_repeat_ambiguous_query_locally() {
        // T3 (unit): a name+city hub search that returns MULTIPLE records the
        // ambiguity; a later IDENTICAL query is answered from local memory with
        // zero external calls (no transport at all on the repeat).
        let g = SqliteStore::open_in_memory().unwrap();
        let query = NameQuery {
            entity_type: Some("restaurant".into()),
            name: Some("Joe's Pizza".into()),
            city: Some("New York".into()),
            ..Default::default()
        };
        let text_url = PlaceTextSearchResolver::new(
            TextSearchInput::Text(query.text_query()),
            String::new(),
            Arc::new(FixtureTransport::from_pairs(vec![])),
        )
        .url();
        let transport = FixtureTransport::from_pairs(vec![
            ("POST", "https://api.placekey.io/v1/placekey", json!({})),
            (
                "POST",
                &text_url,
                json!({"places": [{"id": "JOE_A"}, {"id": "JOE_B"}, {"id": "JOE_C"}]}),
            ),
        ]);
        let ctx = CompletionCtx::new(Arc::new(transport));
        let first = resolve_name(&g, &query, &ctx).await.unwrap();
        assert_eq!(first.confidence_reason, ConfidenceReason::AmbiguousAmongN(3));
        assert!(first.canonical_id.is_none());

        // Local-only repeat (no transport) → ambiguous from memory, zero external.
        let repeat = resolve_name_local(&g, &query).await.unwrap().expect("from memory");
        assert_eq!(repeat.confidence_reason, ConfidenceReason::AmbiguousAmongN(3));
        assert_eq!(repeat.candidates.len(), 3);
        assert_eq!(repeat.harvested, 0);
        assert_eq!(repeat.new_edges, 0);
    }

    #[tokio::test]
    async fn kibatsu_unique_flow_coarse_repeat_hits_locally() {
        use crate::hubs::{PlaceDetailsResolver, PlaceTextSearchResolver, TextSearchInput};

        let g = SqliteStore::open_in_memory().unwrap();

        // Step 1: name + street + city, --complete. The hub text-search returns
        // exactly ONE place → resolves + mints cx, indexed with the STREET folded
        // into its establishing set {500 main st, san francisco}.
        let specific = NameQuery {
            entity_type: Some("restaurant".into()),
            name: Some("Kibatsu".into()),
            street: Some("500 Main St".into()),
            city: Some("San Francisco".into()),
            ..Default::default()
        };
        let text_url = PlaceTextSearchResolver::new(
            TextSearchInput::Text(specific.text_query()),
            String::new(),
            Arc::new(FixtureTransport::from_pairs(vec![])),
        )
        .url();
        let details_url = PlaceDetailsResolver::new(
            ExternalId::google_place_id("KIBATSU_MAIN").unwrap(),
            String::new(),
            Arc::new(FixtureTransport::from_pairs(vec![])),
        )
        .url();
        let t1 = FixtureTransport::from_pairs(vec![
            ("POST", "https://api.placekey.io/v1/placekey", json!({})),
            ("POST", &text_url, json!({"places": [{"id": "KIBATSU_MAIN"}]})),
            (
                "GET",
                &details_url,
                json!({"displayName": {"text": "Kibatsu"}, "websiteUri": "https://kibatsu.example/"}),
            ),
        ]);
        let step1 = resolve_name(&g, &specific, &CompletionCtx::new(Arc::new(t1))).await.unwrap();
        let cid = step1.canonical_id.clone().expect("step 1 resolves");

        // Step 2: coarse city-only query, LOCAL ONLY → MISS. Uniqueness of the
        // coarse (name, city) query was never confirmed, and the street-
        // established entity does not satisfy the superset rule.
        let coarse = NameQuery {
            entity_type: Some("restaurant".into()),
            name: Some("Kibatsu".into()),
            city: Some("San Francisco".into()),
            ..Default::default()
        };
        assert!(
            resolve_name_local(&g, &coarse).await.unwrap().is_none(),
            "step 2 (coarse local-only, no memory yet) must miss"
        );

        // Step 3: coarse city-only query WITH --complete. Hub returns exactly ONE
        // (the same place) → resolves to cx AND records the coarse (name, city)
        // as unique. The place_id already carries a domain, so Place Details is
        // skipped — the transport needs only the text search.
        let coarse_text_url = PlaceTextSearchResolver::new(
            TextSearchInput::Text(coarse.text_query()),
            String::new(),
            Arc::new(FixtureTransport::from_pairs(vec![])),
        )
        .url();
        let t3 = FixtureTransport::from_pairs(vec![(
            "POST",
            &coarse_text_url,
            json!({"places": [{"id": "KIBATSU_MAIN"}]}),
        )]);
        let step3 = resolve_name(&g, &coarse, &CompletionCtx::new(Arc::new(t3))).await.unwrap();
        assert_eq!(step3.canonical_id.as_deref(), Some(cid.as_str()), "step 3 resolves to cx");

        // Step 4: coarse city-only query, LOCAL ONLY repeat → now HITS from unique
        // memory (zero external), local_name_match, nothing harvested.
        let step4 = resolve_name_local(&g, &coarse).await
            .unwrap()
            .expect("step 4 hits from unique memory");
        assert_eq!(step4.canonical_id.as_deref(), Some(cid.as_str()));
        assert_eq!(step4.confidence_reason, ConfidenceReason::LocalNameMatch);
        assert_eq!(step4.harvested, 0);
    }

    #[tokio::test]
    async fn local_lookup_flips_from_unique_to_ambiguous() {
        // Flip on change: a (name, Q) recorded UNIQUE (hub returned one) that a
        // later hub call proves MULTIPLE must overwrite the unique row with an
        // ambiguous one — a subsequent LOCAL query then returns ambiguous_among_n,
        // never the stale unique hit. `resolve_name` short-circuits on the local
        // unique hit, so the two hub outcomes are modeled by the two graph records
        // they would write; the local-consult behavior across the flip is the
        // subject under test.
        let g = SqliteStore::open_in_memory().unwrap();
        g.create_entity("cx_one", "google_place_id:ONE", None, Some("Joe's Pizza")).await
            .unwrap();
        let quals = vec!["new york".to_string()];
        let q = NameQuery {
            name: Some("Joe's Pizza".into()),
            city: Some("New York".into()),
            ..Default::default()
        };

        // Hub returned ONE → unique. A coarse local query hits it (via the unique
        // fallback: nothing is indexed in name_index under this key).
        g.record_name_unique("joe's pizza", &quals, "cx_one").await.unwrap();
        let hit = resolve_name_local(&g, &q).await.unwrap().expect("unique local hit");
        assert_eq!(hit.canonical_id.as_deref(), Some("cx_one"));
        assert_eq!(hit.confidence_reason, ConfidenceReason::LocalNameMatch);

        // Later hub call returns MULTIPLE → flips memory to ambiguous.
        let cands = vec![
            (String::new(), "google_place_id:ONE".into(), None),
            (String::new(), "google_place_id:TWO".into(), None),
        ];
        g.record_name_cardinality("joe's pizza", &quals, &cands).await.unwrap();

        let out = resolve_name_local(&g, &q).await.unwrap().expect("ambiguous from memory");
        assert_eq!(out.confidence_reason, ConfidenceReason::AmbiguousAmongN(2));
        assert!(out.canonical_id.is_none(), "must not serve the stale unique hit");
    }

    #[tokio::test]
    async fn unique_memory_does_not_override_genuine_graph_ambiguity() {
        // Safety: a stale unique fact for (name, Q) must NOT mask a genuine
        // multi-entity ambiguity discoverable from the graph. Two same-name
        // entities are indexed under the SAME establishing set = the query set, so
        // the graph scan finds both; the unique fallback must lose to that.
        let g = SqliteStore::open_in_memory().unwrap();
        g.create_entity("cx_a", "google_place_id:A", None, Some("Nova")).await.unwrap();
        g.create_entity("cx_b", "google_place_id:B", None, Some("Nova")).await.unwrap();
        g.index_name("nova", &["berlin".into()], "cx_a", Some("t")).await.unwrap();
        g.index_name("nova", &["berlin".into()], "cx_b", Some("t")).await.unwrap();
        // A stale unique fact naming just one of them.
        g.record_name_unique("nova", &["berlin".into()], "cx_a").await.unwrap();

        let q = NameQuery {
            name: Some("Nova".into()),
            city: Some("Berlin".into()),
            ..Default::default()
        };
        let out = resolve_name_local(&g, &q).await.unwrap().expect("ambiguous from graph");
        assert_eq!(out.confidence_reason, ConfidenceReason::AmbiguousAmongN(2));
        assert!(out.canonical_id.is_none());
    }

    // ---------------------------------------------------------------
    // U3: hub routing, candidate selection, candidate enrichment
    // ---------------------------------------------------------------

    #[test]
    fn routing_is_by_nsid_leaf_case_insensitively() {
        // Places — the only billable hub — is entered ONLY by an explicit
        // place-ish type.
        for t in ["place", "localBusiness", "FoodEstablishment", "restaurant"] {
            assert_eq!(name_hub_for(Some(t)), NameHub::Places, "{t}");
        }
        for t in ["movie", "MOVIE", "tvSeries"] {
            assert_eq!(name_hub_for(Some(t)), NameHub::Tmdb, "{t}");
        }
        // A full NSID routes on its leaf, both forms of the same type.
        assert_eq!(
            name_hub_for(Some("info.cursive.creativeWork.movie")),
            NameHub::Tmdb
        );
        assert_eq!(
            name_hub_for(Some("info.cursive.organization.restaurant")),
            NameHub::Places
        );
        // Unknown and ABSENT both fall back to the free, type-agnostic hub — a
        // mis-typed query must never reach the metered one.
        assert_eq!(name_hub_for(Some("book")), NameHub::Wikidata);
        assert_eq!(name_hub_for(None), NameHub::Wikidata);
        assert_eq!(name_hub_for(Some("")), NameHub::Wikidata);
    }

    fn avatar_multi() -> serde_json::Value {
        json!({ "results": [
            { "id": 19995, "media_type": "movie", "title": "Avatar",
              "release_date": "2009-12-15" },
            { "id": 76600, "media_type": "movie", "title": "Avatar: The Way of Water",
              "release_date": "2022-12-14" },
            { "id": 246, "media_type": "tv", "name": "Avatar: The Last Airbender",
              "first_air_date": "2005-02-21" }
        ]})
    }

    fn tmdb_search_url(query: &str) -> String {
        crate::hubs::TmdbSearchResolver::new(
            query,
            String::new(),
            Arc::new(FixtureTransport::from_pairs(vec![])),
        )
        .url()
    }

    #[tokio::test]
    async fn movie_name_routes_to_tmdb_and_spans_both_ambiguity_cases() {
        // A film query must NOT go to Google Places. The candidate list has to
        // cover both shapes of name ambiguity at once:
        //   (b) unrelated franchises colliding on a name — the 2009 film vs the
        //       Nickelodeon series, which share nothing;
        //   (c) one franchise, several works — 2009 vs 2022, told apart ONLY by
        //       the year, so the year has to be in the label.
        let g = SqliteStore::open_in_memory().unwrap();
        let url = tmdb_search_url("Avatar");
        let transport = FixtureTransport::from_pairs(vec![("GET", &url, avatar_multi())]);
        let ctx = CompletionCtx::new(Arc::new(transport));
        let query = NameQuery {
            name: Some("Avatar".into()),
            entity_type: Some("movie".into()),
            ..Default::default()
        };

        let out = resolve_name(&g, &query, &ctx).await.unwrap();
        assert_eq!(out.status, Status::Unresolved);
        assert_eq!(out.confidence_reason, ConfidenceReason::AmbiguousAmongN(3));
        let labels: Vec<String> = out
            .candidates
            .iter()
            .map(|c| c.name.clone().expect("every candidate is choosable"))
            .collect();
        assert!(labels.contains(&"Avatar (2009 film)".to_string()), "{labels:?}");
        assert!(
            labels.contains(&"Avatar: The Way of Water (2022 film)".to_string()),
            "case (c) — same franchise, distinguished only by year: {labels:?}"
        );
        assert!(
            labels.contains(&"Avatar: The Last Airbender (2005 TV series)".to_string()),
            "case (b) — a different franchise entirely: {labels:?}"
        );
        // The retry keys are echo-able refs, and the series is NOT keyed in the
        // movie namespace.
        let refs: Vec<String> = out.candidates.iter().map(|c| c.anchor.clone()).collect();
        assert_eq!(
            refs,
            vec![
                "tmdb:19995".to_string(),
                "tmdb:76600".to_string(),
                "url:themoviedb.org/tv/246".to_string()
            ]
        );

        // The verdict is remembered: the identical query repeats with zero calls.
        let repeat = resolve_name_local(&g, &query).await.unwrap().expect("from memory");
        assert_eq!(repeat.confidence_reason, ConfidenceReason::AmbiguousAmongN(3));
        assert_eq!(repeat.candidates.len(), 3);
    }

    #[tokio::test]
    async fn a_year_qualifier_narrows_a_franchise_to_one_work() {
        // Case (c) resolved: the year is in the hub's own description of each
        // candidate, so `--qualifier 2009` picks one WITHOUT a second hub call.
        let g = SqliteStore::open_in_memory().unwrap();
        let url = tmdb_search_url("Avatar");
        let transport = FixtureTransport::from_pairs(vec![("GET", &url, avatar_multi())]);
        let ctx = CompletionCtx::new(Arc::new(transport));
        let query = NameQuery {
            name: Some("Avatar".into()),
            entity_type: Some("movie".into()),
            qualifiers: vec!["2009".into()],
            ..Default::default()
        };

        let out = resolve_name(&g, &query, &ctx).await.unwrap();
        assert!(out.canonical_id.is_some(), "narrowed to one → resolves");
        assert_eq!(out.confidence_reason, ConfidenceReason::PlaceUniqueMatch);
        assert_eq!(out.name.as_deref(), Some("Avatar"));
        let keys: Vec<String> = out.same_as.iter().map(|i| i.key()).collect();
        assert_eq!(keys, vec!["tmdb:19995".to_string()]);

        // And it is cached: the same query repeats locally, zero external.
        let repeat = resolve_name_local(&g, &query).await.unwrap().expect("cached");
        assert_eq!(repeat.canonical_id, out.canonical_id);

        // A qualifier that matches NOTHING must not erase the list (fail-open):
        // the ambiguity is still reported rather than a zero-candidate refusal.
        let g2 = SqliteStore::open_in_memory().unwrap();
        let transport2 = FixtureTransport::from_pairs(vec![("GET", &url, avatar_multi())]);
        let bogus = NameQuery {
            name: Some("Avatar".into()),
            entity_type: Some("movie".into()),
            qualifiers: vec!["1873".into()],
            ..Default::default()
        };
        let out2 = resolve_name(&g2, &bogus, &CompletionCtx::new(Arc::new(transport2)))
            .await
            .unwrap();
        assert_eq!(out2.confidence_reason, ConfidenceReason::AmbiguousAmongN(3));
    }

    #[tokio::test]
    async fn an_untyped_name_query_falls_back_to_wikidata() {
        // No type → the free, type-agnostic hub. (A park, here — nothing about
        // this path is place- or film-specific.)
        let g = SqliteStore::open_in_memory().unwrap();
        let probe = crate::hubs::WikidataSearchResolver::new(
            "Yosemite",
            Arc::new(FixtureTransport::from_pairs(vec![])),
        );
        let url = probe.url();
        let transport = FixtureTransport::from_pairs(vec![(
            "GET",
            &url,
            json!({ "search": [
                { "id": "Q180402", "label": "Yosemite National Park",
                  "description": "national park in California, United States" },
                { "id": "Q1064523", "label": "Yosemite Valley",
                  "description": "valley in California" }
            ]}),
        )]);
        let ctx = CompletionCtx::new(Arc::new(transport));
        let query = NameQuery {
            name: Some("Yosemite".into()),
            ..Default::default()
        };
        let out = resolve_name(&g, &query, &ctx).await.unwrap();
        assert_eq!(out.confidence_reason, ConfidenceReason::AmbiguousAmongN(2));
        assert_eq!(
            out.candidates[0].name.as_deref(),
            Some("Yosemite National Park (national park in California, United States)")
        );
        assert_eq!(out.candidates[0].anchor, "wikidata:Q180402");
    }

    #[tokio::test]
    async fn a_hub_search_with_no_results_is_a_reported_miss() {
        let g = SqliteStore::open_in_memory().unwrap();
        let url = tmdb_search_url("Nonesuch");
        let transport =
            FixtureTransport::from_pairs(vec![("GET", &url, json!({ "results": [] }))]);
        let out = resolve_name(
            &g,
            &NameQuery {
                name: Some("Nonesuch".into()),
                entity_type: Some("movie".into()),
                ..Default::default()
            },
            &CompletionCtx::new(Arc::new(transport)),
        )
        .await
        .unwrap();
        assert_eq!(out.status, Status::Unresolved);
        assert_eq!(out.confidence_reason, ConfidenceReason::NeedsStrongerIdentifier);
        assert!(out.hint.unwrap().contains("tmdb"));
    }

    #[tokio::test]
    async fn the_candidate_cap_bounds_the_list_but_not_the_stored_count() {
        // 12 hits: the caller gets 8 and is TOLD 4 were dropped, the count stays
        // truthful (12), and the memory keeps all 12 — so a display cap never
        // hardens into "there are exactly 8 of these".
        let g = SqliteStore::open_in_memory().unwrap();
        let results: Vec<serde_json::Value> = (0..12)
            .map(|i| {
                json!({ "id": 1000 + i, "media_type": "movie",
                        "title": format!("Nova {i}"), "release_date": format!("20{:02}-01-01", i) })
            })
            .collect();
        let url = tmdb_search_url("Nova");
        let transport =
            FixtureTransport::from_pairs(vec![("GET", &url, json!({ "results": results }))]);
        let query = NameQuery {
            name: Some("Nova".into()),
            entity_type: Some("movie".into()),
            ..Default::default()
        };
        let out = resolve_name(&g, &query, &CompletionCtx::new(Arc::new(transport)))
            .await
            .unwrap();
        assert_eq!(out.candidates.len(), CANDIDATE_CAP);
        assert_eq!(out.confidence_reason, ConfidenceReason::AmbiguousAmongN(12));
        let hint = out.hint.expect("the drop is reported, never silent");
        assert!(hint.contains("12") && hint.contains("4"), "hint={hint}");

        // The stored memory holds all 12 (the local repeat still says 12, and
        // still caps its own view at 8).
        let repeat = resolve_name_local(&g, &query).await.unwrap().expect("from memory");
        assert_eq!(repeat.confidence_reason, ConfidenceReason::AmbiguousAmongN(12));
        assert_eq!(repeat.candidates.len(), CANDIDATE_CAP);
    }

    fn text_search_url(query: &NameQuery) -> String {
        PlaceTextSearchResolver::new(
            TextSearchInput::Text(query.text_query()),
            String::new(),
            Arc::new(FixtureTransport::from_pairs(vec![])),
        )
        .url()
    }

    #[tokio::test]
    async fn ambiguous_places_are_labelled_from_the_search_response_alone() {
        // The Souvla shape: one brand, two locations. The text-search field mask
        // already carries name + address, so each candidate is choosable with NO
        // Place Details call — the transport has no details fixture at all, which
        // is what proves none was made.
        let g = SqliteStore::open_in_memory().unwrap();
        let query = NameQuery {
            name: Some("Souvla".into()),
            entity_type: Some("restaurant".into()),
            city: Some("San Francisco".into()),
            ..Default::default()
        };
        let url = text_search_url(&query);
        let transport = FixtureTransport::from_pairs(vec![(
            "POST",
            &url,
            json!({ "places": [
                { "id": "ChIJ_hayes", "displayName": { "text": "Souvla" },
                  "formattedAddress": "517 Hayes St, San Francisco, CA" },
                { "id": "ChIJ_marina", "displayName": { "text": "Souvla" },
                  "formattedAddress": "2272 Chestnut St, San Francisco, CA" }
            ]}),
        )]);
        let out = resolve_name(&g, &query, &CompletionCtx::new(Arc::new(transport)))
            .await
            .unwrap();
        assert_eq!(out.confidence_reason, ConfidenceReason::AmbiguousAmongN(2));
        assert_eq!(
            out.candidates[0].name.as_deref(),
            Some("Souvla (517 Hayes St, San Francisco, CA)")
        );
        assert_eq!(
            out.candidates[1].name.as_deref(),
            Some("Souvla (2272 Chestnut St, San Francisco, CA)")
        );
        // Both carry an echo-able ref alongside the label.
        assert_eq!(out.candidates[0].anchor, "google_place_id:ChIJ_hayes");
        assert!(out.hint.is_none(), "nothing was dropped: {:?}", out.hint);
    }

    #[tokio::test]
    async fn place_details_fanout_is_capped_and_the_shortfall_reported() {
        // A search response that describes nothing (an older/narrower field mask,
        // or a place with no name): each un-graphed candidate then needs its own
        // Place Details call, and that is the one metered fan-out in the system.
        // Seven candidates, budget five → five labelled, two reported.
        let g = SqliteStore::open_in_memory().unwrap();
        let query = NameQuery {
            name: Some("Joe's Pizza".into()),
            entity_type: Some("restaurant".into()),
            city: Some("New York".into()),
            ..Default::default()
        };
        let url = text_search_url(&query);
        let ids: Vec<String> = (0..7).map(|i| format!("JOE_{i}")).collect();
        let places: Vec<serde_json::Value> = ids.iter().map(|id| json!({ "id": id })).collect();
        let details: Vec<String> = ids
            .iter()
            .map(|id| {
                PlaceDetailsResolver::new(
                    ExternalId::google_place_id(id).unwrap(),
                    String::new(),
                    Arc::new(FixtureTransport::from_pairs(vec![])),
                )
                .url()
            })
            .collect();
        let mut pairs: Vec<(&str, &str, serde_json::Value)> =
            vec![("POST", url.as_str(), json!({ "places": places }))];
        for (i, d) in details.iter().enumerate() {
            pairs.push((
                "GET",
                d.as_str(),
                json!({ "displayName": { "text": "Joe's Pizza" },
                        "formattedAddress": format!("{i} Bleecker St, New York, NY") }),
            ));
        }
        let out = resolve_name(
            &g,
            &query,
            &CompletionCtx::new(Arc::new(FixtureTransport::from_pairs(pairs))),
        )
        .await
        .unwrap();

        assert_eq!(out.confidence_reason, ConfidenceReason::AmbiguousAmongN(7));
        let labelled = out.candidates.iter().filter(|c| c.name.is_some()).count();
        assert_eq!(
            labelled, PLACE_DETAILS_FANOUT_CAP,
            "the fan-out budget is spent in rank order, on the candidates the caller will see"
        );
        assert_eq!(
            out.candidates[0].name.as_deref(),
            Some("Joe's Pizza (0 Bleecker St, New York, NY)")
        );
        let hint = out.hint.expect("the unlabelled candidates are reported");
        assert!(hint.contains('2'), "hint={hint}");
    }

    #[tokio::test]
    async fn a_graphed_place_candidate_costs_no_hub_call() {
        // A candidate we already know is labelled and keyed from the graph — the
        // local-first rule applies to candidate enrichment too.
        let g = SqliteStore::open_in_memory().unwrap();
        g.create_entity("cx_known", "google_place_id:KNOWN", None, Some("Souvla Hayes"))
            .await
            .unwrap();
        g.attach("google_place_id:KNOWN", "cx_known").await.unwrap();
        let query = NameQuery {
            name: Some("Souvla".into()),
            entity_type: Some("restaurant".into()),
            city: Some("San Francisco".into()),
            ..Default::default()
        };
        let url = text_search_url(&query);
        // No details fixture for KNOWN: if the code reached out for it, the label
        // would be missing.
        let transport = FixtureTransport::from_pairs(vec![(
            "POST",
            &url,
            json!({ "places": [
                { "id": "KNOWN" },
                { "id": "OTHER", "displayName": { "text": "Souvla" },
                  "formattedAddress": "2272 Chestnut St" }
            ]}),
        )]);
        let out = resolve_name(&g, &query, &CompletionCtx::new(Arc::new(transport)))
            .await
            .unwrap();
        assert_eq!(out.candidates[0].canonical_id, "cx_known");
        assert_eq!(out.candidates[0].name.as_deref(), Some("Souvla Hayes"));
        assert!(out.hint.is_none(), "nothing dropped: {:?}", out.hint);
    }

}
