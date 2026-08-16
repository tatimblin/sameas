//! Backend-agnostic conformance suite for [`GraphStore`].
//!
//! Every case here is written against `&dyn GraphStore`, so it can be run against
//! any backend. Today only [`super::SqliteStore`] is reachable from a host test;
//! `D1Store` needs a Worker + miniflare, so when that harness lands it can call
//! [`run_all`] and inherit this whole suite rather than re-deriving it.
//!
//! These assert the **invariants the project treats as load-bearing** — the ones a
//! backend could plausibly get wrong in a way ordinary unit tests wouldn't catch:
//!
//! * union-find transitivity through storage
//! * phone never merges two entities (corroborator-only)
//! * a merge re-points every referencing table and leaves nothing dangling
//! * `apply_split` re-anchors both sides — the case where the D1 backend
//!   deliberately diverges (precomputed anchors vs. reading its own writes), so
//!   agreement here is what proves that rewrite equivalent
//! * the derived membership views agree with the `member_rows` primitive
//! * `name_cardinality`'s liveness rule
//! * a source-less re-attach preserves existing provenance — the invariant the D1
//!   backend's empty-string/`NULLIF` convention exists to protect
//!
//! Not a substitute for the per-backend unit tests: those cover SQL details
//! (migrations, upsert provenance) that only one backend has.

use anyhow::Result;

use super::{GraphStore, NameCardinality};

/// Run every conformance case against `store`.
///
/// The store must be **empty**. Cases use disjoint key/id namespaces so they can
/// share one store without interfering.
pub async fn run_all(store: &dyn GraphStore) -> Result<()> {
    union_find_is_transitive(store).await?;
    phone_never_merges(store).await?;
    merge_repoints_every_table(store).await?;
    member_views_agree_with_primitive(store).await?;
    split_reanchors_both_sides(store).await?;
    name_cardinality_liveness(store).await?;
    find_many_matches_find(store).await?;
    attach_preserves_existing_provenance(store).await?;
    Ok(())
}

/// Any identifier in a cluster resolves to the same canonical id.
async fn union_find_is_transitive(g: &dyn GraphStore) -> Result<()> {
    g.create_entity("t1_cx", "domain:t1a.com", None, None).await?;
    for k in ["domain:t1a.com", "domain:t1b.com", "wikidata:QT1"] {
        g.attach(k, "t1_cx").await?;
    }
    let a = g.find("domain:t1a.com").await?;
    let b = g.find("domain:t1b.com").await?;
    let c = g.find("wikidata:QT1").await?;
    assert_eq!(a.as_deref(), Some("t1_cx"), "transitivity: first key");
    assert_eq!(a, b, "transitivity: all keys agree");
    assert_eq!(b, c, "transitivity: all keys agree");
    Ok(())
}

/// A shared phone is recorded but must NEVER fuse two entities. This is the
/// project's primary safety invariant, and it is structural: phone edges live
/// outside the union-find entirely.
async fn phone_never_merges(g: &dyn GraphStore) -> Result<()> {
    g.create_entity("t2_x", "google_place_id:T2X", None, None)
        .await?;
    g.create_entity("t2_y", "google_place_id:T2Y", None, None)
        .await?;
    g.attach("google_place_id:T2X", "t2_x").await?;
    g.attach("google_place_id:T2Y", "t2_y").await?;

    let shared = "phone:+15550100200";
    g.add_phone_edge(shared, "t2_x").await?;
    g.add_phone_edge(shared, "t2_y").await?;

    let corroborated = g.find_phone(shared).await?;
    assert_eq!(
        corroborated.len(),
        2,
        "one phone may corroborate several entities"
    );
    // The entities remain distinct.
    assert!(g.get_entity("t2_x").await?.is_some());
    assert!(g.get_entity("t2_y").await?.is_some());
    assert_ne!(
        g.find("google_place_id:T2X").await?,
        g.find("google_place_id:T2Y").await?,
        "a shared phone must not merge two entities"
    );
    Ok(())
}

