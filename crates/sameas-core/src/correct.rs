//! Correction operations: `link`, `merge`, `split`.
//!
//! The resolver is deliberately conservative (false-merge safety is the primary
//! invariant), so it leaves two error modes it cannot self-correct:
//!
//! * **misses** — two clusters that are really one entity, which the resolver
//!   refused to union because no key bridged them. `link`/`merge` join them.
//! * **false merges** — two distinct entities the gated union wrongly combined
//!   (e.g. two chain locations that shared a forwarding phone). `split` is the
//!   only way to undo one.
//!
//! These are the human-in-the-loop repair verbs. The *policy* lives here
//! (identity-conflict guards, winner selection, re-anchoring); the low-level
//! SQL primitives live behind [`GraphStore`]. Every op re-selects the affected entities'
//! anchors — the graph primitives move edges but never re-anchor.

use anyhow::{bail, Result};

use crate::anchor;
use crate::kind::Grain;
use crate::model::ExternalId;
use crate::store::GraphStore;

/// What a [`link`] did — reported back so the caller can explain the outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkOutcome {
    /// Neither key was known; a fresh entity was created holding both.
    Created(String),
    /// One key was known; the other was attached to it.
    Attached(String),
    /// Both keys already belonged to the same entity — nothing to do.
    AlreadyLinked(String),
    /// Both keys were known to *different* entities, which were merged.
    Merged(String),
}

impl LinkOutcome {
    /// The canonical id the two keys now share.
    pub fn canonical_id(&self) -> &str {
        match self {
            LinkOutcome::Created(c)
            | LinkOutcome::Attached(c)
            | LinkOutcome::AlreadyLinked(c)
            | LinkOutcome::Merged(c) => c,
        }
    }
}

/// True iff the two entities carry an **identity key of the same kind with
/// different values** — e.g. two distinct `google_place_id`s, or two Wikidata
/// QIDs. That is the meaningful false-merge signal: same-kind identity keys name
/// provably distinct things (two chain locations, two films), so merging them is
/// almost certainly wrong. Cross-kind identity keys (a `google_place_id` and a
/// `wikidata` QID) are exactly the links a human means to assert, so they are
/// NOT a conflict — only same-kind, different-value collisions are.
fn same_kind_identity_conflict(a: &[ExternalId], b: &[ExternalId]) -> bool {
    let ident = |ids: &[ExternalId]| -> Vec<(&'static str, String)> {
        ids.iter()
            .filter(|id| id.spec().grain == Grain::Identity)
            .map(|id| (id.kind_tag(), id.value().to_string()))
            .collect()
    };
    let ka = ident(a);
    let kb = ident(b);
    ka.iter().any(|(tag_a, val_a)| {
        kb.iter()
            .any(|(tag_b, val_b)| tag_a == tag_b && val_a != val_b)
    })
}

/// Re-select an entity's anchor from its current members and persist it. The
/// graph edge-moving primitives never touch the anchor, so every correction op
/// must call this on the entities it changed (mirrors `resolve.rs`'s post-merge
/// re-anchor). Preserves a synthetic `local:` anchor when no strong key remains.
async fn reanchor(graph: &dyn GraphStore, canonical_id: &str) -> Result<()> {
    let entity = match graph.get_entity(canonical_id).await? {
        Some(e) => e,
        None => return Ok(()),
    };
    let members = graph.members(canonical_id).await?;
    let anchor = anchor::recompute_anchor(&members, &entity.anchor);
    if anchor != entity.anchor {
        graph.set_anchor(canonical_id, &anchor).await?;
    }
    Ok(())
}

/// Parse a stored `kind:value` key, requiring it be a *strong* key. Weak keys
/// (phone) are corroborators only — they may never be the sole basis of a link
/// or the identity that a split extracts, so they are rejected here.
fn parse_strong_key(key: &str) -> Result<ExternalId> {
    let id = ExternalId::from_key(key)
        .ok_or_else(|| anyhow::anyhow!("not a valid kind:value key: {key:?}"))?;
    if !id.is_strong() {
        bail!("{key} is a weak (corroborator) key; only strong keys can be linked or split");
    }
    Ok(id)
}

