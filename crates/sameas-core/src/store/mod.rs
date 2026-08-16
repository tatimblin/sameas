//! The storage boundary: [`GraphStore`], the trait every crosswalk-graph backend
//! implements, plus the row types that cross it.
//!
//! The graph itself is a **union-find over typed external-id keys**. We store only
//! ID-to-ID edges plus a minimal canonical anchor — never provider content.
//! Logical layout (identical in every backend):
//! * `nodes(key, canonical_id)` — strong keys. `key` is unique, so a strong
//!   identifier belongs to exactly one entity; union re-points losers to the
//!   winner. This *is* the union-find.
//! * `phone_edges(phone_key, canonical_id)` — phone is a **corroborator only**, so
//!   it lives outside the union-find and one phone may edge to several entities
//!   without merging them.
//! * `entities(canonical_id, anchor, entity_type, name)` — anchor + light display
//!   metadata.
//! * `name_index` / `name_cardinality` — the local name-resolution memory.
//! * `resolutions` — the append-only miss-rate log.
//!
//! Two backends live behind the trait:
//! * [`SqliteStore`] — `rusqlite`, used by the CLI and every unit test. Owns
//!   schema creation and migration.
//! * `d1::D1Store` — Cloudflare D1, for the WASM Worker build. Reachable as
//!   `sameas_core::store::d1::D1Store` (feature-gated, like `ReqwestTransport`).
//!
//! **Why the trait is async.** `rusqlite`'s `bundled` feature compiles C SQLite and
//! cannot target `wasm32-unknown-unknown`, so the Worker needs D1 — and D1 is
//! async-only. `SqliteStore`'s methods are therefore `async fn` wrapping
//! synchronous `rusqlite` calls: they contain no `.await` and never yield.
//!
//! **Why `?Send`.** `worker::D1Database` is `!Send`. Workers are single-threaded,
//! so a `Send` bound would buy nothing and cost `SendWrapper` gymnastics at every
//! call. The CLI drives this with a current-thread runtime.

use anyhow::Result;
use async_trait::async_trait;

use crate::model::ExternalId;

pub mod conformance;
#[cfg(feature = "d1")]
pub mod d1;
#[cfg(feature = "d1")]
mod d1_codec;
#[cfg(feature = "sqlite")]
pub mod sqlite;

#[cfg(feature = "sqlite")]
pub use sqlite::SqliteStore;

/// One row of an entity's membership: a key plus the provenance of that edge.
/// Covers both strong `nodes` keys and `phone_edges` corroborators — the two are
/// read together and separated by kind afterwards.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberRow {
    pub key: String,
    pub source: Option<String>,
}

/// A stored ambiguity candidate: `(canonical_id, anchor, name)`. `canonical_id` is
/// usually empty (these are un-committed candidates surfaced by a hub search).
pub type NameCandidate = (String, String, Option<String>);

/// What a hub text-search revealed about the uniqueness of a `(name, Q)` query, as
/// remembered locally. `Unique` carries the resolved `canonical_id` (so a later
/// coarse repeat can hit locally with zero external calls); `Ambiguous` carries the
/// candidate list surfaced when the hub returned more than one.
#[derive(Clone, Debug)]
pub enum NameCardinality {
    Unique(String),
    Ambiguous(Vec<NameCandidate>),
}

/// A stored entity row.
#[derive(Clone, Debug)]
pub struct EntityRow {
    pub canonical_id: String,
    pub anchor: String,
    pub entity_type: Option<String>,
    pub name: Option<String>,
}

/// The three outcome buckets for the miss-rate report.
// Only a backend's `stats()` reaches these, so a build with no backend feature
// enabled (`--no-default-features`) legitimately never calls them.
#[cfg_attr(not(any(feature = "sqlite", feature = "d1")), allow(dead_code))]
pub(crate) enum Bucket {
    Exact,
    Hub,
    Miss,
}

/// Map a confidence `reason_tag` to its miss-rate bucket. Unknown tags (should not
/// occur) count as a miss — conservative: an unclassified outcome is not silently
/// credited as a hit.
///
/// **Exact** = answered purely from the supplied exact key (in-graph hit, or a
/// new/synthetic entity minted directly from that strong key — no external call was
/// needed). **Hub** = required reaching beyond the supplied key: a hub crosswalk, or
/// a name/address reverse-resolution (including a repeat served from the local name
/// index). **Miss** = unresolved / too weak to resolve.
#[cfg_attr(not(any(feature = "sqlite", feature = "d1")), allow(dead_code))]
pub(crate) fn reason_bucket(tag: &str) -> Bucket {
    match tag {
        "direct_lookup" | "exact_strong_key" | "new_public_anchor" | "synthetic_strong_key" => {
            Bucket::Exact
        }
        "hub_crosswalk" | "placekey_address" | "place_unique_match" | "local_name_match"
        | "placekey_city_only" => Bucket::Hub,
        // needs_stronger_identifier, ambiguous_among_n, phone_only, and anything
        // unrecognized.
        _ => Bucket::Miss,
    }
}