/// A merge must re-point nodes, phone edges, and cached name rows, then drop the
/// loser — leaving no row referencing a deleted canonical id.
async fn merge_repoints_every_table(g: &dyn GraphStore) -> Result<()> {
    g.create_entity("t3_win", "wikidata:QT3", None, None).await?;
    g.create_entity("t3_lose", "domain:t3.com", None, None)
        .await?;
    g.attach("wikidata:QT3", "t3_win").await?;
    g.attach("domain:t3.com", "t3_lose").await?;
    g.add_phone_edge("phone:+15550300400", "t3_lose").await?;
    g.index_name("t3 diner", &["springfield".into()], "t3_lose", None)
        .await?;
    g.record_name_unique("t3 diner", &["springfield".into()], "t3_lose")
        .await?;

    g.merge_into("t3_win", "t3_lose").await?;

    assert!(
        g.get_entity("t3_lose").await?.is_none(),
        "loser entity row is dropped"
    );
    assert_eq!(
        g.find("domain:t3.com").await?.as_deref(),
        Some("t3_win"),
        "loser's strong key re-points to the winner"
    );
    assert_eq!(
        g.find_phone("phone:+15550300400").await?,
        vec!["t3_win".to_string()],
        "loser's phone edge re-points to the winner"
    );
    let named = g.name_entities("t3 diner").await?;
    assert!(
        named.iter().all(|(cid, _)| cid != "t3_lose"),
        "no name_index row may still name the deleted entity"
    );
    // The unique cardinality row followed the merge and still resolves.
    match g
        .name_cardinality("t3 diner", &["springfield".into()])
        .await?
    {
        Some(NameCardinality::Unique(cid)) => {
            assert_eq!(cid, "t3_win", "unique cardinality re-points to the winner")
        }
        other => panic!("expected a live unique row after merge, got {other:?}"),
    }
    Ok(())
}

/// `member_keys` / `member_sources` / `members` are derived from `member_rows`; a
/// backend overriding any of them must stay consistent with the primitive.
async fn member_views_agree_with_primitive(g: &dyn GraphStore) -> Result<()> {
    g.create_entity("t4_cx", "wikidata:QT4", None, None).await?;
    g.attach_with_source("wikidata:QT4", "t4_cx", Some("input"))
        .await?;
    g.attach_with_source("domain:t4.com", "t4_cx", Some("wikidata"))
        .await?;
    g.add_phone_edge_with_source("phone:+15550500600", "t4_cx", Some("google_places"))
        .await?;

    let rows = g.member_rows("t4_cx").await?;
    let keys = g.member_keys("t4_cx").await?;
    let sources = g.member_sources("t4_cx").await?;
    let typed = g.members("t4_cx").await?;

    assert_eq!(rows.len(), 3, "three member edges");
    assert_eq!(keys.len(), rows.len(), "member_keys covers every row");
    assert_eq!(sources.len(), rows.len(), "member_sources covers every row");
    // `members` drops nothing here: all three keys are parseable identifiers.
    assert_eq!(typed.len(), rows.len(), "members covers every row");
    // member_sources is sorted by key, for stable reporting.
    let mut sorted = sources.clone();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(sources, sorted, "member_sources is sorted by key");
    // Provenance survives the round trip.
    assert!(
        sources
            .iter()
            .any(|(k, s)| k == "domain:t4.com" && s.as_deref() == Some("wikidata")),
        "edge provenance is preserved"
    );
    Ok(())
}

/// `apply_split` must move the named keys AND re-anchor both sides.
///
/// This is the case where the two backends are implemented differently — SQLite
/// re-anchors by reading its own uncommitted writes; D1 precomputes both anchors
/// before a single atomic batch. Identical observable outcomes here is the evidence
/// that the D1 rewrite is equivalent.
async fn split_reanchors_both_sides(g: &dyn GraphStore) -> Result<()> {
    // One entity holding two identity keys; the weaker-ranked one is peeled off.
    // wikidata outranks google_place_id, so the source keeps the QID anchor and the
    // detached side takes the place_id.
    g.create_entity("t5_src", "wikidata:QT5", None, None).await?;
    g.attach("wikidata:QT5", "t5_src").await?;
    g.attach("google_place_id:T5P", "t5_src").await?;
    g.index_name("t5 cafe", &["oakland".into()], "t5_src", None)
        .await?;

    g.apply_split(
        "t5_new",
        "google_place_id:T5P",
        &["google_place_id:T5P".to_string()],
        "t5_src",
    )
    .await?;

    assert_eq!(
        g.find("google_place_id:T5P").await?.as_deref(),
        Some("t5_new"),
        "the detached key moved to the new entity"
    );
    assert_eq!(
        g.find("wikidata:QT5").await?.as_deref(),
        Some("t5_src"),
        "the retained key stayed with the source"
    );

    let src = g.get_entity("t5_src").await?.expect("source survives");
    let new = g.get_entity("t5_new").await?.expect("new entity minted");
    assert_eq!(
        src.anchor, "wikidata:QT5",
        "source re-anchors over its POST-split membership"
    );
    assert_eq!(
        new.anchor, "google_place_id:T5P",
        "new side anchors on the key it received"
    );

    // The source's cached name rows are invalidated: they can no longer be mapped
    // reliably to a post-split side.
    let named = g.name_entities("t5 cafe").await?;
    assert!(
        named.is_empty(),
        "split invalidates the source's cached name rows, got {named:?}"
    );
    Ok(())
}