/// Assert two identifier keys name the same entity.
///
/// Cases: neither known → mint a new entity holding both; exactly one known →
/// attach the other to it; both known & same → no-op; both known & different →
/// [`merge`] them. Refuses (unless `force`) when the two sides carry conflicting
/// identity keys, so `link` can never silently force a false merge.
pub async fn link(graph: &dyn GraphStore, a_key: &str, b_key: &str, force: bool) -> Result<LinkOutcome> {
    let a = parse_strong_key(a_key)?;
    let b = parse_strong_key(b_key)?;
    let a_key = a.key();
    let b_key = b.key();
    if a_key == b_key {
        bail!("cannot link a key to itself: {a_key}");
    }

    let a_cid = graph.find(&a_key).await?;
    let b_cid = graph.find(&b_key).await?;

    match (a_cid, b_cid) {
        (None, None) => {
            // Neither known: mint one entity anchored on the stronger key. Guard
            // first — two distinct same-kind identity keys (e.g. two place_ids)
            // must not silently fuse into one entity just because both are new.
            if !force && same_kind_identity_conflict(&[a.clone()], &[b.clone()]) {
                bail!(
                    "refusing to link {a_key} and {b_key}: they are distinct identity keys \
                     of the same kind (pass --force to override)"
                );
            }
            let ids = vec![a, b];
            let anchor = anchor::choose_anchor(&ids);
            let cid = anchor::canonical_id_for(&anchor);
            graph.create_entity(&cid, &anchor, None, None).await?;
            graph.attach_with_source(&a_key, &cid, Some("link")).await?;
            graph.attach_with_source(&b_key, &cid, Some("link")).await?;
            Ok(LinkOutcome::Created(cid))
        }
        (Some(cid), None) => {
            attach_guarded(graph, &cid, &b, &b_key, force).await?;
            Ok(LinkOutcome::Attached(cid))
        }
        (None, Some(cid)) => {
            attach_guarded(graph, &cid, &a, &a_key, force).await?;
            Ok(LinkOutcome::Attached(cid))
        }
        (Some(ca), Some(cb)) if ca == cb => Ok(LinkOutcome::AlreadyLinked(ca)),
        (Some(ca), Some(cb)) => {
            let winner = merge(graph, &[ca, cb], force).await?;
            Ok(LinkOutcome::Merged(winner))
        }
    }
}

/// Attach a new key to an existing entity, but only after checking the incoming
/// key does not carry a same-kind identity conflict with the entity's current
/// members (mirrors the both-known branch delegating to `merge`, so `link` can
/// never silently force a false merge from *any* branch). Respects `force`.
async fn attach_guarded(
    graph: &dyn GraphStore,
    cid: &str,
    incoming: &ExternalId,
    incoming_key: &str,
    force: bool,
) -> Result<()> {
    if !force {
        let members = graph.members(cid).await?;
        if same_kind_identity_conflict(&members, std::slice::from_ref(incoming)) {
            bail!(
                "refusing to link {incoming_key} into {cid}: it carries an identity key of the \
                 same kind but a different value than an existing member (pass --force to override)"
            );
        }
    }
    graph.attach_with_source(incoming_key, cid, Some("link")).await?;
    reanchor(graph, cid).await?;
    Ok(())
}