/// The aggregated miss-rate report backing `sameas stats`.
#[derive(Clone, Debug)]
pub struct StatsReport {
    /// Total logged resolutions.
    pub total: usize,
    /// Answered from an exact key (`direct_lookup`, `exact_strong_key`).
    pub exact: usize,
    /// Required a hub lookup / bootstrap.
    pub hub: usize,
    /// Unresolved — the miss set (the evidence gate for a future fuzzy phase).
    pub miss: usize,
    /// Per-reason counts, most frequent first.
    pub by_reason: Vec<(String, usize)>,
    /// Distinct entities currently in the graph.
    pub entities: usize,
    /// Total edges (strong nodes + phone corroborators).
    pub edges: usize,
}

impl StatsReport {
    /// Miss rate in `0.0`–`1.0` (0.0 when nothing has been logged yet).
    pub fn miss_rate(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.miss as f64 / self.total as f64
        }
    }

    /// Assemble the report from the raw per-reason counts plus the two totals.
    /// Shared by every backend so bucketing and ordering can't drift between them.
    #[cfg_attr(not(any(feature = "sqlite", feature = "d1")), allow(dead_code))]
    pub(crate) fn from_counts(
        mut by_reason: Vec<(String, usize)>,
        entities: usize,
        edges: usize,
    ) -> StatsReport {
        // Most frequent first; ties broken alphabetically for determinism.
        by_reason.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        let mut exact = 0;
        let mut hub = 0;
        let mut miss = 0;
        for (tag, n) in &by_reason {
            match reason_bucket(tag) {
                Bucket::Exact => exact += n,
                Bucket::Hub => hub += n,
                Bucket::Miss => miss += n,
            }
        }
        StatsReport {
            total: exact + hub + miss,
            exact,
            hub,
            miss,
            by_reason,
            entities,
            edges,
        }
    }
}

/// A stable, canonical serialization of a normalized qualifier set for use as a
/// table key. Callers pass an already-normalized, sorted, deduped set
/// (`NameQuery::establishing_qualifiers`); we join with `\n` (never a token char) so
/// distinct sets never collide.
#[cfg_attr(not(any(feature = "sqlite", feature = "d1")), allow(dead_code))]
pub(crate) fn qualifier_set_key(qualifiers: &[String]) -> String {
    qualifiers.join("\n")
}

/// Turn raw member keys into typed identifiers, sorted and deduped for stable
/// output. Shared by every backend and by the derived `members` view so ordering is
/// identical everywhere.
pub(crate) fn typed_members<'a>(keys: impl Iterator<Item = &'a str>) -> Vec<ExternalId> {
    let mut ids: Vec<ExternalId> = keys.filter_map(ExternalId::from_key).collect();
    ids.sort_by(|a, b| {
        a.kind_tag()
            .cmp(b.kind_tag())
            .then_with(|| a.value().cmp(b.value()))
    });
    ids.dedup();
    ids
}

/// The crosswalk graph's storage boundary.
///
/// Object-safe (no generics, no associated types) so it is usable as
/// `&dyn GraphStore` — the shape every orchestration function in `resolve`,
/// `complete`, and `correct` takes. Mirrors [`crate::transport::HttpTransport`]:
/// backend-neutral types only, and richer capabilities land as **provided methods**
/// the simpler backend can ignore.
///
/// Construction is deliberately off the trait (constructors don't work usefully as
/// trait methods): use `SqliteStore::open`/`open_in_memory` or `D1Store::new`.
#[async_trait(?Send)]
pub trait GraphStore {
    // --- union-find over strong keys ------------------------------------

    /// Find the canonical id a strong key currently belongs to.
    async fn find(&self, key: &str) -> Result<Option<String>>;

    /// Attach (or re-point) a strong key, recording where the edge came from. A
    /// `None` source leaves the provenance NULL; a re-point updates it only when a
    /// source is supplied (so a later, better-attributed write can fill it in
    /// without a plain re-point clobbering it to NULL).
    async fn attach_with_source(
        &self,
        key: &str,
        canonical_id: &str,
        source: Option<&str>,
    ) -> Result<()>;

    /// Attach (or re-point) a strong key to a canonical id.
    async fn attach(&self, key: &str, canonical_id: &str) -> Result<()> {
        self.attach_with_source(key, canonical_id, None).await
    }