/// A `unique` row naming an entity that no longer exists must read as a miss, not
/// as a dead hit.
async fn name_cardinality_liveness(g: &dyn GraphStore) -> Result<()> {
    let quals = vec!["portland".to_string()];
    g.create_entity("t6_cx", "wikidata:QT6", None, None).await?;
    g.attach("wikidata:QT6", "t6_cx").await?;
    g.record_name_unique("t6 grill", &quals, "t6_cx").await?;

    match g.name_cardinality("t6 grill", &quals).await? {
        Some(NameCardinality::Unique(cid)) => assert_eq!(cid, "t6_cx"),
        other => panic!("expected a live unique row, got {other:?}"),
    }

    // Exact qualifier-set match only: a coarser query must not be answered by a
    // finer stored fact.
    assert!(
        g.name_cardinality("t6 grill", &[]).await?.is_none(),
        "cardinality lookup is specificity-exact"
    );

    // Merge the named entity away; the row now points at a dead id.
    g.create_entity("t6_other", "wikidata:QT6B", None, None)
        .await?;
    g.attach("wikidata:QT6B", "t6_other").await?;
    g.merge_into("t6_other", "t6_cx").await?;
    // After the merge the row was re-pointed, so it resolves again — to the winner.
    match g.name_cardinality("t6 grill", &quals).await? {
        Some(NameCardinality::Unique(cid)) => {
            assert_eq!(cid, "t6_other", "unique row follows the merge winner")
        }
        other => panic!("expected the re-pointed unique row, got {other:?}"),
    }
    Ok(())
}

/// `find_many` is a provided default that a backend may override with a batched
/// query; it must agree with `find` key-for-key, preserve input order, and report
/// misses as `None` rather than dropping them.
async fn find_many_matches_find(g: &dyn GraphStore) -> Result<()> {
    g.create_entity("t7_cx", "wikidata:QT7", None, None).await?;
    g.attach("wikidata:QT7", "t7_cx").await?;
    g.attach("domain:t7.com", "t7_cx").await?;

    let keys = vec![
        "domain:t7.com".to_string(),
        "wikidata:QT7NOPE".to_string(), // deliberate miss, in the middle
        "wikidata:QT7".to_string(),
    ];
    let many = g.find_many(&keys).await?;
    assert_eq!(many.len(), keys.len(), "one result per input key");
    for (i, (k, hit)) in many.iter().enumerate() {
        assert_eq!(k, &keys[i], "find_many preserves input order");
        assert_eq!(hit, &g.find(k).await?, "find_many agrees with find for {k}");
    }
    assert!(many[1].1.is_none(), "a miss is reported as None, not dropped");
    Ok(())
}

/// Re-attaching a key with **no source** must PRESERVE the provenance already
/// recorded, never erase it.
///
/// Both backends use the same upsert:
/// `source = COALESCE(excluded.source, nodes.source)` — which keeps the existing
/// value only when the incoming source is SQL NULL.
///
/// This is the highest-consequence site of the D1 backend's empty-string
/// convention. `d1_codec::bind_opt` binds `None` as `''` (a JS null arrives at D1
/// as the Worker's `Env` object), so without the `NULLIF(?3, '')` in
/// `d1.rs::attach_with_source` the incoming `excluded.source` is `''` — which is
/// NOT NULL, so `COALESCE` takes it and **silently erases** the recorded
/// provenance on every source-less re-attach. No error, no log: reads deserialize
/// `""` to `Some("")`, so it looks like a present-but-empty value.
///
/// Worth stating why this lives here rather than in a Worker HTTP test: no
/// endpoint reaches a `None`-source attach. `repoint_key` — the only such path —
/// has no production callers today, so this invariant is one reconcile-worker
/// feature away from going live and is only checkable at the trait boundary.
async fn attach_preserves_existing_provenance(g: &dyn GraphStore) -> Result<()> {
    g.create_entity("t8_cx", "wikidata:QT8", None, None).await?;
    g.attach_with_source("wikidata:QT8", "t8_cx", Some("wikidata"))
        .await?;
    // Re-attach the SAME key with no source (what `repoint_key` does).
    g.attach_with_source("wikidata:QT8", "t8_cx", None).await?;

    let sources = g.member_sources("t8_cx").await?;
    let got = sources
        .iter()
        .find(|(k, _)| k == "wikidata:QT8")
        .map(|(_, s)| s.clone())
        .expect("the key is still a member after a re-attach");
    assert_eq!(
        got.as_deref(),
        Some("wikidata"),
        "a source-less re-attach must not erase existing provenance (got {got:?})"
    );
    // The distinction that actually fails when the NULLIF is dropped: an erased
    // provenance shows up as the empty string, not as None.
    assert_ne!(
        got.as_deref(),
        Some(""),
        "provenance was erased to the empty string — the NULLIF is missing"
    );
    Ok(())
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;
    use crate::store::SqliteStore;

    #[tokio::test]
    async fn sqlite_store_satisfies_the_contract() {
        let store = SqliteStore::open_in_memory().unwrap();
        run_all(&store).await.unwrap();
    }
}
