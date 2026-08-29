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
use async_trait::async_trait;

use crate::anchor;
use crate::confidence::{score, ConfidenceReason};
use crate::kind::Grain;
use crate::model::{EntityRecord, ExternalId};
use crate::store::GraphStore;

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

/// What to supply instead of a brand/site-level key. Appended to every
/// affiliation-only refusal so the caller can tell the user what *would* work,
/// and so the cheap fixes (a page URL the user already has) are named before
/// anything reaches a paid hub.
const STRONGER_IDENTIFIER_HINT: &str = "Supply an identifier for the individual thing: a \
     location-specific page URL with a path (https://example.com/hayes-valley), a Yelp \
     /biz/ link, or a Google Maps place URL (https://www.google.com/maps/place/?q=place_id:...).";

/// Knobs on a commit. Constructed by the caller so a new policy question is a
/// new field here rather than a new `commit_record_*` overload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommitOpts {
    /// Whether a record whose *every* strong key is [`Grain::Affiliation`] (a
    /// bare brand/site domain and nothing else) may mint or attach.
    ///
    /// `true` — today's behavior, and what bulk ingest needs: the seed corpus is
    /// full of records identified only by their own domain, and refusing those
    /// would empty the graph. `false` — the publish path, where a chain's brand
    /// domain must not silently stand in for one of its locations; the commit is
    /// refused with candidates or a hint instead. See [`commit_record_with_opts`].
    pub allow_affiliation_only: bool,
}