    /// Look up many strong keys at once, returning `(key, canonical_id)` pairs in
    /// the order given.
    ///
    /// The default loops [`find`](Self::find), which is right for a local SQLite
    /// file. A network-backed store should override this with one chunked
    /// `IN (...)` query — resolving a record probes every strong key it carries, so
    /// this is the largest term in the round-trip budget.
    async fn find_many(&self, keys: &[String]) -> Result<Vec<(String, Option<String>)>> {
        let mut out = Vec::with_capacity(keys.len());
        for k in keys {
            out.push((k.clone(), self.find(k).await?));
        }
        Ok(out)
    }

    // --- phone edges (corroborator, outside union-find) -----------------

    /// Record a phone corroborator edge with provenance.
    async fn add_phone_edge_with_source(
        &self,
        phone_key: &str,
        canonical_id: &str,
        source: Option<&str>,
    ) -> Result<()>;

    async fn add_phone_edge(&self, phone_key: &str, canonical_id: &str) -> Result<()> {
        self.add_phone_edge_with_source(phone_key, canonical_id, None)
            .await
    }

    /// All canonical ids a phone corroborates (may be more than one; that does not
    /// mean they are the same entity).
    async fn find_phone(&self, phone_key: &str) -> Result<Vec<String>>;

    // --- entities -------------------------------------------------------

    async fn create_entity(
        &self,
        canonical_id: &str,
        anchor: &str,
        entity_type: Option<&str>,
        name: Option<&str>,
    ) -> Result<()>;

    async fn get_entity(&self, canonical_id: &str) -> Result<Option<EntityRow>>;

    async fn set_anchor(&self, canonical_id: &str, anchor: &str) -> Result<()>;

    /// Fill in type/name only when currently empty (never clobber existing).
    async fn enrich_entity(
        &self,
        canonical_id: &str,
        entity_type: Option<&str>,
        name: Option<&str>,
    ) -> Result<()>;

    /// Count strong-key members of a canonical id (used to guard against orphaning
    /// an entity by splitting away all its strong keys).
    async fn strong_key_count(&self, canonical_id: &str) -> Result<usize>;

    // --- members --------------------------------------------------------

    /// Every member edge of an entity — strong nodes *and* phone corroborators —
    /// with provenance, in unspecified order.
    ///
    /// This is the single membership primitive; the three views below derive from it
    /// in Rust. Backends implement one read instead of three, and a network-backed
    /// store makes one round trip where the old shape made two per view.
    async fn member_rows(&self, canonical_id: &str) -> Result<Vec<MemberRow>>;

    /// All member keys (strong nodes + phone edges) of a canonical entity.
    async fn member_keys(&self, canonical_id: &str) -> Result<Vec<String>> {
        Ok(self
            .member_rows(canonical_id)
            .await?
            .into_iter()
            .map(|r| r.key)
            .collect())
    }

    /// All member keys with their edge provenance, sorted by key for stable output.
    async fn member_sources(&self, canonical_id: &str) -> Result<Vec<(String, Option<String>)>> {
        let mut rows: Vec<(String, Option<String>)> = self
            .member_rows(canonical_id)
            .await?
            .into_iter()
            .map(|r| (r.key, r.source))
            .collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        rows.dedup();
        Ok(rows)
    }

    /// Members as typed identifiers, sorted for stable output.
    async fn members(&self, canonical_id: &str) -> Result<Vec<ExternalId>> {
        let rows = self.member_rows(canonical_id).await?;
        Ok(typed_members(rows.iter().map(|r| r.key.as_str())))
    }

    // --- local name index (resolve name + qualifiers offline) -----------

    /// Index an entity under a normalized name and a set of normalized qualifier
    /// tokens (city / state / borough / year / …). Writes one row per qualifier,
    /// plus a `qualifier = ""` row so a bare-name query can still match. Both name
    /// and qualifiers are expected already normalized (via `normalize::name_key`).
    async fn index_name(
        &self,
        name_norm: &str,
        qualifiers: &[String],
        canonical_id: &str,
        source: Option<&str>,
    ) -> Result<()>;

    /// Every entity indexed under `name_norm`, paired with the non-empty qualifier
    /// tokens it was established under (the union of all facets ever indexed for it
    /// — its "establishing set"). The always-present `""` bare row is excluded. Used
    /// by the specificity-monotonic local matcher: an entity is a confident hit only
    /// for a query whose token set is a superset of its establishing set. Sorted by
    /// canonical id (tokens sorted) for determinism.
    async fn name_entities(&self, name_norm: &str) -> Result<Vec<(String, Vec<String>)>>;

