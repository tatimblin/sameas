//! [`SqliteStore`] — the `rusqlite` backend. Used by the CLI and every unit test,
//! and the only backend that owns schema creation/migration.
//!
//! Its [`GraphStore`] methods are `async fn` wrapping synchronous `rusqlite` calls:
//! they contain no `.await` and never yield. The async signature exists purely so
//! one trait can also cover D1 (see [`super`]); the SQL and its semantics are
//! unchanged from the pre-trait `Graph`.

use anyhow::Result;
use async_trait::async_trait;
use rusqlite::{params, Connection, OptionalExtension};

use super::{
    qualifier_set_key, typed_members, EntityRow, GraphStore, MemberRow, NameCandidate,
    NameCardinality, StatsReport,
};
use crate::model::ExternalId;

pub struct SqliteStore {
    conn: Connection,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS entities (
    canonical_id TEXT PRIMARY KEY,
    anchor       TEXT NOT NULL,
    entity_type  TEXT,
    name         TEXT
);
CREATE TABLE IF NOT EXISTS nodes (
    key          TEXT PRIMARY KEY,
    canonical_id TEXT NOT NULL,
    source       TEXT,
    FOREIGN KEY(canonical_id) REFERENCES entities(canonical_id)
);
CREATE INDEX IF NOT EXISTS idx_nodes_canonical ON nodes(canonical_id);
CREATE TABLE IF NOT EXISTS phone_edges (
    phone_key    TEXT NOT NULL,
    canonical_id TEXT NOT NULL,
    source       TEXT,
    PRIMARY KEY (phone_key, canonical_id),
    FOREIGN KEY(canonical_id) REFERENCES entities(canonical_id)
);
CREATE INDEX IF NOT EXISTS idx_phone_canonical ON phone_edges(canonical_id);
CREATE TABLE IF NOT EXISTS name_index (
    name_norm    TEXT NOT NULL,
    qualifier    TEXT NOT NULL,
    canonical_id TEXT NOT NULL,
    source       TEXT,
    PRIMARY KEY (name_norm, qualifier, canonical_id),
    FOREIGN KEY(canonical_id) REFERENCES entities(canonical_id)
);
CREATE INDEX IF NOT EXISTS idx_name_index_name ON name_index(name_norm);
CREATE TABLE IF NOT EXISTS name_cardinality (
    name_norm     TEXT NOT NULL,
    qualifier_set TEXT NOT NULL,
    candidates    TEXT NOT NULL,
    status        TEXT NOT NULL DEFAULT 'ambiguous',
    canonical_id  TEXT,
    PRIMARY KEY (name_norm, qualifier_set)
);
-- Append-only log of resolution outcomes, one row per user-facing resolve.
-- The evidence gate for the optional fuzzy phase: `sameas stats` aggregates
-- these into an exact/hub/miss breakdown + a headline miss rate. rowid gives
-- insertion order; no wall-clock is stored (kept deterministic + IDs-only).
CREATE TABLE IF NOT EXISTS resolutions (
    status_tag   TEXT NOT NULL,
    reason_tag   TEXT NOT NULL,
    matched_via  TEXT,
    confidence   REAL NOT NULL,
    input_desc   TEXT
);
"#;

impl SqliteStore {
    pub fn open(path: &str) -> Result<SqliteStore> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<SqliteStore> {
        let conn = Connection::open_in_memory()?;
        Self::init(conn)
    }

    fn init(conn: Connection) -> Result<SqliteStore> {
        // Enforce the declared FKs (nodes/phone_edges/name_index → entities).
        // SQLite ignores FK declarations unless this pragma is on, so without it
        // the constraints are documentation only. Correction ops now mutate
        // inside transactions where the FK check runs at commit, so every op must
        // leave no dangling references (create the entity row before attaching
        // nodes; re-point a loser's nodes away before deleting its entity row).
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        conn.execute_batch(SCHEMA)?;
        Self::migrate(&conn)?;
        Ok(SqliteStore { conn })
    }

    /// Idempotent, forward-only migrations. New DBs are already born from
    /// `SCHEMA`; this only patches pre-M2 DBs missing the `source` column.
    /// Guarding on `PRAGMA table_info` avoids "duplicate column" errors.
    fn migrate(conn: &Connection) -> Result<()> {
        for table in ["nodes", "phone_edges"] {
            if !Self::has_column(conn, table, "source")? {
                conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN source TEXT;"))?;
            }
        }
        // A pre-existing (M2-early) `name_cardinality` recorded only the ambiguous
        // case, with no uniqueness column. Add `status` (defaulting existing rows
        // to 'ambiguous', which is all they ever were) and a nullable
        // `canonical_id` (populated only for 'unique' rows).
        if !Self::has_column(conn, "name_cardinality", "status")? {
            conn.execute_batch(
                "ALTER TABLE name_cardinality ADD COLUMN status TEXT NOT NULL DEFAULT 'ambiguous';",
            )?;
        }
        if !Self::has_column(conn, "name_cardinality", "canonical_id")? {
            conn.execute_batch("ALTER TABLE name_cardinality ADD COLUMN canonical_id TEXT;")?;
        }
        Ok(())
    }

    fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(1)?;
            if name == column {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The non-empty qualifier tokens an entity was indexed under for `name_norm`.
    /// (The always-present `""` row is excluded — it is the bare-name marker, not
    /// a facet.) Used by [`find_by_name`](Self::find_by_name) for the
    /// qualifier-subset test.
    fn indexed_qualifiers(&self, name_norm: &str, canonical_id: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT qualifier FROM name_index \
             WHERE name_norm = ?1 AND canonical_id = ?2 AND qualifier != ''",
        )?;
        let rows = stmt
            .query_map(params![name_norm, canonical_id], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // --- CLI-only correction primitives ---------------------------------
    //
    // These have no production callers on the resolve path and are not part of
    // `GraphStore`: they exist for the CLI's correction surface (and its tests),
    // so a network backend owes no implementation.

    /// Find canonical ids for a normalized name, optionally narrowed by qualifier
    /// tokens. With no qualifiers, matches on name alone (bare-name semantics: a
    /// single hit is confident, several are ambiguous). With qualifiers, a
    /// candidate matches only when the qualifier set it was **indexed under** is a
    /// SUBSET of the query's qualifiers — every facet the entity was indexed with
    /// must also be present in the query. This is the "not rigid, but not
    /// over-broad" rule: indexing by `{oakland}` still hits a later `{oakland,ca}`
    /// query (subset holds), a bare-indexed entity (`{}`) hits any qualified query
    /// (empty ⊆ anything), but an entity indexed under `{boston,us}` does NOT match
    /// a `{seattle,us}` query (they share only the coarse `us`).
    /// Returns distinct canonical ids, sorted for determinism.
    pub fn find_by_name(&self, name_norm: &str, qualifiers: &[String]) -> Result<Vec<String>> {
        if name_norm.is_empty() {
            return Ok(Vec::new());
        }
        let quals: Vec<&str> = qualifiers
            .iter()
            .map(|q| q.as_str())
            .filter(|q| !q.is_empty())
            .collect();

        // Every entity indexed under this name carries a `""` row, so a plain
        // name match enumerates all candidates.
        let mut candidates: Vec<String> = {
            let mut stmt = self
                .conn
                .prepare("SELECT DISTINCT canonical_id FROM name_index WHERE name_norm = ?1")?;
            let rows = stmt
                .query_map(params![name_norm], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };

        // Bare query: name-only semantics — return all same-name entities.
        // Qualified query: keep only entities whose indexed (non-empty) qualifier
        // set is a subset of the query's qualifiers.
        if !quals.is_empty() {
            let mut kept = Vec::new();
            for cid in candidates {
                let indexed = self.indexed_qualifiers(name_norm, &cid)?;
                if indexed.iter().all(|q| quals.contains(&q.as_str())) {
                    kept.push(cid);
                }
            }
            candidates = kept;
        }
        candidates.sort();
        candidates.dedup();
        Ok(candidates)
    }

    /// Drop every cached name row (`name_index` + unique `name_cardinality`) that
    /// names `canonical_id`. Unlike `nodes`, the name caches are keyed by
    /// `(name, canonical_id)` — NOT by strong key — so once keys are peeled onto a
    /// new cid there is no reliable mapping from a detached key back to which
    /// cached name rows belong to it. Rather than mispoint those rows (which would
    /// let a name query confidently return the SOURCE cid for an identity that has
    /// moved, and leave the new entity unfindable by name — a false-merge-safety
    /// break), we INVALIDATE them: both entities stay findable by their strong keys
    /// and re-resolvable by name via the hub (the cache self-heals). Ambiguous
    /// `name_cardinality` rows carry a NULL canonical_id, so only unique rows are
    /// touched — exactly the ones that could mislead.
    pub fn clear_name_caches(&self, canonical_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM name_index WHERE canonical_id = ?1",
            params![canonical_id],
        )?;
        self.conn.execute(
            "DELETE FROM name_cardinality WHERE canonical_id = ?1",
            params![canonical_id],
        )?;
        Ok(())
    }

    /// Move one phone corroborator edge from `from` to `to`. Phone is outside the
    /// union-find (a phone may edge to several entities), so we move only the
    /// specific `(phone_key, from)` edge; `OR IGNORE` drops it if `to` already
    /// carries that phone.
    pub fn repoint_phone_edge(
        &self,
        phone_key: &str,
        from_canonical_id: &str,
        to_canonical_id: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE OR IGNORE phone_edges SET canonical_id = ?1 \
             WHERE phone_key = ?2 AND canonical_id = ?3",
            params![to_canonical_id, phone_key, from_canonical_id],
        )?;
        // If the move was ignored (target already had the phone), drop the leftover.
        self.conn.execute(
            "DELETE FROM phone_edges WHERE phone_key = ?1 AND canonical_id = ?2",
            params![phone_key, from_canonical_id],
        )?;
        Ok(())
    }

    /// Delete an entity row that has no remaining members. Used when a split (or
    /// other correction) empties an entity. No-op if members remain.
    pub async fn delete_entity_if_empty(&self, canonical_id: &str) -> Result<bool> {
        if !self.member_keys(canonical_id).await?.is_empty() {
            return Ok(false);
        }
        self.conn.execute(
            "DELETE FROM entities WHERE canonical_id = ?1",
            params![canonical_id],
        )?;
        Ok(true)
    }

    /// Run raw SQL against the connection. Test-only: lets a test manufacture a
    /// state the public API deliberately can't (e.g. deleting an entity row out
    /// from under its members to simulate a stale cache).
    #[cfg(test)]
    pub(crate) fn exec_raw(&self, sql: &str) -> Result<()> {
        self.conn.execute_batch(sql)?;
        Ok(())
    }

    /// `get_entity` against an explicit connection handle (a live transaction), so
    /// `apply_split` can read its own uncommitted writes.
    fn get_entity_tx(conn: &Connection, canonical_id: &str) -> Result<Option<EntityRow>> {
        Ok(conn
            .query_row(
                "SELECT canonical_id, anchor, entity_type, name FROM entities WHERE canonical_id = ?1",
                params![canonical_id],
                |row| {
                    Ok(EntityRow {
                        canonical_id: row.get(0)?,
                        anchor: row.get(1)?,
                        entity_type: row.get(2)?,
                        name: row.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    /// Members (typed, sorted, deduped) against an explicit connection handle, so
    /// `apply_split` re-anchors over the uncommitted post-move membership.
    fn members_tx(conn: &Connection, canonical_id: &str) -> Result<Vec<ExternalId>> {
        let mut keys: Vec<String> = Vec::new();
        {
            let mut stmt = conn.prepare("SELECT key FROM nodes WHERE canonical_id = ?1")?;
            let rows = stmt
                .query_map(params![canonical_id], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            keys.extend(rows);
        }
        {
            let mut stmt =
                conn.prepare("SELECT phone_key FROM phone_edges WHERE canonical_id = ?1")?;
            let rows = stmt
                .query_map(params![canonical_id], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            keys.extend(rows);
        }
        Ok(typed_members(keys.iter().map(|k| k.as_str())))
    }
}

#[async_trait(?Send)]
impl GraphStore for SqliteStore {
    // --- union-find over strong keys ------------------------------------

    async fn find(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT canonical_id FROM nodes WHERE key = ?1",
                params![key],
                |row| row.get::<_, String>(0),
            )
            .optional()?)
    }

    async fn attach_with_source(
        &self,
        key: &str,
        canonical_id: &str,
        source: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO nodes(key, canonical_id, source) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET
                 canonical_id = excluded.canonical_id,
                 source = COALESCE(excluded.source, nodes.source)",
            params![key, canonical_id, source],
        )?;
        Ok(())
    }

    // --- phone edges (corroborator, outside union-find) -----------------

    async fn add_phone_edge_with_source(
        &self,
        phone_key: &str,
        canonical_id: &str,
        source: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO phone_edges(phone_key, canonical_id, source) VALUES (?1, ?2, ?3)",
            params![phone_key, canonical_id, source],
        )?;
        Ok(())
    }

    async fn find_phone(&self, phone_key: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT canonical_id FROM phone_edges WHERE phone_key = ?1 ORDER BY canonical_id",
        )?;
        let rows = stmt
            .query_map(params![phone_key], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // --- entities -------------------------------------------------------

    async fn create_entity(
        &self,
        canonical_id: &str,
        anchor: &str,
        entity_type: Option<&str>,
        name: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO entities(canonical_id, anchor, entity_type, name)
             VALUES (?1, ?2, ?3, ?4)",
            params![canonical_id, anchor, entity_type, name],
        )?;
        Ok(())
    }

    async fn get_entity(&self, canonical_id: &str) -> Result<Option<EntityRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT canonical_id, anchor, entity_type, name FROM entities WHERE canonical_id = ?1",
                params![canonical_id],
                |row| {
                    Ok(EntityRow {
                        canonical_id: row.get(0)?,
                        anchor: row.get(1)?,
                        entity_type: row.get(2)?,
                        name: row.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    async fn set_anchor(&self, canonical_id: &str, anchor: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE entities SET anchor = ?2 WHERE canonical_id = ?1",
            params![canonical_id, anchor],
        )?;
        Ok(())
    }

    async fn enrich_entity(
        &self,
        canonical_id: &str,
        entity_type: Option<&str>,
        name: Option<&str>,
    ) -> Result<()> {
        if let Some(t) = entity_type {
            self.conn.execute(
                "UPDATE entities SET entity_type = ?2 WHERE canonical_id = ?1 AND entity_type IS NULL",
                params![canonical_id, t],
            )?;
        }
        if let Some(n) = name {
            self.conn.execute(
                "UPDATE entities SET name = ?2 WHERE canonical_id = ?1 AND name IS NULL",
                params![canonical_id, n],
            )?;
        }
        Ok(())
    }

    async fn strong_key_count(&self, canonical_id: &str) -> Result<usize> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE canonical_id = ?1",
            params![canonical_id],
            |row| row.get(0),
        )?;
        Ok(n as usize)
    }

    // --- members --------------------------------------------------------

    async fn member_rows(&self, canonical_id: &str) -> Result<Vec<MemberRow>> {
        let mut rows: Vec<MemberRow> = Vec::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT key, source FROM nodes WHERE canonical_id = ?1")?;
            let r = stmt
                .query_map(params![canonical_id], |row| {
                    Ok(MemberRow {
                        key: row.get(0)?,
                        source: row.get(1)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows.extend(r);
        }
        {
            let mut stmt = self
                .conn
                .prepare("SELECT phone_key, source FROM phone_edges WHERE canonical_id = ?1")?;
            let r = stmt
                .query_map(params![canonical_id], |row| {
                    Ok(MemberRow {
                        key: row.get(0)?,
                        source: row.get(1)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows.extend(r);
        }
        Ok(rows)
    }

    // --- local name index -----------------------------------------------

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
        for q in quals {
            self.conn.execute(
                "INSERT OR IGNORE INTO name_index(name_norm, qualifier, canonical_id, source)
                 VALUES (?1, ?2, ?3, ?4)",
                params![name_norm, q, canonical_id, source],
            )?;
        }
        Ok(())
    }

    async fn name_entities(&self, name_norm: &str) -> Result<Vec<(String, Vec<String>)>> {
        if name_norm.is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt = self
            .conn
            .prepare("SELECT canonical_id, qualifier FROM name_index WHERE name_norm = ?1")?;
        let rows = stmt
            .query_map(params![name_norm], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        use std::collections::BTreeMap;
        let mut by_cid: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (cid, qual) in rows {
            let entry = by_cid.entry(cid).or_default();
            if !qual.is_empty() && !entry.contains(&qual) {
                entry.push(qual);
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
        self.conn.execute(
            "INSERT INTO name_cardinality(name_norm, qualifier_set, candidates, status, canonical_id)
             VALUES (?1, ?2, ?3, 'ambiguous', NULL)
             ON CONFLICT(name_norm, qualifier_set) DO UPDATE SET
                 candidates = excluded.candidates,
                 status = excluded.status,
                 canonical_id = excluded.canonical_id",
            params![name_norm, qualifier_set_key(qualifiers), blob],
        )?;
        Ok(())
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
        self.conn.execute(
            "INSERT INTO name_cardinality(name_norm, qualifier_set, candidates, status, canonical_id)
             VALUES (?1, ?2, '[]', 'unique', ?3)
             ON CONFLICT(name_norm, qualifier_set) DO UPDATE SET
                 candidates = excluded.candidates,
                 status = excluded.status,
                 canonical_id = excluded.canonical_id",
            params![name_norm, qualifier_set_key(qualifiers), canonical_id],
        )?;
        Ok(())
    }

    async fn name_cardinality_raw(
        &self,
        name_norm: &str,
        qualifiers: &[String],
    ) -> Result<Option<NameCardinality>> {
        if name_norm.is_empty() {
            return Ok(None);
        }
        let row: Option<(String, Option<String>, Option<String>)> = self
            .conn
            .query_row(
                "SELECT candidates, status, canonical_id \
                 FROM name_cardinality WHERE name_norm = ?1 AND qualifier_set = ?2",
                params![name_norm, qualifier_set_key(qualifiers)],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?;
        let (blob, status, canonical_id) = match row {
            Some(r) => r,
            None => return Ok(None),
        };
        // A NULL status is a legacy (M2-early) row — those were only ever
        // ambiguous.
        if status.as_deref() == Some("unique") {
            // The liveness check lives in `GraphStore::name_cardinality`; a
            // malformed row with no id still reads as absent here.
            return Ok(canonical_id.map(NameCardinality::Unique));
        }
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&blob)?;
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

    async fn merge_into(&self, winner: &str, loser: &str) -> Result<()> {
        if winner == loser {
            return Ok(());
        }
        // One transaction: the ~5 re-points + the loser-row delete are a single
        // unit, so a mid-op failure can never leave the graph half-merged (and,
        // with FKs enforced, the loser row is only dropped after its nodes/edges
        // have been re-pointed away, so no dangling reference exists at commit).
        // `unchecked_transaction` takes the shared `&self` borrow.
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE nodes SET canonical_id = ?1 WHERE canonical_id = ?2",
            params![winner, loser],
        )?;
        // Re-point phone edges, ignoring rows that would collide with an
        // existing (phone,winner) edge, then delete any leftovers.
        tx.execute(
            "UPDATE OR IGNORE phone_edges SET canonical_id = ?1 WHERE canonical_id = ?2",
            params![winner, loser],
        )?;
        tx.execute(
            "DELETE FROM phone_edges WHERE canonical_id = ?1",
            params![loser],
        )?;
        // Re-point local name-index rows too (ignore rows that would collide).
        tx.execute(
            "UPDATE OR IGNORE name_index SET canonical_id = ?1 WHERE canonical_id = ?2",
            params![winner, loser],
        )?;
        tx.execute(
            "DELETE FROM name_index WHERE canonical_id = ?1",
            params![loser],
        )?;
        // Re-point unique cardinality rows that named the loser (ambiguous rows
        // carry a NULL canonical_id, so this only touches unique memory).
        tx.execute(
            "UPDATE name_cardinality SET canonical_id = ?1 WHERE canonical_id = ?2",
            params![winner, loser],
        )?;
        tx.execute(
            "DELETE FROM entities WHERE canonical_id = ?1",
            params![loser],
        )?;
        tx.commit()?;
        Ok(())
    }

    // --- split ----------------------------------------------------------

    async fn apply_split(
        &self,
        new_cid: &str,
        new_anchor: &str,
        detached_keys: &[String],
        src_cid: &str,
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO entities(canonical_id, anchor, entity_type, name)
             VALUES (?1, ?2, NULL, NULL)",
            params![new_cid, new_anchor],
        )?;
        for key in detached_keys {
            // Re-point exactly one node row (mirrors `repoint_key`, but on `tx`).
            tx.execute(
                "INSERT INTO nodes(key, canonical_id, source) VALUES (?1, ?2, NULL)
                 ON CONFLICT(key) DO UPDATE SET
                     canonical_id = excluded.canonical_id,
                     source = COALESCE(excluded.source, nodes.source)",
                params![key, new_cid],
            )?;
        }
        // The keys named the source before this split, so its cached name rows
        // may now point at an identity that moved — invalidate them.
        tx.execute(
            "DELETE FROM name_index WHERE canonical_id = ?1",
            params![src_cid],
        )?;
        tx.execute(
            "DELETE FROM name_cardinality WHERE canonical_id = ?1",
            params![src_cid],
        )?;
        // Re-anchor both sides over their (moved) memberships. `merge_into` /
        // `split` primitives never touch the anchor, so we must — the source may
        // drop to a weaker/synthetic anchor and the new side confirms its own.
        // Reads on this connection see the uncommitted re-points.
        for cid in [src_cid, new_cid] {
            if let Some(entity) = Self::get_entity_tx(&tx, cid)? {
                let members = Self::members_tx(&tx, cid)?;
                let anchor = crate::anchor::recompute_anchor(&members, &entity.anchor);
                if anchor != entity.anchor {
                    tx.execute(
                        "UPDATE entities SET anchor = ?2 WHERE canonical_id = ?1",
                        params![cid, anchor],
                    )?;
                }
            }
        }
        tx.commit()?;
        Ok(())
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
        self.conn.execute(
            "INSERT INTO resolutions(status_tag, reason_tag, matched_via, confidence, input_desc)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                status_tag,
                reason_tag,
                matched_via,
                confidence as f64,
                input_desc
            ],
        )?;
        Ok(())
    }

    async fn stats(&self) -> Result<StatsReport> {
        let by_reason: Vec<(String, usize)> = {
            let mut stmt = self
                .conn
                .prepare("SELECT reason_tag, COUNT(*) FROM resolutions GROUP BY reason_tag")?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as usize))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };

        let entities: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM entities", [], |row| row.get(0))?;
        let edges: i64 = self.conn.query_row(
            "SELECT (SELECT COUNT(*) FROM nodes) + (SELECT COUNT(*) FROM phone_edges)",
            [],
            |row| row.get(0),
        )?;

        Ok(StatsReport::from_counts(
            by_reason,
            entities as usize,
            edges as usize,
        ))
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn union_find_transitivity_through_sqlite() {
        let g = SqliteStore::open_in_memory().unwrap();
        g.create_entity("cx_a", "domain:a.com", None, None).await.unwrap();
        // attach a <-> b (same canonical)
        g.attach("domain:a.com", "cx_a").await.unwrap();
        g.attach("domain:b.com", "cx_a").await.unwrap();
        // b <-> c (c joins the same canonical)
        g.attach("domain:c.com", "cx_a").await.unwrap();

        assert_eq!(g.find("domain:a.com").await.unwrap().as_deref(), Some("cx_a"));
        assert_eq!(
            g.find("domain:a.com").await.unwrap(),
            g.find("domain:c.com").await.unwrap()
        );
    }

    #[tokio::test]
    async fn merge_repoints_members() {
        let g = SqliteStore::open_in_memory().unwrap();
        g.create_entity("cx_1", "domain:a.com", None, None).await.unwrap();
        g.create_entity("cx_2", "wikidata:Q1", None, None).await.unwrap();
        g.attach("domain:a.com", "cx_1").await.unwrap();
        g.attach("wikidata:Q1", "cx_2").await.unwrap();

        g.merge_into("cx_2", "cx_1").await.unwrap();

        assert_eq!(g.find("domain:a.com").await.unwrap().as_deref(), Some("cx_2"));
        assert!(g.get_entity("cx_1").await.unwrap().is_none());
        assert_eq!(g.members("cx_2").await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn migrates_pre_m2_db_without_source_column() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db");
        let path = path.to_str().unwrap();
        // Simulate a pre-M2 DB: nodes/phone_edges with NO `source` column.
        {
            let conn = Connection::open(path).unwrap();
            conn.execute_batch(
                "CREATE TABLE entities (canonical_id TEXT PRIMARY KEY, anchor TEXT NOT NULL, entity_type TEXT, name TEXT);
                 CREATE TABLE nodes (key TEXT PRIMARY KEY, canonical_id TEXT NOT NULL);
                 CREATE TABLE phone_edges (phone_key TEXT NOT NULL, canonical_id TEXT NOT NULL, PRIMARY KEY (phone_key, canonical_id));
                 INSERT INTO entities(canonical_id, anchor) VALUES ('cx_x', 'domain:a.com');
                 INSERT INTO nodes(key, canonical_id) VALUES ('domain:a.com', 'cx_x');",
            )
            .unwrap();
        }
        // Opening through SqliteStore::open must migrate in the `source` column and
        // preserve the existing rows.
        let g = SqliteStore::open(path).unwrap();
        assert_eq!(g.find("domain:a.com").await.unwrap().as_deref(), Some("cx_x"));
        // A source-aware write now works against the migrated table.
        g.attach_with_source("wikidata:Q1", "cx_x", Some("wikidata")).await
            .unwrap();
        let sources = g.member_sources("cx_x").await.unwrap();
        assert!(sources
            .iter()
            .any(|(k, s)| k == "wikidata:Q1" && s.as_deref() == Some("wikidata")));
        // The pre-existing edge has a NULL (unknown) source.
        assert!(sources
            .iter()
            .any(|(k, s)| k == "domain:a.com" && s.is_none()));
    }

    #[tokio::test]
    async fn name_index_matches_name_plus_qualifier() {
        let g = SqliteStore::open_in_memory().unwrap();
        g.create_entity("cx_sf", "google_place_id:SF", None, Some("Basecamp")).await
            .unwrap();
        g.create_entity("cx_ny", "google_place_id:NY", None, Some("Basecamp")).await
            .unwrap();
        g.index_name("basecamp", &["san francisco".into()], "cx_sf", Some("t")).await
            .unwrap();
        g.index_name("basecamp", &["new york".into()], "cx_ny", Some("t")).await
            .unwrap();

        // name + qualifier → the one matching entity.
        assert_eq!(
            g.find_by_name("basecamp", &["san francisco".into()]).unwrap(),
            vec!["cx_sf".to_string()]
        );
        // Subset holds: indexed {san francisco} ⊆ query {san francisco, ca}, so a
        // query with an EXTRA (finer) qualifier still hits the indexed entity.
        assert_eq!(
            g.find_by_name("basecamp", &["san francisco".into(), "ca".into()])
                .unwrap(),
            vec!["cx_sf".to_string()]
        );
        // Bare name matches BOTH → caller treats as ambiguous.
        assert_eq!(
            g.find_by_name("basecamp", &[]).unwrap(),
            vec!["cx_ny".to_string(), "cx_sf".to_string()]
        );
        // Unknown name / qualifier → no hit.
        assert!(g.find_by_name("nowhere", &[]).unwrap().is_empty());
        assert!(g.find_by_name("basecamp", &["boston".into()]).unwrap().is_empty());
    }

    #[tokio::test]
    async fn find_by_name_qualifier_subset_rule() {
        let g = SqliteStore::open_in_memory().unwrap();
        // H5: two same-name entities that share ONLY a coarse country qualifier.
        g.create_entity("cx_bos", "google_place_id:BOS", None, Some("Acme")).await
            .unwrap();
        g.create_entity("cx_sea", "google_place_id:SEA", None, Some("Acme")).await
            .unwrap();
        g.index_name("acme", &["boston".into(), "us".into()], "cx_bos", Some("t")).await
            .unwrap();

        // Querying a DIFFERENT city (sharing only "us") must MISS the Boston
        // entity — indexed {boston, us} ⊄ query {seattle, us}.
        assert!(g
            .find_by_name("acme", &["seattle".into(), "us".into()])
            .unwrap()
            .is_empty());
        // The exact city (superset query) hits.
        assert_eq!(
            g.find_by_name("acme", &["boston".into(), "us".into(), "ma".into()])
                .unwrap(),
            vec!["cx_bos".to_string()]
        );

        // M7: a bare-indexed entity (empty qualifier set) is hit by any qualified
        // query — {} ⊆ anything.
        g.index_name("cafe x", &[], "cx_sea", Some("t")).await.unwrap();
        assert_eq!(
            g.find_by_name("cafe x", &["oakland".into()]).unwrap(),
            vec!["cx_sea".to_string()]
        );
    }

    #[tokio::test]
    async fn name_entities_reports_establishing_sets() {
        let g = SqliteStore::open_in_memory().unwrap();
        g.create_entity("cx_a", "google_place_id:A", None, Some("X")).await.unwrap();
        g.create_entity("cx_b", "google_place_id:B", None, Some("X")).await.unwrap();
        g.index_name("x", &["100 a st".into(), "sf".into()], "cx_a", Some("t")).await.unwrap();
        g.index_name("x", &["sf".into()], "cx_b", Some("t")).await.unwrap();

        let ents = g.name_entities("x").await.unwrap();
        // Sorted by canonical id; the always-present "" bare row is excluded.
        assert_eq!(ents[0].0, "cx_a");
        assert_eq!(ents[0].1, vec!["100 a st".to_string(), "sf".to_string()]);
        assert_eq!(ents[1].0, "cx_b");
        assert_eq!(ents[1].1, vec!["sf".to_string()]);
        assert!(g.name_entities("nope").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn cardinality_memory_roundtrips_and_is_specificity_exact() {
        let g = SqliteStore::open_in_memory().unwrap();
        let cands = vec![
            (String::new(), "google_place_id:A".into(), None),
            (String::new(), "google_place_id:B".into(), Some("Joe's".into())),
        ];
        g.record_name_cardinality("joe's pizza", &["new york".into()], &cands).await
            .unwrap();

        let got = match g.name_cardinality("joe's pizza", &["new york".into()]).await.unwrap().unwrap() {
            NameCardinality::Ambiguous(c) => c,
            other => panic!("expected ambiguous, got {other:?}"),
        };
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].1, "google_place_id:B");
        assert_eq!(got[1].2.as_deref(), Some("Joe's"));

        // A DIFFERENT (finer/coarser) qualifier set does not match — the coarse
        // ambiguity never answers a more-specific query.
        assert!(g
            .name_cardinality("joe's pizza", &["brooklyn".into(), "new york".into()]).await
            .unwrap()
            .is_none());
        assert!(g.name_cardinality("joe's pizza", &[]).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn unique_cardinality_roundtrips_and_validates_liveness() {
        let g = SqliteStore::open_in_memory().unwrap();
        g.create_entity("cx_one", "google_place_id:ONE", None, Some("Kibatsu")).await
            .unwrap();
        g.record_name_unique("kibatsu", &["san francisco".into()], "cx_one").await
            .unwrap();

        match g.name_cardinality("kibatsu", &["san francisco".into()]).await.unwrap().unwrap() {
            NameCardinality::Unique(cid) => assert_eq!(cid, "cx_one"),
            other => panic!("expected unique, got {other:?}"),
        }

        // Exact qualifier-set match only.
        assert!(g.name_cardinality("kibatsu", &[]).await.unwrap().is_none());

        // A unique row whose id no longer resolves (deleted entity) is a miss,
        // never a dead hit.
        g.exec_raw("DELETE FROM entities WHERE canonical_id = 'cx_one';")
            .unwrap();
        assert!(g.name_cardinality("kibatsu", &["san francisco".into()]).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn cardinality_flips_between_unique_and_ambiguous() {
        let g = SqliteStore::open_in_memory().unwrap();
        g.create_entity("cx_one", "google_place_id:ONE", None, Some("Nova")).await
            .unwrap();
        // First: unique.
        g.record_name_unique("nova", &["berlin".into()], "cx_one").await.unwrap();
        assert!(matches!(
            g.name_cardinality("nova", &["berlin".into()]).await.unwrap().unwrap(),
            NameCardinality::Unique(_)
        ));
        // Later hub call says MULTIPLE → flips to ambiguous, clearing the id.
        let cands = vec![
            (String::new(), "google_place_id:X".into(), None),
            (String::new(), "google_place_id:Y".into(), None),
        ];
        g.record_name_cardinality("nova", &["berlin".into()], &cands).await.unwrap();
        match g.name_cardinality("nova", &["berlin".into()]).await.unwrap().unwrap() {
            NameCardinality::Ambiguous(c) => assert_eq!(c.len(), 2),
            other => panic!("expected ambiguous after flip, got {other:?}"),
        }
        // And back to unique again (newest truth wins).
        g.record_name_unique("nova", &["berlin".into()], "cx_one").await.unwrap();
        assert!(matches!(
            g.name_cardinality("nova", &["berlin".into()]).await.unwrap().unwrap(),
            NameCardinality::Unique(_)
        ));
    }

    #[tokio::test]
    async fn merge_repoints_unique_cardinality_to_winner() {
        let g = SqliteStore::open_in_memory().unwrap();
        g.create_entity("cx_loser", "google_place_id:L", None, Some("Kibatsu")).await
            .unwrap();
        g.create_entity("cx_winner", "wikidata:Q1", None, Some("Kibatsu")).await
            .unwrap();
        g.attach("google_place_id:L", "cx_loser").await.unwrap();
        g.attach("wikidata:Q1", "cx_winner").await.unwrap();
        g.record_name_unique("kibatsu", &["san francisco".into()], "cx_loser").await
            .unwrap();

        g.merge_into("cx_winner", "cx_loser").await.unwrap();

        // The unique row now names the winner and still resolves.
        match g.name_cardinality("kibatsu", &["san francisco".into()]).await.unwrap().unwrap() {
            NameCardinality::Unique(cid) => assert_eq!(cid, "cx_winner"),
            other => panic!("expected unique pointing at winner, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn migrates_old_name_cardinality_without_status_column() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old_card.db");
        let path = path.to_str().unwrap();
        // Simulate an M2-early DB: name_cardinality with only the 3 original
        // columns (no status / canonical_id) and one recorded ambiguous row.
        {
            let conn = Connection::open(path).unwrap();
            conn.execute_batch(
                "CREATE TABLE name_cardinality (name_norm TEXT NOT NULL, qualifier_set TEXT NOT NULL, candidates TEXT NOT NULL, PRIMARY KEY (name_norm, qualifier_set));
                 INSERT INTO name_cardinality(name_norm, qualifier_set, candidates)
                   VALUES ('joe''s pizza', 'new york', '[{\"canonical_id\":\"\",\"anchor\":\"google_place_id:A\",\"name\":null},{\"canonical_id\":\"\",\"anchor\":\"google_place_id:B\",\"name\":null}]');",
            )
            .unwrap();
        }
        // Opening through SqliteStore::open migrates in status + canonical_id and the
        // pre-existing row reads back as ambiguous (its historical meaning).
        let g = SqliteStore::open(path).unwrap();
        match g.name_cardinality("joe's pizza", &["new york".into()]).await.unwrap().unwrap() {
            NameCardinality::Ambiguous(c) => assert_eq!(c.len(), 2),
            other => panic!("legacy row must read as ambiguous, got {other:?}"),
        }
        // And a fresh unique write works against the migrated table.
        g.create_entity("cx_u", "google_place_id:U", None, Some("Solo")).await.unwrap();
        g.record_name_unique("solo", &["reno".into()], "cx_u").await.unwrap();
        assert!(matches!(
            g.name_cardinality("solo", &["reno".into()]).await.unwrap().unwrap(),
            NameCardinality::Unique(_)
        ));
    }

    #[tokio::test]
    async fn repoint_key_moves_single_node() {
        let g = SqliteStore::open_in_memory().unwrap();
        g.create_entity("cx_a", "domain:a.com", None, None).await.unwrap();
        g.create_entity("cx_b", "domain:b.com", None, None).await.unwrap();
        g.attach("google_place_id:X", "cx_a").await.unwrap();
        g.attach("google_place_id:Y", "cx_a").await.unwrap();

        g.repoint_key("google_place_id:Y", "cx_b").await.unwrap();

        assert_eq!(g.find("google_place_id:X").await.unwrap().as_deref(), Some("cx_a"));
        assert_eq!(g.find("google_place_id:Y").await.unwrap().as_deref(), Some("cx_b"));
    }

    #[tokio::test]
    async fn repoint_phone_edge_moves_one_edge() {
        let g = SqliteStore::open_in_memory().unwrap();
        g.create_entity("cx_a", "domain:a.com", None, None).await.unwrap();
        g.create_entity("cx_b", "domain:b.com", None, None).await.unwrap();
        g.add_phone_edge("phone:+15550001111", "cx_a").await.unwrap();

        g.repoint_phone_edge("phone:+15550001111", "cx_a", "cx_b")
            .unwrap();

        assert_eq!(g.find_phone("phone:+15550001111").await.unwrap(), vec!["cx_b"]);
    }

    #[tokio::test]
    async fn strong_key_count_and_empty_delete() {
        let g = SqliteStore::open_in_memory().unwrap();
        g.create_entity("cx_a", "domain:a.com", None, None).await.unwrap();
        g.attach("domain:a.com", "cx_a").await.unwrap();
        assert_eq!(g.strong_key_count("cx_a").await.unwrap(), 1);
        // Non-empty: delete is a no-op.
        assert!(!g.delete_entity_if_empty("cx_a").await.unwrap());
        assert!(g.get_entity("cx_a").await.unwrap().is_some());
        // Move the only key away, then the entity is empty and deletable.
        g.create_entity("cx_b", "domain:b.com", None, None).await.unwrap();
        g.repoint_key("domain:a.com", "cx_b").await.unwrap();
        assert_eq!(g.strong_key_count("cx_a").await.unwrap(), 0);
        assert!(g.delete_entity_if_empty("cx_a").await.unwrap());
        assert!(g.get_entity("cx_a").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn stats_buckets_and_miss_rate() {
        let g = SqliteStore::open_in_memory().unwrap();
        // 2 exact, 1 hub, 1 miss.
        g.record_resolution("hit", "exact_strong_key", Some("wikidata"), 0.95, None).await
            .unwrap();
        g.record_resolution("hit", "exact_strong_key", Some("domain"), 0.95, None).await
            .unwrap();
        g.record_resolution("hit", "hub_crosswalk", Some("imdb"), 0.90, None).await
            .unwrap();
        g.record_resolution("unresolved", "needs_stronger_identifier", None, 0.15, None).await
            .unwrap();

        let s = g.stats().await.unwrap();
        assert_eq!(s.total, 4);
        assert_eq!(s.exact, 2);
        assert_eq!(s.hub, 1);
        assert_eq!(s.miss, 1);
        assert!((s.miss_rate() - 0.25).abs() < 1e-9);
        // Most-frequent reason first.
        assert_eq!(s.by_reason[0].0, "exact_strong_key");
        assert_eq!(s.by_reason[0].1, 2);
    }

    #[tokio::test]
    async fn stats_empty_graph_is_zero_not_nan() {
        let g = SqliteStore::open_in_memory().unwrap();
        let s = g.stats().await.unwrap();
        assert_eq!(s.total, 0);
        assert_eq!(s.miss_rate(), 0.0);
    }

    #[tokio::test]
    async fn foreign_keys_are_enforced() {
        let g = SqliteStore::open_in_memory().unwrap();
        // With PRAGMA foreign_keys = ON, a node referencing a non-existent
        // entity is rejected (previously silently accepted).
        assert!(g.attach("google_place_id:X", "cx_missing").await.is_err());
    }

    #[tokio::test]
    async fn merge_into_leaves_no_dangling_refs_under_fk() {
        let g = SqliteStore::open_in_memory().unwrap();
        g.create_entity("cx_1", "domain:a.com", None, None).await.unwrap();
        g.create_entity("cx_2", "wikidata:Q1", None, None).await.unwrap();
        g.attach("domain:a.com", "cx_1").await.unwrap();
        g.attach("wikidata:Q1", "cx_2").await.unwrap();
        // The transactional merge_into must re-point the loser's nodes before it
        // deletes the loser entity row, so the FK check passes at commit.
        g.merge_into("cx_2", "cx_1").await.unwrap();
        assert!(g.get_entity("cx_1").await.unwrap().is_none());
        assert_eq!(g.find("domain:a.com").await.unwrap().as_deref(), Some("cx_2"));
    }

    #[tokio::test]
    async fn clear_name_caches_drops_index_and_unique_only() {
        let g = SqliteStore::open_in_memory().unwrap();
        g.create_entity("cx_a", "google_place_id:A", None, Some("Joe")).await.unwrap();
        g.attach("google_place_id:A", "cx_a").await.unwrap();
        g.index_name("joe", &["ny".into()], "cx_a", Some("t")).await.unwrap();
        g.record_name_unique("joe", &["ny".into()], "cx_a").await.unwrap();
        // An ambiguous row (NULL canonical_id) that must survive the clear.
        let cands = vec![(String::new(), "google_place_id:X".into(), None)];
        g.record_name_cardinality("joe", &["sf".into()], &cands).await.unwrap();

        g.clear_name_caches("cx_a").unwrap();

        // Both the index row and the unique cardinality row for cx_a are gone.
        assert!(g.find_by_name("joe", &["ny".into()]).unwrap().is_empty());
        assert!(g.name_cardinality("joe", &["ny".into()]).await.unwrap().is_none());
        // The ambiguous (NULL-cid) row is untouched.
        assert!(g.name_cardinality("joe", &["sf".into()]).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g.db");
        let path = path.to_str().unwrap();
        {
            let g = SqliteStore::open(path).unwrap();
            g.create_entity("cx_a", "domain:a.com", None, None).await.unwrap();
            g.attach("domain:a.com", "cx_a").await.unwrap();
        }
        let g = SqliteStore::open(path).unwrap();
        assert_eq!(g.find("domain:a.com").await.unwrap().as_deref(), Some("cx_a"));
    }
}