impl Default for CommitOpts {
    /// The permissive default, matching [`commit_record`] / ingest. Only the
    /// publish path opts into the stricter grain rule.
    fn default() -> Self {
        CommitOpts {
            allow_affiliation_only: true,
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
    /// Optional human-readable guidance (e.g. what to do on a local name miss).
    /// Distinct from `matched_via`, which is strictly identifier-kind tags.
    pub hint: Option<String>,
    /// Per-member edge provenance: `(key, source)`, e.g.
    /// `("wikidata:Q83495", Some("wikidata"))`.
    pub provenance: Vec<(String, Option<String>)>,
    /// A **non-fatal** external-hub failure that degraded this answer.
    ///
    /// Hub calls are best-effort by contract: a hub that is down must not fail the
    /// whole resolution, because everything the local graph already knows is still
    /// a valid answer. But `.unwrap_or_default()` made "the hub 403'd" and "the hub
    /// genuinely knows of nothing" the *same* observation — an empty candidate list
    /// — so a broken key or a blocked egress IP presented as a confident
    /// `needs_stronger_identifier` verdict, and the only way to tell them apart was
    /// to replay the request by hand. This field is the difference.
    ///
    /// **A string, not an enum.** `read_json` in [`crate::transport`] already
    /// classifies the failure (auth / not-found / rate-limited / decode / other)
    /// with the method, the redacted URL and a body snippet; re-encoding that as a
    /// Rust enum would mean parsing our own message back into a taxonomy, i.e. a
    /// second source of truth that can silently disagree with the transport's. It
    /// is also the shape the consumer needs: agent-web's `sanitizeContext` keeps
    /// scalars and *drops* nested objects, so a struct would arrive as nothing at
    /// all. The string is pre-redacted at the transport (see
    /// [`crate::transport::redact_url`]) — never interpolate a raw hub URL here.
    ///
    /// Set is not the same as failed-overall: a resolution can succeed on a
    /// Placekey while the Text Search errored. Read it as "this answer was
    /// computed with less than the full evidence", and pair it with `status`.
    pub hub_error: Option<String>,
}

/// Anything that can produce a record to resolve.
///
/// Async because the hub adapters in [`crate::hubs`] implement it over
/// [`crate::transport::HttpTransport`], which is async so it can be backed by
/// `worker::Fetch` inside a Cloudflare Worker. `?Send` for the same reason the
/// transport and [`crate::store::GraphStore`] are: `worker::Fetch` futures hold
/// `JsValue`s. The offline implementations below have no `.await` in them and
/// never yield.
#[async_trait(?Send)]
pub trait Resolver {
    async fn harvest(&self) -> Result<EntityRecord>;
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

#[async_trait(?Send)]
impl Resolver for DirectRecordResolver {
    async fn harvest(&self) -> Result<EntityRecord> {
        Ok(self.record.clone())
    }
}

/// Commit a harvested record into the graph and return the completed entity.
///
/// This is the heart of "resolve = complete": whatever identifier came in, the
/// returned `same_as` is the whole cluster from the local graph. Edges written
/// here carry the provenance `"input"`; use [`commit_record_with_source`] to
/// attribute edges to a specific hub.
pub async fn commit_record(graph: &dyn GraphStore, record: &EntityRecord) -> Result<ResolveOutput> {
    commit_record_with_source(graph, record, "input").await
}

/// Like [`commit_record`], but tags every edge it writes with `source`
/// (e.g. `"wikidata"`, `"google_places"`) for edge provenance.
///
/// Keeps the permissive [`CommitOpts::default`] grain policy, so `/ingest`, the
/// CLI and hub completion behave exactly as before.
pub async fn commit_record_with_source(
    graph: &dyn GraphStore,
    record: &EntityRecord,
    source: &str,
) -> Result<ResolveOutput> {
    commit_record_with_opts(graph, record, source, CommitOpts::default()).await
}

/// [`commit_record_with_source`] plus an explicit policy ([`CommitOpts`]).
///
/// The one policy today is `allow_affiliation_only`. With it `false`, a record
/// whose only strong keys are [`Grain::Affiliation`] — a bare brand/site domain —
/// is **refused** rather than resolved, because such a key may name a chain, a
/// studio or a brand rather than the one thing the caller meant. Three outcomes,
/// all [`Status::Unresolved`] and all writing nothing:
///
/// | Graph state | `confidence_reason` | `candidates` |
/// |---|---|---|
/// | affiliation cluster(s) with identity keys | [`ConfidenceReason::AmbiguousAmongN`] | the identity-bearing entities |
/// | affiliation cluster(s), none identity-bearing | [`ConfidenceReason::NeedsStrongerIdentifier`] | empty |
/// | no cluster at all | [`ConfidenceReason::NeedsStrongerIdentifier`] | empty |
///
/// `AmbiguousAmongN(n)` is emitted **only** with `n == candidates.len() >= 1`;
/// an ambiguous verdict never carries an empty candidate list. The two
/// `NeedsStrongerIdentifier` cases are the caller's cue to fall through to a name
/// search (that orchestration lives in the caller, not here); `hint` distinguishes
/// them for humans.
///
/// A bare domain is refused only when it is the *sole* strong grain: a co-present
/// Identity key (a Michelin deep link, a Yelp `/biz/` slug, a place id) resolves
/// normally and carries the domain along with it.
pub async fn commit_record_with_opts(
    graph: &dyn GraphStore,
    record: &EntityRecord,
    source: &str,
    opts: CommitOpts,
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
        if let Some(canon) = graph.find(&id.key()).await? {
            identity_hits.push(canon);
        }
    }
    dedup(&mut identity_hits);
    let mut affil_hits: Vec<String> = Vec::new();
    for id in &affiliation_ids {
        if let Some(canon) = graph.find(&id.key()).await? {
            affil_hits.push(canon);
        }
    }
    dedup(&mut affil_hits);

    // 2. Refuse if the record has NO strong key at all (only phone / name /
    //    empty). We never mint or attach on the strength of a phone alone.
    if strong_ids.is_empty() {
        let mut phone_canons: Vec<String> = Vec::new();
        for id in &phone_ids {
            phone_canons.extend(graph.find_phone(&id.key()).await?);
        }
        dedup(&mut phone_canons);
        let mut candidates: Vec<Candidate> = Vec::new();
        for c in &phone_canons {
            if let Some(e) = graph.get_entity(c).await? {
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
            hub_error: None,
            hint: None,
        });
    }

    // 2b. Refuse when EVERY strong key is Affiliation grain (a brand/site domain
    //     and nothing else). `strong_ids` is non-empty here, so this is exactly
    //     "no Identity-grain key to go on". Until now Grain::Affiliation only had
    //     defensive consequences (don't steal a shared domain in step 4, don't
    //     merge identity-conflicting owners in step 3) — never interrogative
    //     ones, so `sameAs: ["https://souvla.com"]` on a review of one location of
    //     a chain sailed straight through and minted a brand-level entity.
    //
    //     Opt-in, because bulk ingest legitimately loads domain-only records.
    //     Nothing is written on any of these paths: the gate sits ahead of every
    //     mint / merge / attach below.
    if !opts.allow_affiliation_only && affiliation_ids.len() == strong_ids.len() {
        // One candidate per distinct entity the affiliation key(s) reach — not
        // one per member key, which would list the same entity once per identity
        // key it holds. Identity-less clusters are excluded: a brand org whose
        // only key is the shared domain answers no better than the domain did.
        // `EntityRow::anchor` is Identity-preferring (see `anchor::entity_anchor`),
        // so candidates do not all collapse back onto the brand domain.
        let mut candidates: Vec<Candidate> = Vec::new();
        for canon in &affil_hits {
            if identity_keys(&graph.members(canon).await?).is_empty() {
                continue;
            }
            if let Some(e) = graph.get_entity(canon).await? {
                candidates.push(Candidate {
                    canonical_id: e.canonical_id,
                    anchor: e.anchor,
                    name: e.name,
                });
            }
        }

        let affil_keys: Vec<String> = affiliation_ids.iter().map(|id| id.key()).collect();
        let joined = affil_keys.join(", ");
        // `AmbiguousAmongN` is reachable only with at least one candidate, so an
        // ambiguous verdict never comes back with an empty list to choose from.
        let (reason, hint) = if candidates.is_empty() {
            let lead = if affil_hits.is_empty() {
                format!("{joined} is not in the graph, and it names a brand or site rather than one specific thing.")
            } else {
                format!("{joined} is in the graph, but only as a brand/site with no identifying key of its own.")
            };
            (
                ConfidenceReason::NeedsStrongerIdentifier,
                Some(format!("{lead} {STRONGER_IDENTIFIER_HINT}")),
            )
        } else {
            (
                ConfidenceReason::AmbiguousAmongN(candidates.len()),
                Some(format!(
                    "{joined} may be shared across several things. Pick one of the candidates, \
                     or: {STRONGER_IDENTIFIER_HINT}"
                )),
            )
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
            hub_error: None,
            hint,
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
        let winner = pick_winner(graph, &identity_hits).await?;
        for canon in &identity_hits {
            if canon != &winner {
                graph.merge_into(&winner, canon).await?;
            }
        }
        // The winner's membership grows as it absorbs each affiliation hit, and a
        // key absorbed from an earlier hit can conflict with a later one — so the
        // comparison must see the CURRENT membership, not a snapshot from before
        // the loop. Cache it and refresh only after a merge actually changes it
        // (one read per merge instead of one read per candidate).
        let mut winner_members = graph.members(&winner).await?;
        for canon in &affil_hits {
            if canon != &winner
                && !identity_conflict(&winner_members, &graph.members(canon).await?)
            {
                graph.merge_into(&winner, canon).await?;
                winner_members = graph.members(&winner).await?;
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
            let winner = pick_winner(graph, &affil_hits).await?;
            // Same as the identity-hits branch: re-read after each absorb so a key
            // gained from an earlier merge still blocks a later conflicting one.
            let mut winner_members = graph.members(&winner).await?;
            for canon in &affil_hits {
                if canon != &winner
                    && !identity_conflict(&winner_members, &graph.members(canon).await?)
                {
                    graph.merge_into(&winner, canon).await?;
                    winner_members = graph.members(&winner).await?;
                }
            }
            (winner, Status::Hit)
        } else {
            // We carry identity keys; only adopt an affiliation cluster that
            // already shares one of them, else this is a distinct thing.
            let mut target: Option<String> = None;
            for canon in &affil_hits {
                let c_identity = identity_keys(&graph.members(canon).await?);
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
                None => (mint_entity(graph, record).await?, Status::New),
            }
        }
    } else {
        // Strong keys, but none matched an existing entity → a new entity.
        (mint_entity(graph, record).await?, Status::New)
    };

    // 4. Attach every strong key to the target, except an incoming affiliation
    //    key currently owned by an entity that is a *distinct* thing (identity
    //    conflict) — don't steal a shared domain.
    for id in &strong_ids {
        let key = id.key();
        match graph.find(&key).await? {
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
                graph.attach_with_source(&key, &canonical_id, Some(source)).await?;
                new_edges += 1;
            }
            None => {
                graph.attach_with_source(&key, &canonical_id, Some(source)).await?;
                new_edges += 1;
            }
        }
    }

    // 5. Record phone edges (corroborators). Attaching to the target never
    //    merges anything — a phone may edge to multiple entities.
    for id in &phone_ids {
        let already = graph.find_phone(&id.key()).await?;
        if already.iter().any(|c| c == &canonical_id) {
            matched_via.push("phone (corroborating)".into());
        } else {
            graph.add_phone_edge_with_source(&id.key(), &canonical_id, Some(source)).await?;
            new_edges += 1;
            if !already.is_empty() {
                matched_via.push("phone (corroborating)".into());
            }
        }
    }

    // 6. Sharpen the anchor from the full membership (canonical id is fixed).
    let members = graph.members(&canonical_id).await?;
    let current = graph
        .get_entity(&canonical_id).await?
        .ok_or_else(|| anyhow!("entity {canonical_id} vanished mid-resolve"))?
        .anchor;
    let anchor = anchor::recompute_anchor(&members, &current);
    if anchor != current {
        graph.set_anchor(&canonical_id, &anchor).await?;
    }
    graph.enrich_entity(
        &canonical_id,
        record.entity_type.as_deref(),
        record.name.as_deref(),
    ).await?;

    let entity = graph
        .get_entity(&canonical_id).await?
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
    let provenance = graph.member_sources(&canonical_id).await?;

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
        hub_error: None,
        hint: None,
    })
}

/// Mint a fresh entity for `record`: public anchor if any, else a deterministic
/// synthetic anchor from the strongest strong key, else a local synthetic id.
async fn mint_entity(graph: &dyn GraphStore, record: &EntityRecord) -> Result<String> {
    let anchor = anchor::choose_anchor(&record.same_as);
    let cid = anchor::canonical_id_for(&anchor);
    graph.create_entity(
        &cid,
        &anchor,
        record.entity_type.as_deref(),
        record.name.as_deref(),
    ).await?;
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
pub async fn resolve_id(graph: &dyn GraphStore, id: ExternalId) -> Result<ResolveOutput> {
    let record = EntityRecord {
        same_as: vec![id],
        ..Default::default()
    };
    commit_record(graph, &record).await
}

/// Load an existing entity by canonical id (the `entity <id>` path).
pub async fn load_entity(graph: &dyn GraphStore, canonical_id: &str) -> Result<ResolveOutput> {
    let entity = graph
        .get_entity(canonical_id).await?
        .ok_or_else(|| anyhow!("no entity with canonical_id {canonical_id}"))?;
    let members = graph.members(canonical_id).await?;
    let provenance = graph.member_sources(canonical_id).await?;
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
        hub_error: None,
        hint: None,
    })
}