    /// Record that (name, Q) is ambiguous among `candidates` (hub returned >1).
    /// Overwrites any prior row for (name, Q) — including a stale `unique` row —
    /// clearing its `canonical_id`, so the newest hub truth wins.
    async fn record_name_cardinality(
        &self,
        name_norm: &str,
        qualifiers: &[String],
        candidates: &[NameCandidate],
    ) -> Result<()>;

    /// Record that (name, Q) resolved UNIQUELY to `canonical_id` (hub returned
    /// exactly one). Overwrites any prior row for (name, Q) — including a stale
    /// `ambiguous` row — so the newest hub truth wins.
    async fn record_name_unique(
        &self,
        name_norm: &str,
        qualifiers: &[String],
        canonical_id: &str,
    ) -> Result<()>;

    /// The stored cardinality row for (name, Q), if any — an EXACT qualifier-set
    /// match (specificity-preserving: a coarse fact never answers a finer query).
    /// **No liveness check**; see [`name_cardinality`](Self::name_cardinality).
    async fn name_cardinality_raw(
        &self,
        name_norm: &str,
        qualifiers: &[String],
    ) -> Result<Option<NameCardinality>>;

    /// The stored cardinality for (name, Q), with a liveness check: a `unique` row
    /// whose `canonical_id` no longer resolves to a live entity reads as a miss
    /// (`None`), so a merged/deleted id is never served.
    ///
    /// Layered above the raw read so the liveness rule lives in exactly one place
    /// rather than being re-implemented (and possibly diverging) per backend.
    async fn name_cardinality(
        &self,
        name_norm: &str,
        qualifiers: &[String],
    ) -> Result<Option<NameCardinality>> {
        match self.name_cardinality_raw(name_norm, qualifiers).await? {
            Some(NameCardinality::Unique(cid)) => {
                // Malformed unique (empty id) or a stale id pointing at a
                // merged/deleted entity → a miss, not a dead hit.
                if cid.is_empty() || self.get_entity(&cid).await?.is_none() {
                    Ok(None)
                } else {
                    Ok(Some(NameCardinality::Unique(cid)))
                }
            }
            other => Ok(other),
        }
    }

    // --- union / split (atomic multi-statement ops) ----------------------

    /// Merge `loser` into `winner`: re-point all strong nodes, phone edges and
    /// cached name rows, then drop the loser entity row. Strong keys drive this;
    /// callers must never invoke it on the strength of a phone alone.
    ///
    /// **Must be atomic.** A half-applied merge leaves nodes pointing at a deleted
    /// canonical id — a corrupted union-find, which is the exact false-merge class
    /// this project treats as its primary invariant.
    async fn merge_into(&self, winner: &str, loser: &str) -> Result<()>;

    /// Apply a split atomically: mint the new entity, move `detached_keys` onto it,
    /// invalidate the source's stale name caches, and re-anchor BOTH sides. The
    /// correction op (`correct::split`) owns the policy (which keys, the orphan
    /// guard, `new_cid` disambiguation, choosing `new_anchor`); this owns the
    /// mutation so it cannot half-apply.
    async fn apply_split(
        &self,
        new_cid: &str,
        new_anchor: &str,
        detached_keys: &[String],
        src_cid: &str,
    ) -> Result<()>;

    /// Move a single strong node key to a different canonical id, preserving its
    /// existing provenance. `nodes.key` is unique, so this re-points exactly one
    /// row. (A thin, intent-named wrapper over the attach upsert.)
    async fn repoint_key(&self, key: &str, to_canonical_id: &str) -> Result<()> {
        self.attach_with_source(key, to_canonical_id, None).await
    }

    // --- stats (miss-rate instrumentation) ------------------------------

    /// Append one resolution outcome to the log. Best-effort at the call site: a
    /// logging failure must never fail the resolution itself.
    ///
    /// **Log user-facing *queries* only.** A direct id lookup (`entity`) or a bulk
    /// load (`ingest`) is not a query, and counting them would skew the miss rate
    /// this log exists to measure — and that miss rate is the documented evidence
    /// gate for ever adding a fuzzy-matching layer. Both front-ends call this from
    /// exactly one place: the CLI's `Resolve` arm and the Worker's `/resolve`.
    async fn record_resolution(
        &self,
        status_tag: &str,
        reason_tag: &str,
        matched_via: Option<&str>,
        confidence: f32,
        input_desc: Option<&str>,
    ) -> Result<()>;

    /// Aggregate the resolution log into the miss-rate report.
    async fn stats(&self) -> Result<StatsReport>;
}
