//! [`D1Store`] — the Cloudflare D1 backend, for the WASM Worker build.
//!
//! Same logical schema and same semantics as [`super::sqlite::SqliteStore`], with
//! four deliberate differences forced by the platform:
//!
//! 1. **No schema management.** D1 migrations are external SQL files applied by
//!    `wrangler d1 migrations apply`, so `D1Store::new` runs no DDL. `SqliteStore`
//!    remains the only backend that creates and migrates its own schema.
//! 2. **Multi-statement writes go through `batch()`**, which D1 documents as a
//!    transaction (sequential, non-concurrent, whole-sequence rollback on failure).
//!    That preserves the atomicity `merge_into` and `apply_split` need.
//! 3. **`apply_split` precomputes anchors.** The `rusqlite` version reads its own
//!    uncommitted writes mid-transaction; a `batch()` is a fixed statement list and
//!    cannot. Since the key moves are known up front, both post-split memberships
//!    are derivable *before* any write — see [`D1Store::apply_split`].
//! 4. **Optional text is bound as the EMPTY STRING, and the SQL must undo it.**
//!    [`bind_opt`](super::d1_codec::bind_opt) cannot bind a JS null: in a `cdylib`
//!    built through `worker-build`, both `JsValue::NULL` and `JsValue::null()`
//!    arrive at D1 as the Worker's `Env` object and are rejected with
//!    `D1_TYPE_ERROR: Type 'object' not supported`. So **every statement below that
//!    takes a `bind_opt` wraps that placeholder in `NULLIF(?N, '')`.** Omitting it
//!    stores `''` where NULL was meant, *silently*: no error, and reads tolerate it
//!    invisibly (`Option<String>` deserializes `""` to `Some("")`, never `None`).
//!    A SQL *literal* `NULL` is fine — see `apply_split`'s `VALUES (?1, ?2, NULL)`;
//!    only *bindings* are affected. Enforced by
//!    `crates/sameas-core/tests/d1_nullif_contract.rs`, and the behavioural
//!    consequence is pinned by `conformance::attach_preserves_existing_provenance`.
//!
//! Every statement here is a network round trip, so reads that `SqliteStore` splits
//! for convenience are merged where practical (`member_rows` is one `UNION ALL`,
//! `find_many` is one chunked `IN (...)`, `stats` is one batch).

use anyhow::{bail, Result};
use async_trait::async_trait;
use serde::Deserialize;
use worker::D1Database;

use super::d1_codec::{batch, bind_f64, bind_opt, bind_str, chunked_rows, first_row, rows, run, stmt};
use super::{
    qualifier_set_key, EntityRow, GraphStore, MemberRow, NameCandidate, NameCardinality,
    StatsReport,
};
use crate::anchor;
use crate::model::ExternalId;

/// The crosswalk graph backed by a Cloudflare D1 database.
///
/// Holds the binding handle from the Worker's `Env`. Cheap to construct per
/// request — `D1Database` is itself a handle to the JS-side binding.
pub struct D1Store {
    db: D1Database,
}

impl D1Store {
    pub fn new(db: D1Database) -> Self {
        D1Store { db }
    }
}

#[derive(Deserialize)]
struct CidRow {
    canonical_id: String,
}

#[derive(Deserialize)]
struct KeyedCidRow {
    key: String,
    canonical_id: String,
}

#[derive(Deserialize)]
struct EntityRowRaw {
    canonical_id: String,
    anchor: String,
    entity_type: Option<String>,
    name: Option<String>,
}

#[derive(Deserialize)]
struct MemberRowRaw {
    key: String,
    source: Option<String>,
}

#[derive(Deserialize)]
struct CountRow {
    n: i64,
}

#[derive(Deserialize)]
struct NameQualRow {
    canonical_id: String,
    qualifier: String,
}

#[derive(Deserialize)]
struct CardinalityRow {
    candidates: String,
    status: Option<String>,
    canonical_id: Option<String>,
}

#[derive(Deserialize)]
struct ReasonCountRow {
    reason_tag: String,
    n: i64,
}