/// Pick the union winner among candidate canonicals: strongest anchor, ties
/// broken by canonical id for determinism.
async fn pick_winner(graph: &dyn GraphStore, canonicals: &[String]) -> Result<String> {
    let mut best: Option<(u8, String)> = None;
    for cid in canonicals {
        let anchor = graph
            .get_entity(cid).await?
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
///
/// Requires the `harvest` feature: parsing needs `scraper`, and `from_fixture`
/// needs `std::fs` — neither is usable in a Worker, where a domain is only ever a
/// plain graph key.
#[cfg(feature = "harvest")]
pub struct DomainResolver {
    domain: String,
    html: String,
}

#[cfg(feature = "harvest")]
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
    pub async fn from_live(domain: &str) -> Result<Self> {
        let reg = crate::normalize::registrable_domain(domain)?;
        let url = format!("https://{reg}/");
        let resp = reqwest::get(&url)
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| anyhow!("fetching {url}: {e}"))?;
        let body = resp
            .text()
            .await
            .map_err(|e| anyhow!("fetching {url}: {e}"))?;
        Ok(DomainResolver { domain: reg, html: body })
    }
}

#[cfg(feature = "harvest")]
#[async_trait(?Send)]
impl Resolver for DomainResolver {
    async fn harvest(&self) -> Result<EntityRecord> {
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
#[cfg(feature = "harvest")]
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

#[cfg(feature = "harvest")]
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
///
/// **Not** behind the `harvest` feature, despite living among the HTML-harvesting
/// helpers: it touches only the kind registry and `ExternalId`, never `scraper`.
/// The Worker build (`default-features = false, features = ["d1", "worker-fetch"]`)
/// needs it to turn a caller's raw `sameAs` URLs into `kind:value` keys, and pulling
/// `harvest` in for that would drag the whole `scraper` parser tree into wasm.
pub fn guess_id_from_url(raw: &str) -> Option<ExternalId> {
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

    // No dedicated kind recognized the host. Prefer the generic `url` kind, which accepts
    // only a path-bearing URL — that names one page, so it is an Identity key.
    //
    // Falling back to `domain` for *every* URL (as this used to) is actively wrong:
    // `guide.michelin.com/.../kan-kiin` becomes `domain:michelin.com`, a key every
    // Michelin-listed restaurant shares. With `Grain::Affiliation` and no conflicting
    // identity key on either side, `commit_record` adopts the cluster and unrelated
    // restaurants merge — measured at 162-into-1 on a real corpus, reported as a 0.95
    // `exact_strong_key` hit because a strong key is exactly what it was handed.
    //
    // `domain` remains the fallback for a **path-less** URL, where the host really does
    // name the thing (a business's own site) and Affiliation grain is the right semantics.
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return ExternalId::new("url", raw)
            .or_else(|_| ExternalId::domain(raw))
            .ok();
    }
    None
}

#[cfg(feature = "harvest")]
fn push_unique(ids: &mut Vec<ExternalId>, id: ExternalId) {
    if !ids.iter().any(|existing| existing == &id) {
        ids.push(id);
    }
}

#[cfg(feature = "harvest")]
fn meta_content(doc: &scraper::Html, selector: &str) -> Option<String> {
    let sel = scraper::Selector::parse(selector).ok()?;
    doc.select(&sel)
        .next()
        .and_then(|el| el.value().attr("content"))
        .map(|s| s.to_string())
}

#[cfg(feature = "harvest")]
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
    use crate::store::SqliteStore;

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