/// Combine several entities into one, returning the surviving canonical id.
///
/// The survivor is the entity with the strongest anchor (ties broken by
/// canonical id for determinism). Losers' name/type backfill the winner before
/// they are dropped, and the winner is re-anchored over the merged membership
/// (the step [`GraphStore::merge_into`] omits). Refuses (unless `force`) when any two
/// inputs carry disjoint identity keys — that would be a false merge.
pub async fn merge(graph: &dyn GraphStore, canonical_ids: &[String], force: bool) -> Result<String> {
    // Distinct, existing ids only.
    let mut cids: Vec<String> = Vec::new();
    for c in canonical_ids {
        if graph.get_entity(c).await?.is_none() {
            bail!("no entity with canonical_id {c}");
        }
        if !cids.contains(c) {
            cids.push(c.clone());
        }
    }
    match cids.len() {
        0 => bail!("merge needs at least one canonical id"),
        1 => return Ok(cids.into_iter().next().unwrap()),
        _ => {}
    }

    // False-merge guard: no two entities may carry disjoint identity keys.
    if !force {
        // A plain loop, not `.map(...).collect()`: an async call cannot run inside
        // a sync closure.
        let mut members: Vec<Vec<ExternalId>> = Vec::with_capacity(cids.len());
        for c in &cids {
            members.push(graph.members(c).await?);
        }
        for i in 0..members.len() {
            for j in (i + 1)..members.len() {
                if same_kind_identity_conflict(&members[i], &members[j]) {
                    bail!(
                        "refusing to merge {} and {}: they carry distinct identity keys \
                         of the same kind (pass --force to override)",
                        cids[i],
                        cids[j]
                    );
                }
            }
        }
    }

    // Strongest anchor wins; ties broken by canonical id.
    let winner = pick_winner(graph, &cids).await?;
    for loser in cids.iter().filter(|c| **c != winner) {
        // Backfill display metadata from the loser before it is dropped.
        if let Some(le) = graph.get_entity(loser).await? {
            graph.enrich_entity(&winner, le.entity_type.as_deref(), le.name.as_deref()).await?;
        }
        graph.merge_into(&winner, loser).await?;
    }
    reanchor(graph, &winner).await?;
    Ok(winner)
}

/// Pick the merge survivor: strongest anchor (lowest rank), ties broken by the
/// smaller canonical id for determinism.
async fn pick_winner(graph: &dyn GraphStore, canonicals: &[String]) -> Result<String> {
    let mut best: Option<(u8, String)> = None;
    for cid in canonicals {
        let anchor = graph
            .get_entity(cid).await?
            .ok_or_else(|| anyhow::anyhow!("entity {cid} not found"))?
            .anchor;
        let cand = (anchor::anchor_key_rank(&anchor), cid.clone());
        best = match best {
            None => Some(cand),
            Some(cur) if cand.0 < cur.0 || (cand.0 == cur.0 && cand.1 < cur.1) => Some(cand),
            Some(cur) => Some(cur),
        };
    }
    Ok(best.expect("non-empty canonicals").1)
}