#[async_trait(?Send)]
impl GraphStore for D1Store {
    // --- union-find over strong keys ------------------------------------

    async fn find(&self, key: &str) -> Result<Option<String>> {
        let s = stmt(
            &self.db,
            "SELECT canonical_id FROM nodes WHERE key = ?1",
            &[bind_str(key)],
        )?;
        Ok(first_row::<CidRow>(s).await?.map(|r| r.canonical_id))
    }

    /// One chunked `IN (...)` instead of N probes — the largest saving on the
    /// resolve path, which looks up every strong key a record carries.
    async fn find_many(&self, keys: &[String]) -> Result<Vec<(String, Option<String>)>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let found: Vec<KeyedCidRow> = chunked_rows(&self.db, keys, |ph| {
            (
                format!(
                    "SELECT key, canonical_id FROM nodes WHERE key IN ({})",
                    ph.join(", ")
                ),
                Vec::new(),
            )
        })
        .await?;
        // Preserve the caller's order, and report a miss as `None` rather than
        // dropping the key.
        Ok(keys
            .iter()
            .map(|k| {
                let hit = found
                    .iter()
                    .find(|r| &r.key == k)
                    .map(|r| r.canonical_id.clone());
                (k.clone(), hit)
            })
            .collect())
    }

    /// **The `NULLIF(?3, '')` below is load-bearing — the highest-consequence
    /// instance of difference 4 in the module header.**
    ///
    /// `source = COALESCE(excluded.source, nodes.source)` preserves the recorded
    /// provenance only when the incoming source is SQL NULL. `bind_opt(None)` binds
    /// `''`, which is NOT NULL — so without the `NULLIF`, `COALESCE` takes `''` and
    /// **silently erases** existing provenance on every source-less re-attach. No
    /// error, no log. Pinned by
    /// `conformance::attach_preserves_existing_provenance`.
    async fn attach_with_source(
        &self,
        key: &str,
        canonical_id: &str,
        source: Option<&str>,
    ) -> Result<()> {
        run(stmt(
            &self.db,
            "INSERT INTO nodes(key, canonical_id, source) VALUES (?1, ?2, NULLIF(?3, ''))
             ON CONFLICT(key) DO UPDATE SET
                 canonical_id = excluded.canonical_id,
                 source = COALESCE(excluded.source, nodes.source)",
            &[
                bind_str(key),
                bind_str(canonical_id),
                bind_opt(source),
            ],
        )?)
        .await
    }

    // --- phone edges (corroborator, outside union-find) -----------------

    async fn add_phone_edge_with_source(
        &self,
        phone_key: &str,
        canonical_id: &str,
        source: Option<&str>,
    ) -> Result<()> {
        run(stmt(
            &self.db,
            "INSERT OR IGNORE INTO phone_edges(phone_key, canonical_id, source)
             VALUES (?1, ?2, NULLIF(?3, ''))",
            &[
                bind_str(phone_key),
                bind_str(canonical_id),
                bind_opt(source),
            ],
        )?)
        .await
    }

    async fn find_phone(&self, phone_key: &str) -> Result<Vec<String>> {
        let s = stmt(
            &self.db,
            "SELECT canonical_id FROM phone_edges WHERE phone_key = ?1 ORDER BY canonical_id",
            &[bind_str(phone_key)],
        )?;
        Ok(rows::<CidRow>(s)
            .await?
            .into_iter()
            .map(|r| r.canonical_id)
            .collect())
    }

    // --- entities -------------------------------------------------------

    async fn create_entity(
        &self,
        canonical_id: &str,
        anchor: &str,
        entity_type: Option<&str>,
        name: Option<&str>,
    ) -> Result<()> {
        run(stmt(
            &self.db,
            "INSERT OR IGNORE INTO entities(canonical_id, anchor, entity_type, name)
             VALUES (?1, ?2, NULLIF(?3, ''), NULLIF(?4, ''))",
            &[
                bind_str(canonical_id),
                bind_str(anchor),
                bind_opt(entity_type),
                bind_opt(name),
            ],
        )?)
        .await
    }

    async fn get_entity(&self, canonical_id: &str) -> Result<Option<EntityRow>> {
        let s = stmt(
            &self.db,
            "SELECT canonical_id, anchor, entity_type, name FROM entities WHERE canonical_id = ?1",
            &[bind_str(canonical_id)],
        )?;
        Ok(first_row::<EntityRowRaw>(s).await?.map(|r| EntityRow {
            canonical_id: r.canonical_id,
            anchor: r.anchor,
            entity_type: r.entity_type,
            name: r.name,
        }))
    }

    async fn set_anchor(&self, canonical_id: &str, anchor: &str) -> Result<()> {
        run(stmt(
            &self.db,
            "UPDATE entities SET anchor = ?2 WHERE canonical_id = ?1",
            &[bind_str(canonical_id), bind_str(anchor)],
        )?)
        .await
    }

    /// Both column updates in one batch: two statements, one round trip, and
    /// atomic (the `rusqlite` version runs them unwrapped).
    async fn enrich_entity(
        &self,
        canonical_id: &str,
        entity_type: Option<&str>,
        name: Option<&str>,
    ) -> Result<()> {
        let mut stmts = Vec::new();
        if let Some(t) = entity_type {
            stmts.push(stmt(
                &self.db,
                "UPDATE entities SET entity_type = ?2
                 WHERE canonical_id = ?1 AND entity_type IS NULL",
                &[bind_str(canonical_id), bind_str(t)],
            )?);
        }
        if let Some(n) = name {
            stmts.push(stmt(
                &self.db,
                "UPDATE entities SET name = ?2 WHERE canonical_id = ?1 AND name IS NULL",
                &[bind_str(canonical_id), bind_str(n)],
            )?);
        }
        batch(&self.db, stmts).await
    }

    async fn strong_key_count(&self, canonical_id: &str) -> Result<usize> {
        let s = stmt(
            &self.db,
            "SELECT COUNT(*) AS n FROM nodes WHERE canonical_id = ?1",
            &[bind_str(canonical_id)],
        )?;
        Ok(first_row::<CountRow>(s).await?.map_or(0, |r| r.n as usize))
    }

    // --- members --------------------------------------------------------

    /// One `UNION ALL` rather than two queries — `SqliteStore` splits them only
    /// because a local read is free.
    async fn member_rows(&self, canonical_id: &str) -> Result<Vec<MemberRow>> {
        let s = stmt(
            &self.db,
            "SELECT key, source FROM nodes WHERE canonical_id = ?1
             UNION ALL
             SELECT phone_key AS key, source FROM phone_edges WHERE canonical_id = ?1",
            &[bind_str(canonical_id)],
        )?;
        Ok(rows::<MemberRowRaw>(s)
            .await?
            .into_iter()
            .map(|r| MemberRow {
                key: r.key,
                source: r.source,
            })
            .collect())
    }

    // --- local name index -----------------------------------------------

    /// One batch: the bare-name row plus one row per qualifier.
    async fn index_name(
        &self,
        name_norm: &str,
        qualifiers: &[String],
        canonical_id: &str,
        source: Option<&str>,
    ) -> Result<()> {
        if name_norm.is_empty() {
            return Ok(());
        }
        // Empty-string row lets a name-only query match; qualifier rows add facets.
        let mut quals: Vec<&str> = vec![""];
        quals.extend(
            qualifiers
                .iter()
                .map(|q| q.as_str())
                .filter(|q| !q.is_empty()),
        );
        let mut stmts = Vec::with_capacity(quals.len());
        for q in quals {
            stmts.push(stmt(
                &self.db,
                "INSERT OR IGNORE INTO name_index(name_norm, qualifier, canonical_id, source)
                 VALUES (?1, ?2, ?3, NULLIF(?4, ''))",
                &[
                    bind_str(name_norm),
                    bind_str(q),
                    bind_str(canonical_id),
                    bind_opt(source),
                ],
            )?);
        }
        batch(&self.db, stmts).await
    }

    async fn name_entities(&self, name_norm: &str) -> Result<Vec<(String, Vec<String>)>> {
        if name_norm.is_empty() {
            return Ok(Vec::new());
        }
        let s = stmt(
            &self.db,
            "SELECT canonical_id, qualifier FROM name_index WHERE name_norm = ?1",
            &[bind_str(name_norm)],
        )?;
        let fetched = rows::<NameQualRow>(s).await?;

        use std::collections::BTreeMap;
        let mut by_cid: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for r in fetched {
            let entry = by_cid.entry(r.canonical_id).or_default();
            if !r.qualifier.is_empty() && !entry.contains(&r.qualifier) {
                entry.push(r.qualifier);
            }
        }
        Ok(by_cid
            .into_iter()
            .map(|(cid, mut quals)| {
                quals.sort();
                (cid, quals)
            })
            .collect())
    }

    async fn record_name_cardinality(
        &self,
        name_norm: &str,
        qualifiers: &[String],
        candidates: &[NameCandidate],
    ) -> Result<()> {
        if name_norm.is_empty() {
            return Ok(());
        }
        let blob = serde_json::to_string(
            &candidates
                .iter()
                .map(|(cid, anchor, name)| {
                    serde_json::json!({
                        "canonical_id": cid,
                        "anchor": anchor,
                        "name": name,
                    })
                })
                .collect::<Vec<_>>(),
        )?;
        run(stmt(
            &self.db,
            "INSERT INTO name_cardinality(name_norm, qualifier_set, candidates, status, canonical_id)
             VALUES (?1, ?2, ?3, 'ambiguous', NULL)
             ON CONFLICT(name_norm, qualifier_set) DO UPDATE SET
                 candidates = excluded.candidates,
                 status = excluded.status,
                 canonical_id = excluded.canonical_id",
            &[
                bind_str(name_norm),
                bind_str(&qualifier_set_key(qualifiers)),
                bind_str(&blob),
            ],
        )?)
        .await
    }

    async fn record_name_unique(
        &self,
        name_norm: &str,
        qualifiers: &[String],
        canonical_id: &str,
    ) -> Result<()> {
        if name_norm.is_empty() {
            return Ok(());
        }
        run(stmt(
            &self.db,
            "INSERT INTO name_cardinality(name_norm, qualifier_set, candidates, status, canonical_id)
             VALUES (?1, ?2, '[]', 'unique', ?3)
             ON CONFLICT(name_norm, qualifier_set) DO UPDATE SET
                 candidates = excluded.candidates,
                 status = excluded.status,
                 canonical_id = excluded.canonical_id",
            &[
                bind_str(name_norm),
                bind_str(&qualifier_set_key(qualifiers)),
                bind_str(canonical_id),
            ],
        )?)
        .await
    }

    async fn name_cardinality_raw(
        &self,
        name_norm: &str,
        qualifiers: &[String],
    ) -> Result<Option<NameCardinality>> {
        if name_norm.is_empty() {
            return Ok(None);
        }
        let s = stmt(
            &self.db,
            "SELECT candidates, status, canonical_id \
             FROM name_cardinality WHERE name_norm = ?1 AND qualifier_set = ?2",
            &[
                bind_str(name_norm),
                bind_str(&qualifier_set_key(qualifiers)),
            ],
        )?;
        let row = match first_row::<CardinalityRow>(s).await? {
            Some(r) => r,
            None => return Ok(None),
        };
        // A NULL status is a legacy (M2-early) row — those were only ever
        // ambiguous. The liveness check lives in `GraphStore::name_cardinality`.
        if row.status.as_deref() == Some("unique") {
            return Ok(row.canonical_id.map(NameCardinality::Unique));
        }
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&row.candidates)?;
        Ok(Some(NameCardinality::Ambiguous(
            parsed
                .into_iter()
                .map(|v| {
                    (
                        v.get("canonical_id")
                            .and_then(|x| x.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        v.get("anchor")
                            .and_then(|x| x.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        v.get("name").and_then(|x| x.as_str()).map(|s| s.to_string()),
                    )
                })
                .collect(),
        )))
    }

    // --- union ----------------------------------------------------------

    /// The seven re-point/delete statements as ONE atomic batch.
    ///
    /// **Statement order is load-bearing.** With foreign keys enforced, the loser's
    /// `entities` row must be deleted LAST — every reference to it has to be
    /// re-pointed away first, or the commit trips a dangling-reference check. This
    /// mirrors `SqliteStore::merge_into` exactly.
    async fn merge_into(&self, winner: &str, loser: &str) -> Result<()> {
        if winner == loser {
            return Ok(());
        }
        let w = bind_str(winner);
        let l = bind_str(loser);
        let pair = [w.clone(), l.clone()];
        let solo = [l.clone()];

        let stmts = vec![
            stmt(
                &self.db,
                "UPDATE nodes SET canonical_id = ?1 WHERE canonical_id = ?2",
                &pair,
            )?,
            // Re-point phone edges, ignoring rows that would collide with an
            // existing (phone,winner) edge, then delete any leftovers.
            stmt(
                &self.db,
                "UPDATE OR IGNORE phone_edges SET canonical_id = ?1 WHERE canonical_id = ?2",
                &pair,
            )?,
            stmt(
                &self.db,
                "DELETE FROM phone_edges WHERE canonical_id = ?1",
                &solo,
            )?,
            // Re-point local name-index rows too (ignore rows that would collide).
            stmt(
                &self.db,
                "UPDATE OR IGNORE name_index SET canonical_id = ?1 WHERE canonical_id = ?2",
                &pair,
            )?,
            stmt(
                &self.db,
                "DELETE FROM name_index WHERE canonical_id = ?1",
                &solo,
            )?,
            // Re-point unique cardinality rows that named the loser (ambiguous rows
            // carry a NULL canonical_id, so this only touches unique memory).
            stmt(
                &self.db,
                "UPDATE name_cardinality SET canonical_id = ?1 WHERE canonical_id = ?2",
                &pair,
            )?,
            // LAST — see the note above.
            stmt(
                &self.db,
                "DELETE FROM entities WHERE canonical_id = ?1",
                &solo,
            )?,
        ];
        batch(&self.db, stmts).await
    }

    // --- split ----------------------------------------------------------

    /// Read membership, compute both post-split anchors in Rust, then write once.
    ///
    /// `SqliteStore` re-anchors *inside* its transaction, reading its own
    /// uncommitted re-points. A `batch()` is a fixed list of statements with no
    /// interleaved reads, so that shape is unavailable. It is also unnecessary: the
    /// set of moving keys is an input, so post-split membership on both sides is
    /// pure set arithmetic over the pre-split membership.
    ///
    /// The two reads are outside the batch, so a concurrent writer could in
    /// principle change membership between the read and the write. That is the same
    /// exposure every other read-then-write path here has, and the reconcile worker
    /// is the only writer.
    async fn apply_split(
        &self,
        new_cid: &str,
        new_anchor: &str,
        detached_keys: &[String],
        src_cid: &str,
    ) -> Result<()> {
        // A batch cannot be chunked without losing atomicity, so bound the size
        // rather than silently exceeding D1's statement/parameter limits.
        const MAX_DETACHED: usize = 80;
        if detached_keys.len() > MAX_DETACHED {
            bail!(
                "apply_split: {} detached keys exceeds the {MAX_DETACHED}-key limit for a \
                 single atomic D1 batch",
                detached_keys.len()
            );
        }

        // --- reads (before the batch) ---
        let src_entity = self.get_entity(src_cid).await?;
        let all_members = self.member_rows(src_cid).await?;

        // --- pure Rust: derive both post-move memberships ---
        let detached: std::collections::HashSet<&str> =
            detached_keys.iter().map(|s| s.as_str()).collect();
        let split_side = |keep_detached: bool| -> Vec<ExternalId> {
            super::typed_members(
                all_members
                    .iter()
                    .filter(|m| detached.contains(m.key.as_str()) == keep_detached)
                    .map(|m| m.key.as_str()),
            )
        };
        let src_after = split_side(false);
        let new_after = split_side(true);

        // The source keeps its existing anchor when no strong key remains; the new
        // side confirms its own from the keys it received.
        let src_anchor_final = src_entity
            .as_ref()
            .map(|e| anchor::recompute_anchor(&src_after, &e.anchor));
        let new_anchor_final = anchor::recompute_anchor(&new_after, new_anchor);

        // --- one atomic batch ---
        let mut stmts = Vec::with_capacity(detached_keys.len() + 5);
        stmts.push(stmt(
            &self.db,
            "INSERT OR IGNORE INTO entities(canonical_id, anchor, entity_type, name)
             VALUES (?1, ?2, NULL, NULL)",
            &[bind_str(new_cid), bind_str(&new_anchor_final)],
        )?);
        for key in detached_keys {
            stmts.push(stmt(
                &self.db,
                "INSERT INTO nodes(key, canonical_id, source) VALUES (?1, ?2, NULL)
                 ON CONFLICT(key) DO UPDATE SET
                     canonical_id = excluded.canonical_id,
                     source = COALESCE(excluded.source, nodes.source)",
                &[bind_str(key), bind_str(new_cid)],
            )?);
        }
        // The keys named the source before this split, so its cached name rows may
        // now point at an identity that moved — invalidate them.
        stmts.push(stmt(
            &self.db,
            "DELETE FROM name_index WHERE canonical_id = ?1",
            &[bind_str(src_cid)],
        )?);
        stmts.push(stmt(
            &self.db,
            "DELETE FROM name_cardinality WHERE canonical_id = ?1",
            &[bind_str(src_cid)],
        )?);
        // Re-anchor both sides. Unconditional UPDATEs (the rusqlite path skips the
        // write when the anchor is unchanged); a no-op UPDATE is cheaper than the
        // round trip needed to decide whether to skip it.
        if let Some(a) = &src_anchor_final {
            stmts.push(stmt(
                &self.db,
                "UPDATE entities SET anchor = ?2 WHERE canonical_id = ?1",
                &[bind_str(src_cid), bind_str(a)],
            )?);
        }
        stmts.push(stmt(
            &self.db,
            "UPDATE entities SET anchor = ?2 WHERE canonical_id = ?1",
            &[bind_str(new_cid), bind_str(&new_anchor_final)],
        )?);

        batch(&self.db, stmts).await
    }

    // --- stats (miss-rate instrumentation) ------------------------------

    async fn record_resolution(
        &self,
        status_tag: &str,
        reason_tag: &str,
        matched_via: Option<&str>,
        confidence: f32,
        input_desc: Option<&str>,
    ) -> Result<()> {
        run(stmt(
            &self.db,
            "INSERT INTO resolutions(status_tag, reason_tag, matched_via, confidence, input_desc)
             VALUES (?1, ?2, NULLIF(?3, ''), ?4, NULLIF(?5, ''))",
            &[
                bind_str(status_tag),
                bind_str(reason_tag),
                bind_opt(matched_via),
                bind_f64(confidence as f64),
                bind_opt(input_desc),
            ],
        )?)
        .await
    }

    async fn stats(&self) -> Result<StatsReport> {
        // Three independent reads; `StatsReport::from_counts` does the bucketing so
        // the report is assembled identically to the SQLite backend.
        let by_reason: Vec<(String, usize)> = rows::<ReasonCountRow>(stmt(
            &self.db,
            "SELECT reason_tag, COUNT(*) AS n FROM resolutions GROUP BY reason_tag",
            &[],
        )?)
        .await?
        .into_iter()
        .map(|r| (r.reason_tag, r.n as usize))
        .collect();

        let entities = first_row::<CountRow>(stmt(
            &self.db,
            "SELECT COUNT(*) AS n FROM entities",
            &[],
        )?)
        .await?
        .map_or(0, |r| r.n as usize);

        let edges = first_row::<CountRow>(stmt(
            &self.db,
            "SELECT (SELECT COUNT(*) FROM nodes) + (SELECT COUNT(*) FROM phone_edges) AS n",
            &[],
        )?)
        .await?
        .map_or(0, |r| r.n as usize);

        Ok(StatsReport::from_counts(by_reason, entities, edges))
    }
}