    #[tokio::test]
    async fn jsonld_sameas_extraction() {
        let r = DomainResolver::from_html("bluebottlecoffee.com", FIXTURE.to_string()).unwrap();
        let rec = r.harvest().await.unwrap();
        assert_eq!(rec.entity_type.as_deref(), Some("LocalBusiness"));
        assert_eq!(rec.name.as_deref(), Some("Blue Bottle Coffee"));
        let keys: Vec<String> = rec.same_as.iter().map(|i| i.key()).collect();
        assert!(keys.contains(&"domain:bluebottlecoffee.com".to_string()));
        assert!(keys.contains(&"wikidata:Q4926426".to_string()));
        assert!(keys.contains(&"phone:+15106533394".to_string()));
        // facebook is a social link and is skipped.
        assert!(!keys.iter().any(|k| k.contains("facebook")));
    }

    #[tokio::test]
    async fn guess_id_from_url_recognizes_yelp() {
        let id =
            guess_id_from_url("https://www.yelp.com/biz/blue-bottle-coffee-san-francisco").unwrap();
        assert_eq!(id.key(), "yelp:blue-bottle-coffee-san-francisco");
        // Non-biz yelp URLs and social links do not produce a yelp id.
        assert!(guess_id_from_url("https://www.facebook.com/x").is_none());
    }