/// Detach one or more strong keys from their entity onto a fresh one, returning
/// the new canonical id.
///
/// This is the recovery path for a false merge. It moves exactly the named keys
/// — it does NOT guess which other keys belong with them (shared corroborators
/// like a phone or a chain domain stay put; the operator names what to peel).
/// Both the remaining side and the new side are re-anchored. Refuses to detach
/// every strong key of an entity (that would orphan it, not split it).
pub async fn split(graph: &dyn GraphStore, keys: &[String]) -> Result<String> {
    if keys.is_empty() {
        bail!("split needs at least one key to detach");
    }
    // Parse, require strong, and confirm all keys currently share one entity.
    let mut ids: Vec<ExternalId> = Vec::new();
    let mut src: Option<String> = None;
    for key in keys {
        let id = parse_strong_key(key)?;
        let k = id.key();
        let cid = graph
            .find(&k).await?
            .ok_or_else(|| anyhow::anyhow!("key {k} is not in the graph"))?;
        match &src {
            None => src = Some(cid),
            Some(s) if *s == cid => {}
            Some(s) => bail!(
                "keys belong to different entities ({s} vs {cid}); split operates within one entity"
            ),
        }
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    let src = src.expect("keys non-empty");

    // Orphan guard: leave at least one strong key on the source.
    if ids.len() >= graph.strong_key_count(&src).await? {
        bail!(
            "refusing to detach all strong keys from {src} (that orphans it, not splits it); \
             leave at least one strong key on the original"
        );
    }

    // The detached side anchors on its own strongest key. The canonical id is
    // deterministic from that anchor; if it collides with an existing entity
    // (including the source, when we detach the very key the source is anchored
    // on), suffix-disambiguate so the new entity is distinct and `create_entity`
    // (INSERT OR IGNORE) does not silently no-op onto a live row.
    let new_anchor = anchor::choose_anchor(&ids);
    let base_cid = anchor::canonical_id_for(&new_anchor);
    //
    // Each probe is a round trip on a network-backed store, so the search is
    // bounded: reaching the cap means something is pathologically wrong (a runaway
    // suffix chain), and failing loudly beats spinning.
    const MAX_CID_PROBES: usize = 100;
    let mut new_cid = base_cid.clone();
    let mut n = 1;
    while graph.get_entity(&new_cid).await?.is_some() {
        if n > MAX_CID_PROBES {
            bail!(
                "cannot allocate a canonical id for the split of {src}: {MAX_CID_PROBES} \
                 suffixed variants of {base_cid} are all taken"
            );
        }
        new_cid = format!("{base_cid}_{n}");
        n += 1;
    }

    // Apply the DB mutations atomically in one Graph method: mint the new entity,
    // move the named keys, invalidate the source's stale name caches (they can no
    // longer be reliably mapped to a post-split side), and re-anchor both sides.
    let detached: Vec<String> = ids.iter().map(|id| id.key()).collect();
    graph.apply_split(&new_cid, &new_anchor, &detached, &src).await?;
    Ok(new_cid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EntityRecord;
    use crate::store::SqliteStore;
    use crate::resolve::commit_record;

    async fn ids(g: &dyn GraphStore, keys: &[&str]) -> String {
        // Commit a single record carrying all keys → one entity.
        let record = EntityRecord {
            same_as: keys.iter().map(|k| ExternalId::from_key(k).unwrap()).collect(),
            ..Default::default()
        };
        commit_record(g, &record)
            .await
            .unwrap()
            .canonical_id
            .expect("resolved")
    }

    #[tokio::test]
    async fn link_creates_when_neither_known() {
        let g = SqliteStore::open_in_memory().unwrap();
        let out = link(&g, "google_place_id:A", "yelp:joes", false).await.unwrap();
        let cid = out.canonical_id().to_string();
        assert!(matches!(out, LinkOutcome::Created(_)));
        assert_eq!(g.find("google_place_id:A").await.unwrap().as_deref(), Some(cid.as_str()));
        assert_eq!(g.find("yelp:joes").await.unwrap().as_deref(), Some(cid.as_str()));
    }

    #[tokio::test]
    async fn link_attaches_to_existing() {
        let g = SqliteStore::open_in_memory().unwrap();
        let cid = ids(&g, &["wikidata:Q1"]).await;
        let out = link(&g, "wikidata:Q1", "imdb:tt0111161", false).await.unwrap();
        assert_eq!(out, LinkOutcome::Attached(cid.clone()));
        assert_eq!(g.find("imdb:tt0111161").await.unwrap().as_deref(), Some(cid.as_str()));
    }

    #[tokio::test]
    async fn link_same_entity_is_noop() {
        let g = SqliteStore::open_in_memory().unwrap();
        let cid = ids(&g, &["wikidata:Q1", "imdb:tt0111161"]).await;
        let out = link(&g, "wikidata:Q1", "imdb:tt0111161", false).await.unwrap();
        assert_eq!(out, LinkOutcome::AlreadyLinked(cid));
    }

    #[tokio::test]
    async fn link_rejects_phone_key() {
        let g = SqliteStore::open_in_memory().unwrap();
        assert!(link(&g, "phone:+15550001111", "wikidata:Q1", false).await.is_err());
    }

    #[tokio::test]
    async fn merge_picks_strongest_anchor_and_reanchors() {
        let g = SqliteStore::open_in_memory().unwrap();
        // Two entities: one anchored on a place_id (rank 3), one on wikidata (0).
        let weak = ids(&g, &["google_place_id:P"]).await;
        let strong = ids(&g, &["wikidata:Q1"]).await;
        let winner = merge(&g, &[weak.clone(), strong.clone()], false).await.unwrap();
        assert_eq!(winner, strong);
        // Winner absorbed the place_id and stayed anchored on wikidata.
        assert_eq!(g.find("google_place_id:P").await.unwrap().as_deref(), Some(strong.as_str()));
        assert_eq!(g.get_entity(&strong).await.unwrap().unwrap().anchor, "wikidata:Q1");
        assert!(g.get_entity(&weak).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn merge_refuses_conflicting_identities_without_force() {
        let g = SqliteStore::open_in_memory().unwrap();
        let a = ids(&g, &["wikidata:Q1"]).await;
        let b = ids(&g, &["wikidata:Q2"]).await;
        assert!(merge(&g, &[a.clone(), b.clone()], false).await.is_err());
        // --force overrides.
        assert!(merge(&g, &[a, b], true).await.is_ok());
    }

    #[tokio::test]
    async fn merge_backfills_name_from_loser() {
        let g = SqliteStore::open_in_memory().unwrap();
        // Winner (wikidata) has no name; loser (place_id) carries one.
        let winner = ids(&g, &["wikidata:Q1"]).await;
        let record = EntityRecord {
            name: Some("Joe's Pizza".into()),
            same_as: vec![ExternalId::from_key("google_place_id:P").unwrap()],
            ..Default::default()
        };
        let loser = commit_record(&g, &record).await.unwrap().canonical_id.unwrap();
        merge(&g, &[winner.clone(), loser], false).await.unwrap();
        assert_eq!(g.get_entity(&winner).await.unwrap().unwrap().name.as_deref(), Some("Joe's Pizza"));
    }

    #[tokio::test]
    async fn split_round_trips_a_bad_merge() {
        let g = SqliteStore::open_in_memory().unwrap();
        // A false merge: two place locations forced into one entity.
        let cid = ids(&g, &["google_place_id:MHTN", "google_place_id:BKN"]).await;
        // Peel the Brooklyn location back out.
        let new_cid = split(&g, &["google_place_id:BKN".into()]).await.unwrap();
        assert_ne!(new_cid, cid);
        assert_eq!(g.find("google_place_id:MHTN").await.unwrap().as_deref(), Some(cid.as_str()));
        assert_eq!(g.find("google_place_id:BKN").await.unwrap().as_deref(), Some(new_cid.as_str()));
        // Both sides are anchored on their own place_id.
        assert_eq!(g.get_entity(&new_cid).await.unwrap().unwrap().anchor, "google_place_id:BKN");
    }

    #[tokio::test]
    async fn split_shared_corroborator_stays_put() {
        let g = SqliteStore::open_in_memory().unwrap();
        // Two locations + a shared phone corroborator, all on one entity.
        let cid = ids(&g, &["google_place_id:MHTN", "google_place_id:BKN", "phone:+15550001111"]).await;
        let new_cid = split(&g, &["google_place_id:BKN".into()]).await.unwrap();
        // The phone (a corroborator) is NOT named, so it stays with the source.
        assert_eq!(g.find_phone("phone:+15550001111").await.unwrap(), vec![cid]);
        assert_eq!(g.members(&new_cid).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn split_refuses_to_orphan() {
        let g = SqliteStore::open_in_memory().unwrap();
        ids(&g, &["google_place_id:ONLY"]).await;
        // Detaching the sole strong key would orphan the entity.
        assert!(split(&g, &["google_place_id:ONLY".into()]).await.is_err());
    }

    #[tokio::test]
    async fn split_refuses_keys_from_different_entities() {
        let g = SqliteStore::open_in_memory().unwrap();
        ids(&g, &["google_place_id:A", "yelp:a"]).await;
        ids(&g, &["google_place_id:B", "yelp:b"]).await;
        assert!(split(&g, &["google_place_id:A".into(), "google_place_id:B".into()]).await.is_err());
    }

    #[tokio::test]
    async fn split_invalidates_stale_name_cache_on_source() {
        let g = SqliteStore::open_in_memory().unwrap();
        // A false merge: two locations fused, then indexed by name under the
        // SOURCE cid (both the index and a unique cardinality memory).
        let cid = ids(&g, &["google_place_id:MHTN", "google_place_id:BKN"]).await;
        g.index_name("joes", &["ny".into()], &cid, Some("t")).await.unwrap();
        g.record_name_unique("joes", &["ny".into()], &cid).await.unwrap();

        // Peel the Brooklyn location out. The cached name rows named the source
        // and can't be re-mapped to a post-split side, so they must be dropped —
        // otherwise a name query would confidently return the SOURCE cid for an
        // identity that may now live on the new entity (a false-merge break).
        let new_cid = split(&g, &["google_place_id:BKN".into()]).await.unwrap();
        assert_ne!(new_cid, cid);
        assert!(g.find_by_name("joes", &["ny".into()]).unwrap().is_empty());
        assert!(!g
            .name_entities("joes").await
            .unwrap()
            .iter()
            .any(|(c, _)| c == &cid));
        assert!(g.name_cardinality("joes", &["ny".into()]).await.unwrap().is_none());
        // Both sides remain findable by their strong keys.
        assert_eq!(g.find("google_place_id:MHTN").await.unwrap().as_deref(), Some(cid.as_str()));
        assert_eq!(g.find("google_place_id:BKN").await.unwrap().as_deref(), Some(new_cid.as_str()));
    }

    #[tokio::test]
    async fn link_refuses_two_new_conflicting_identity_keys() {
        let g = SqliteStore::open_in_memory().unwrap();
        // Fresh graph, Created branch: two distinct place_ids must not fuse.
        assert!(link(&g, "google_place_id:A", "google_place_id:B", false).await.is_err());
        // Neither key was created by the refused link.
        assert!(g.find("google_place_id:A").await.unwrap().is_none());
        assert!(g.find("google_place_id:B").await.unwrap().is_none());
        // --force overrides and fuses them into one entity.
        let out = link(&g, "google_place_id:A", "google_place_id:B", true).await.unwrap();
        let cid = out.canonical_id().to_string();
        assert!(matches!(out, LinkOutcome::Created(_)));
        assert_eq!(g.find("google_place_id:A").await.unwrap().as_deref(), Some(cid.as_str()));
        assert_eq!(g.find("google_place_id:B").await.unwrap().as_deref(), Some(cid.as_str()));
    }

    #[tokio::test]
    async fn link_refuses_conflicting_key_attached_to_existing() {
        let g = SqliteStore::open_in_memory().unwrap();
        // Existing entity holds place_id:A + a yelp key. Attaching a DIFFERENT
        // same-kind place_id (Attached branch) must be refused without --force.
        let cid = ids(&g, &["google_place_id:A", "yelp:joes"]).await;
        assert!(link(&g, "yelp:joes", "google_place_id:B", false).await.is_err());
        assert!(g.find("google_place_id:B").await.unwrap().is_none());
        // A cross-kind attach (the link a human means) still succeeds.
        let out = link(&g, "yelp:joes", "imdb:tt0111161", false).await.unwrap();
        assert_eq!(out, LinkOutcome::Attached(cid));
    }
}