    #[tokio::test]
    async fn yelp_harvested_from_jsonld_sameas() {
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
        let rec = r.harvest().await.unwrap();
        let keys: Vec<String> = rec.same_as.iter().map(|i| i.key()).collect();
        assert!(keys.contains(&"yelp:blue-bottle-coffee-san-francisco".to_string()));
    }

    #[tokio::test]
    async fn resolve_by_yelp_hits_same_entity() {
        let g = SqliteStore::open_in_memory().unwrap();
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
        ).await
        .unwrap();
        // Resolving by the generic yelp key lands on the same entity.
        let hit = resolve_id(
            &g,
            ExternalId::new("yelp", "blue-bottle-coffee-san-francisco").unwrap(),
        ).await
        .unwrap();
        assert_eq!(hit.canonical_id, out.canonical_id);
        assert_eq!(hit.status, Status::Hit);
        assert_eq!(hit.anchor, "wikidata:Q4926426");
    }

    #[tokio::test]
    async fn phone_alone_does_not_merge_distinct_entities() {
        let g = SqliteStore::open_in_memory().unwrap();

        // Entity 1: domain A + shared phone.
        let rec1 = EntityRecord {
            entity_type: Some("LocalBusiness".into()),
            name: Some("Cafe A".into()),
            same_as: vec![
                ExternalId::domain("a-cafe.com").unwrap(),
                ExternalId::phone("+1-510-653-3394").unwrap(),
            ],
        };
        let out1 = commit_record(&g, &rec1).await.unwrap();

        // Entity 2: DIFFERENT domain B + the SAME phone.
        let rec2 = EntityRecord {
            entity_type: Some("LocalBusiness".into()),
            name: Some("Cafe B".into()),
            same_as: vec![
                ExternalId::domain("b-cafe.com").unwrap(),
                ExternalId::phone("+1-510-653-3394").unwrap(),
            ],
        };
        let out2 = commit_record(&g, &rec2).await.unwrap();

        // They must remain DISTINCT despite sharing a phone.
        assert_ne!(out1.canonical_id, out2.canonical_id);
        assert_eq!(g.find("domain:a-cafe.com").await.unwrap(), out1.canonical_id.clone());
        assert_eq!(g.find("domain:b-cafe.com").await.unwrap(), out2.canonical_id.clone());
        // The phone corroborates both.
        let phone_canons = g.find_phone("phone:+15106533394").await.unwrap();
        assert_eq!(phone_canons.len(), 2);
    }

    #[tokio::test]
    async fn strong_keys_union_transitively() {
        let g = SqliteStore::open_in_memory().unwrap();
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
        ).await
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
        ).await
        .unwrap();
        // Now domain, place_id, wikidata all resolve to the same canonical.
        assert_eq!(g.find("domain:x.com").await.unwrap(), out.canonical_id.clone());
        assert_eq!(
            g.find("google_place_id:ChIJxyz").await.unwrap(),
            out.canonical_id.clone()
        );
        assert_eq!(out.same_as.len(), 3);
        assert_eq!(out.anchor, "wikidata:Q1");
    }

    // --- C1: an affiliation-only record must not merge distinct identities ---
    #[tokio::test]
    async fn affiliation_only_record_does_not_merge_distinct_identities() {
        let g = SqliteStore::open_in_memory().unwrap();

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
        ).await
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
        ).await
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
        ).await
        .unwrap();

        // The two IMDb identities must STILL resolve to different canonicals.
        let after_p = g.find("imdb:tt1111111").await.unwrap();
        let after_q = g.find("imdb:tt2222222").await.unwrap();
        assert!(after_p.is_some() && after_q.is_some());
        assert_ne!(
            after_p, after_q,
            "an affiliation-only record must not merge distinct-identity entities"
        );
        // Each domain stays with its own identity's owner (not stolen).
        assert_eq!(g.find("domain:p-studio.com").await.unwrap(), after_p);
        assert_eq!(g.find("domain:q-studio.com").await.unwrap(), after_q);
    }

    // --- H1: stealing a domain from an identity-less brand must not orphan it ---
    #[tokio::test]
    async fn store_does_not_orphan_identity_less_brand_owning_the_domain() {
        let g = SqliteStore::open_in_memory().unwrap();

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
        ).await
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
        ).await
        .unwrap();
        let store_id = store.canonical_id.clone().unwrap();
        assert_ne!(brand_id, store_id);

        // The domain must NOT be stolen: it stays with the brand.
        assert_eq!(g.find("domain:acme.com").await.unwrap().as_deref(), Some(brand_id.as_str()));
        assert_eq!(
            g.find("google_place_id:STORE1").await.unwrap().as_deref(),
            Some(store_id.as_str())
        );

        // The brand entity survives AND its anchor still names a key it owns
        // (no stale-anchor orphan).
        let brand_row = g.get_entity(&brand_id).await.unwrap().expect("brand must survive");
        assert_eq!(brand_row.anchor, "domain:acme.com");
        assert_eq!(
            g.find(&brand_row.anchor).await.unwrap().as_deref(),
            Some(brand_id.as_str()),
            "an entity's anchor must always name a key it actually owns"
        );
    }

    // --- guard against over-correction: legitimate affiliation attach still works ---
    #[tokio::test]
    async fn legitimate_domain_attaches_to_same_identity_entity() {
        let g = SqliteStore::open_in_memory().unwrap();

        // Seed an entity by its identity key alone.
        let seed = commit_record(
            &g,
            &EntityRecord {
                same_as: vec![ExternalId::wikidata("Q1").unwrap()],
                ..Default::default()
            },
        ).await
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
        ).await
        .unwrap();

        assert_eq!(out.canonical_id.as_deref(), Some(seed_id.as_str()));
        assert_eq!(out.status, Status::Hit);
        assert_eq!(g.find("domain:e.com").await.unwrap().as_deref(), Some(seed_id.as_str()));
        let keys: Vec<String> = out.same_as.iter().map(|i| i.key()).collect();
        assert!(keys.contains(&"wikidata:Q1".to_string()));
        assert!(keys.contains(&"domain:e.com".to_string()));
    }

    // ---------------------------------------------------------------------
    // Grain refusal: an affiliation-only record (a bare brand domain) must ask
    // "which one?" instead of minting a brand-level entity.
    // ---------------------------------------------------------------------

    /// The publish-path policy: a bare brand domain is not enough on its own.
    const STRICT: CommitOpts = CommitOpts {
        allow_affiliation_only: false,
    };

    /// The Souvla bug on a graph that already knows one location: refuse and hand
    /// back the location(s) the domain reaches, anchored on their identity keys.
    #[tokio::test]
    async fn affiliation_only_is_ambiguous_when_the_domain_reaches_an_identity() {
        let g = SqliteStore::open_in_memory().unwrap();

        // One known location: its own place id + the shared chain domain.
        let hayes = commit_record(
            &g,
            &EntityRecord {
                name: Some("Souvla Hayes Valley".into()),
                same_as: vec![
                    ExternalId::google_place_id("ChIJHAYES").unwrap(),
                    ExternalId::domain("souvla.com").unwrap(),
                ],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let hayes_id = hayes.canonical_id.clone().unwrap();

        // The bug report: a review carrying only the chain domain.
        let out = commit_record_with_opts(
            &g,
            &EntityRecord {
                name: Some("Souvla".into()),
                same_as: vec![ExternalId::domain("souvla.com").unwrap()],
                ..Default::default()
            },
            "input",
            STRICT,
        )
        .await
        .unwrap();

        assert_eq!(out.status, Status::Unresolved);
        assert_eq!(out.canonical_id, None);
        assert_eq!(out.confidence_reason, ConfidenceReason::AmbiguousAmongN(1));
        assert_eq!(out.candidates.len(), 1);
        assert_eq!(out.candidates[0].canonical_id, hayes_id);
        // anchor.rs: Identity grain beats Affiliation, so a candidate never
        // collapses back onto the brand domain that produced the ambiguity.
        assert_eq!(out.candidates[0].anchor, "google_place_id:ChIJHAYES");
        assert!(out.hint.is_some());
        // Nothing was written: no second (brand-level) entity was minted, and the
        // domain still belongs to the location that owned it.
        assert_eq!(out.new_edges, 0);
        assert_eq!(
            g.find("domain:souvla.com").await.unwrap().as_deref(),
            Some(hayes_id.as_str())
        );
    }

    /// `n` is the number of *distinct entities*, and always equals
    /// `candidates.len()` — an ambiguous verdict is never empty-handed.
    #[tokio::test]
    async fn affiliation_only_lists_every_distinct_owner() {
        let g = SqliteStore::open_in_memory().unwrap();
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
        .await
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
        .await
        .unwrap();

        let out = commit_record_with_opts(
            &g,
            &EntityRecord {
                same_as: vec![
                    ExternalId::domain("p-studio.com").unwrap(),
                    ExternalId::domain("q-studio.com").unwrap(),
                ],
                ..Default::default()
            },
            "input",
            STRICT,
        )
        .await
        .unwrap();

        assert_eq!(out.status, Status::Unresolved);
        assert_eq!(out.confidence_reason, ConfidenceReason::AmbiguousAmongN(2));
        let ids: HashSet<String> = out
            .candidates
            .iter()
            .map(|c| c.canonical_id.clone())
            .collect();
        assert_eq!(
            ids,
            HashSet::from([p.canonical_id.unwrap(), q.canonical_id.unwrap()])
        );
    }

    /// The cluster exists but holds no identity key (the winner absorbed only
    /// identity-less hits). There is nothing to choose between, so this must NOT
    /// come back as "ambiguous" with an empty list — the caller has to be able to
    /// tell it apart and fall through to a name search.
    #[tokio::test]
    async fn affiliation_only_with_identity_less_cluster_signals_fallthrough() {
        let g = SqliteStore::open_in_memory().unwrap();

        // A brand org: domain + phone, no identity key at all.
        let brand = commit_record(
            &g,
            &EntityRecord {
                name: Some("Souvla".into()),
                same_as: vec![
                    ExternalId::domain("souvla.com").unwrap(),
                    ExternalId::phone("+1-415-555-0100").unwrap(),
                ],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(brand.anchor, "domain:souvla.com");

        let out = commit_record_with_opts(
            &g,
            &EntityRecord {
                same_as: vec![ExternalId::domain("souvla.com").unwrap()],
                ..Default::default()
            },
            "input",
            STRICT,
        )
        .await
        .unwrap();

        assert_eq!(out.status, Status::Unresolved);
        assert_eq!(
            out.confidence_reason,
            ConfidenceReason::NeedsStrongerIdentifier,
            "a zero-candidate refusal must not masquerade as ambiguity"
        );
        assert!(out.candidates.is_empty());
        assert!(out.hint.unwrap().contains("souvla.com"));
    }

    /// Souvla on an empty graph: nothing to offer, so name what would work.
    #[tokio::test]
    async fn affiliation_only_with_no_cluster_needs_a_stronger_identifier() {
        let g = SqliteStore::open_in_memory().unwrap();

        let out = commit_record_with_opts(
            &g,
            &EntityRecord {
                entity_type: Some("Restaurant".into()),
                name: Some("Souvla".into()),
                same_as: vec![ExternalId::domain("souvla.com").unwrap()],
            },
            "input",
            STRICT,
        )
        .await
        .unwrap();

        assert_eq!(out.status, Status::Unresolved);
        assert_eq!(out.canonical_id, None);
        assert_eq!(
            out.confidence_reason,
            ConfidenceReason::NeedsStrongerIdentifier
        );
        assert!(out.candidates.is_empty());
        // The input is echoed back so the caller can still run a name search.
        assert_eq!(out.name.as_deref(), Some("Souvla"));
        assert_eq!(out.entity_type.as_deref(), Some("Restaurant"));
        let hint = out
            .hint
            .expect("a refusal with no candidates must say what to supply");
        assert!(
            hint.contains("/biz/"),
            "hint should name the Yelp escape hatch: {hint}"
        );
        assert!(
            hint.contains("place_id"),
            "hint should name the Maps escape hatch: {hint}"
        );
        // Refused means refused: no entity, no edge.
        assert!(g.find("domain:souvla.com").await.unwrap().is_none());
        assert_eq!(out.new_edges, 0);
    }

    /// The asymmetry that keeps the rule safe for single-location businesses: a
    /// bare domain is refused only as the SOLE strong grain. Zuni Café carries
    /// both its own site and a Michelin deep link, and resolves on the latter.
    #[tokio::test]
    async fn a_co_present_identity_key_rescues_a_bare_domain() {
        let g = SqliteStore::open_in_memory().unwrap();

        let out = commit_record_with_opts(
            &g,
            &EntityRecord {
                name: Some("Zuni Café".into()),
                same_as: vec![
                    ExternalId::domain("zunicafe.com").unwrap(),
                    ExternalId::new(
                        "url",
                        "https://guide.michelin.com/us/en/california/san-francisco/restaurant/zuni-cafe",
                    )
                    .unwrap(),
                ],
                ..Default::default()
            },
            "input",
            STRICT,
        )
        .await
        .unwrap();

        assert_eq!(out.status, Status::New);
        assert!(out.canonical_id.is_some());
        // The domain rides along on the identity key that carried the record.
        let keys: HashSet<String> = out.same_as.iter().map(|i| i.key()).collect();
        assert!(keys.contains("domain:zunicafe.com"));
        assert!(out.anchor.starts_with("url:guide.michelin.com/"));
    }

    /// Regression for the 151 bare-origin domains in the agent-web seed corpus:
    /// `/ingest`, the CLI and hub completion all go through the permissive
    /// entry points, and must keep minting domain-only entities.
    #[tokio::test]
    async fn domain_only_ingest_still_succeeds() {
        let g = SqliteStore::open_in_memory().unwrap();

        for (i, domain) in ["zunicafe.com", "flourandwater.com", "souvla.com"]
            .iter()
            .enumerate()
        {
            let record = EntityRecord {
                entity_type: Some("Restaurant".into()),
                name: Some(format!("Seed {i}")),
                same_as: vec![ExternalId::domain(domain).unwrap()],
            };

            // The default entry point.
            let out = commit_record(&g, &record).await.unwrap();
            assert_eq!(out.status, Status::New, "{domain} must still mint");
            assert_eq!(out.anchor, format!("domain:{domain}"));
            assert!(out.candidates.is_empty());
            assert!(out.hint.is_none());
            let cid = out.canonical_id.clone().unwrap();
            assert_eq!(
                g.find(&format!("domain:{domain}"))
                    .await
                    .unwrap()
                    .as_deref(),
                Some(cid.as_str())
            );

            // The source-tagged entry point re-resolves to the same entity.
            let again = commit_record_with_source(&g, &record, "seed")
                .await
                .unwrap();
            assert_eq!(again.status, Status::Hit);
            assert_eq!(again.canonical_id.as_deref(), Some(cid.as_str()));

            // And so does an explicit permissive CommitOpts.
            let opted = commit_record_with_opts(&g, &record, "seed", CommitOpts::default())
                .await
                .unwrap();
            assert_eq!(opted.status, Status::Hit);
            assert_eq!(opted.canonical_id.as_deref(), Some(cid.as_str()));
        }
    }

    /// The gate is grain-shaped, not domain-shaped: a phone-only record still
    /// takes the older no-strong-key refusal, unchanged by the new branch.
    #[tokio::test]
    async fn strict_opts_leave_the_no_strong_key_refusal_alone() {
        let g = SqliteStore::open_in_memory().unwrap();
        let out = commit_record_with_opts(
            &g,
            &EntityRecord {
                name: Some("Nameless".into()),
                same_as: vec![ExternalId::phone("+1-415-555-0199").unwrap()],
                ..Default::default()
            },
            "input",
            STRICT,
        )
        .await
        .unwrap();
        assert_eq!(out.status, Status::Unresolved);
        assert_eq!(
            out.confidence_reason,
            ConfidenceReason::NeedsStrongerIdentifier
        );
        assert!(
            out.hint.is_none(),
            "the phone path keeps its existing shape"
        );
    }
}
