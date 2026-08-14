//! The crosswalk graph: a union-find over typed external-id keys, persisted in
//! SQLite. We store **only ID-to-ID edges** plus a minimal canonical anchor —
//! never provider content.
//!
//! Layout:
//! * `nodes(key, canonical_id)` — strong keys. `key` is unique, so a strong
//!   identifier belongs to exactly one entity; union re-points losers to the
//!   winner. This *is* the union-find.
//! * `phone_edges(phone_key, canonical_id)` — phone is a **corroborator only**,
//!   so it lives outside the union-find and a phone may edge to several
//!   entities without merging them.
//! * `entities(canonical_id, anchor, entity_type, name)` — the anchor + light
//!   display metadata.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

use crate::model::ExternalId;

pub struct Graph {
    conn: Connection,
}

/// A stored ambiguity candidate: `(canonical_id, anchor, name)`. `canonical_id`
/// is usually empty (these are un-committed candidates surfaced by a hub search).
pub type NameCandidate = (String, String, Option<String>);

/// What a hub text-search revealed about the uniqueness of a `(name, Q)` query,
/// as remembered locally. `Unique` carries the resolved `canonical_id` (so a
/// later coarse repeat can hit locally with zero external calls); `Ambiguous`
/// carries the candidate list surfaced when the hub returned more than one.
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
"#;

impl Graph {
    pub fn open(path: &str) -> Result<Graph> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Graph> {
        let conn = Connection::open_in_memory()?;
        Self::init(conn)
    }

    fn init(conn: Connection) -> Result<Graph> {
        conn.execute_batch(SCHEMA)?;
        Self::migrate(&conn)?;
        Ok(Graph { conn })
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

    // --- union-find over strong keys ------------------------------------

    /// Find the canonical id a strong key currently belongs to.
    pub fn find(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT canonical_id FROM nodes WHERE key = ?1",
                params![key],
                |row| row.get::<_, String>(0),
            )
            .optional()?)
    }

    /// Attach (or re-point) a strong key to a canonical id.
    pub fn attach(&self, key: &str, canonical_id: &str) -> Result<()> {
        self.attach_with_source(key, canonical_id, None)
    }

    /// Attach (or re-point) a strong key, recording where the edge came from.
    /// A `None` source leaves the provenance NULL; a re-point updates it only
    /// when a source is supplied (so a later, better-attributed write can fill
    /// it in without a plain re-point clobbering it to NULL).
    pub fn attach_with_source(
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

    pub fn add_phone_edge(&self, phone_key: &str, canonical_id: &str) -> Result<()> {
        self.add_phone_edge_with_source(phone_key, canonical_id, None)
    }

    /// Record a phone corroborator edge with provenance.
    pub fn add_phone_edge_with_source(
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

    /// All canonical ids a phone corroborates (may be more than one; that does
    /// not mean they are the same entity).
    pub fn find_phone(&self, phone_key: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT canonical_id FROM phone_edges WHERE phone_key = ?1 ORDER BY canonical_id")?;
        let rows = stmt
            .query_map(params![phone_key], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // --- entities -------------------------------------------------------

    pub fn create_entity(
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

    pub fn get_entity(&self, canonical_id: &str) -> Result<Option<EntityRow>> {
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

    pub fn set_anchor(&self, canonical_id: &str, anchor: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE entities SET anchor = ?2 WHERE canonical_id = ?1",
            params![canonical_id, anchor],
        )?;
        Ok(())
    }

    /// Fill in type/name only when currently empty (never clobber existing).
    pub fn enrich_entity(
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

    // --- members --------------------------------------------------------

    /// All member keys (strong nodes + phone edges) of a canonical entity.
    pub fn member_keys(&self, canonical_id: &str) -> Result<Vec<String>> {
        let mut keys = Vec::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT key FROM nodes WHERE canonical_id = ?1")?;
            let rows = stmt
                .query_map(params![canonical_id], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            keys.extend(rows);
        }
        {
            let mut stmt = self
                .conn
                .prepare("SELECT phone_key FROM phone_edges WHERE canonical_id = ?1")?;
            let rows = stmt
                .query_map(params![canonical_id], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            keys.extend(rows);
        }
        Ok(keys)
    }

    /// All member keys with their edge provenance (source), for reporting.
    /// Sorted by key for stable output.
    pub fn member_sources(&self, canonical_id: &str) -> Result<Vec<(String, Option<String>)>> {
        let mut rows: Vec<(String, Option<String>)> = Vec::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT key, source FROM nodes WHERE canonical_id = ?1")?;
            let r = stmt
                .query_map(params![canonical_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
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
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows.extend(r);
        }
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        rows.dedup();
        Ok(rows)
    }

    /// Members as typed identifiers, sorted for stable output.
    pub fn members(&self, canonical_id: &str) -> Result<Vec<ExternalId>> {
        let mut ids: Vec<ExternalId> = self
            .member_keys(canonical_id)?
            .iter()
            .filter_map(|k| ExternalId::from_key(k))
            .collect();
        ids.sort_by(|a, b| {
            a.kind_tag()
                .cmp(b.kind_tag())
                .then_with(|| a.value().cmp(b.value()))
        });
        ids
            .dedup();
        Ok(ids)
    }

    // --- local name index (resolve name + qualifiers offline) -----------

    /// Index an entity under a normalized name and a set of normalized qualifier
    /// tokens (city / state / borough / year / …). Writes one row per qualifier,
    /// plus a `qualifier = ""` row so a bare-name query can still match. Both
    /// name and qualifiers are expected already normalized (via
    /// `normalize::name_key`).
    pub fn index_name(
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
        quals.extend(qualifiers.iter().map(|q| q.as_str()).filter(|q| !q.is_empty()));
        for q in quals {
            self.conn.execute(
                "INSERT OR IGNORE INTO name_index(name_norm, qualifier, canonical_id, source)
                 VALUES (?1, ?2, ?3, ?4)",
                params![name_norm, q, canonical_id, source],
            )?;
        }
        Ok(())
    }

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

    /// The non-empty qualifier tokens an entity was indexed under for `name_norm`.
    /// (The always-present `""` row is excluded — it is the bare-name marker, not
    /// a facet.) Used by [`find_by_name`] for the qualifier-subset test.
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

    /// Every entity indexed under `name_norm`, paired with the non-empty
    /// qualifier tokens it was established under (the union of all facets ever
    /// indexed for it — its "establishing set"). The always-present `""` bare
    /// row is excluded. Used by the specificity-monotonic local matcher: an
    /// entity is a confident hit only for a query whose token set is a superset
    /// of its establishing set. Sorted by canonical id (tokens sorted) for
    /// determinism.
    pub fn name_entities(&self, name_norm: &str) -> Result<Vec<(String, Vec<String>)>> {
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

    // --- cardinality / negative memory ----------------------------------
    //
    // What a hub text-search revealed about the uniqueness of (name, Q):
    //   * MULTIPLE candidates → the query is ambiguous; we persist the candidate
    //     list so a later identical coarse query is answered from local memory
    //     (zero external calls) instead of re-calling the hub or wrong-binding.
    //   * EXACTLY ONE result that resolved → the query is unique; we persist the
    //     resolved canonical_id so a later coarse repeat that under-specifies the
    //     entity's establishing set (and so misses the superset scan) still hits
    //     locally instead of re-calling the hub every time.
    //
    // Both key on (name, qualifier-set) via INSERT OR REPLACE semantics, so the
    // newest hub truth wins: a later MULTIPLE overwrites a unique row with an
    // ambiguous one, and a later single overwrites an ambiguous row with a unique
    // one.
    //
    // Ambiguous candidates are stored as opaque `(canonical_id, anchor, name)`
    // triples (canonical_id is usually empty — un-committed candidates). Unique
    // rows carry a real `canonical_id`; `merge_into` re-points those to the union
    // winner, and reads validate the id still resolves (a merged/deleted id is
    // treated as a miss).

    /// A stable, canonical serialization of a normalized qualifier set for use as
    /// a table key. Callers pass an already-normalized, sorted, deduped set
    /// (`NameQuery::establishing_qualifiers`); we join with `\n` (never a token
    /// char) so distinct sets never collide.
    fn qualifier_set_key(qualifiers: &[String]) -> String {
        qualifiers.join("\n")
    }

    /// Record that (name, Q) is ambiguous among `candidates` (hub returned >1).
    /// Overwrites any prior row for (name, Q) — including a stale `unique` row —
    /// clearing its `canonical_id`, so the newest hub truth wins.
    pub fn record_name_cardinality(
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
                .map(|(cid, anchor, name)| serde_json::json!({
                    "canonical_id": cid,
                    "anchor": anchor,
                    "name": name,
                }))
                .collect::<Vec<_>>(),
        )?;
        self.conn.execute(
            "INSERT INTO name_cardinality(name_norm, qualifier_set, candidates, status, canonical_id)
             VALUES (?1, ?2, ?3, 'ambiguous', NULL)
             ON CONFLICT(name_norm, qualifier_set) DO UPDATE SET
                 candidates = excluded.candidates,
                 status = excluded.status,
                 canonical_id = excluded.canonical_id",
            params![name_norm, Self::qualifier_set_key(qualifiers), blob],
        )?;
        Ok(())
    }

    /// Record that (name, Q) resolved UNIQUELY to `canonical_id` (hub returned
    /// exactly one). Overwrites any prior row for (name, Q) — including a stale
    /// `ambiguous` row — so the newest hub truth wins.
    pub fn record_name_unique(
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
            params![name_norm, Self::qualifier_set_key(qualifiers), canonical_id],
        )?;
        Ok(())
    }

    /// The stored cardinality for (name, Q), if any — an EXACT qualifier-set
    /// match (specificity-preserving: a coarse fact never answers a finer query).
    /// A `unique` row whose `canonical_id` no longer resolves to a live entity is
    /// treated as a miss (returns `None`), so a merged/deleted id is never served.
    pub fn name_cardinality(
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
                params![name_norm, Self::qualifier_set_key(qualifiers)],
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
            return Ok(match canonical_id {
                Some(cid) if !cid.is_empty() && self.get_entity(&cid)?.is_some() => {
                    Some(NameCardinality::Unique(cid))
                }
                // Malformed unique (no id) or a stale id pointing at a
                // merged/deleted entity → treat as a miss, not a dead hit.
                _ => None,
            });
        }
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&blob)?;
        Ok(Some(NameCardinality::Ambiguous(
            parsed
                .into_iter()
                .map(|v| {
                    (
                        v.get("canonical_id").and_then(|x| x.as_str()).unwrap_or_default().to_string(),
                        v.get("anchor").and_then(|x| x.as_str()).unwrap_or_default().to_string(),
                        v.get("name").and_then(|x| x.as_str()).map(|s| s.to_string()),
                    )
                })
                .collect(),
        )))
    }

    // --- union ----------------------------------------------------------

    /// Merge `loser` into `winner`: re-point all strong nodes and phone edges,
    /// then drop the loser entity row. Strong keys drive this; callers must
    /// never invoke it on the strength of a phone alone.
    pub fn merge_into(&self, winner: &str, loser: &str) -> Result<()> {
        if winner == loser {
            return Ok(());
        }
        self.conn.execute(
            "UPDATE nodes SET canonical_id = ?1 WHERE canonical_id = ?2",
            params![winner, loser],
        )?;
        // Re-point phone edges, ignoring rows that would collide with an
        // existing (phone,winner) edge, then delete any leftovers.
        self.conn.execute(
            "UPDATE OR IGNORE phone_edges SET canonical_id = ?1 WHERE canonical_id = ?2",
            params![winner, loser],
        )?;
        self.conn.execute(
            "DELETE FROM phone_edges WHERE canonical_id = ?1",
            params![loser],
        )?;
        // Re-point local name-index rows too (ignore rows that would collide).
        self.conn.execute(
            "UPDATE OR IGNORE name_index SET canonical_id = ?1 WHERE canonical_id = ?2",
            params![winner, loser],
        )?;
        self.conn.execute(
            "DELETE FROM name_index WHERE canonical_id = ?1",
            params![loser],
        )?;
        // Re-point unique cardinality rows that named the loser (ambiguous rows
        // carry a NULL canonical_id, so this only touches unique memory).
        self.conn.execute(
            "UPDATE name_cardinality SET canonical_id = ?1 WHERE canonical_id = ?2",
            params![winner, loser],
        )?;
        self.conn.execute(
            "DELETE FROM entities WHERE canonical_id = ?1",
            params![loser],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn union_find_transitivity_through_sqlite() {
        let g = Graph::open_in_memory().unwrap();
        g.create_entity("cx_a", "domain:a.com", None, None).unwrap();
        // attach a <-> b (same canonical)
        g.attach("domain:a.com", "cx_a").unwrap();
        g.attach("domain:b.com", "cx_a").unwrap();
        // b <-> c (c joins the same canonical)
        g.attach("domain:c.com", "cx_a").unwrap();

        assert_eq!(g.find("domain:a.com").unwrap().as_deref(), Some("cx_a"));
        assert_eq!(
            g.find("domain:a.com").unwrap(),
            g.find("domain:c.com").unwrap()
        );
    }

    #[test]
    fn merge_repoints_members() {
        let g = Graph::open_in_memory().unwrap();
        g.create_entity("cx_1", "domain:a.com", None, None).unwrap();
        g.create_entity("cx_2", "wikidata:Q1", None, None).unwrap();
        g.attach("domain:a.com", "cx_1").unwrap();
        g.attach("wikidata:Q1", "cx_2").unwrap();

        g.merge_into("cx_2", "cx_1").unwrap();

        assert_eq!(g.find("domain:a.com").unwrap().as_deref(), Some("cx_2"));
        assert!(g.get_entity("cx_1").unwrap().is_none());
        assert_eq!(g.members("cx_2").unwrap().len(), 2);
    }

    #[test]
    fn migrates_pre_m2_db_without_source_column() {
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
        // Opening through Graph::open must migrate in the `source` column and
        // preserve the existing rows.
        let g = Graph::open(path).unwrap();
        assert_eq!(g.find("domain:a.com").unwrap().as_deref(), Some("cx_x"));
        // A source-aware write now works against the migrated table.
        g.attach_with_source("wikidata:Q1", "cx_x", Some("wikidata"))
            .unwrap();
        let sources = g.member_sources("cx_x").unwrap();
        assert!(sources
            .iter()
            .any(|(k, s)| k == "wikidata:Q1" && s.as_deref() == Some("wikidata")));
        // The pre-existing edge has a NULL (unknown) source.
        assert!(sources
            .iter()
            .any(|(k, s)| k == "domain:a.com" && s.is_none()));
    }

    #[test]
    fn name_index_matches_name_plus_qualifier() {
        let g = Graph::open_in_memory().unwrap();
        g.create_entity("cx_sf", "google_place_id:SF", None, Some("Basecamp"))
            .unwrap();
        g.create_entity("cx_ny", "google_place_id:NY", None, Some("Basecamp"))
            .unwrap();
        g.index_name("basecamp", &["san francisco".into()], "cx_sf", Some("t"))
            .unwrap();
        g.index_name("basecamp", &["new york".into()], "cx_ny", Some("t"))
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

    #[test]
    fn find_by_name_qualifier_subset_rule() {
        let g = Graph::open_in_memory().unwrap();
        // H5: two same-name entities that share ONLY a coarse country qualifier.
        g.create_entity("cx_bos", "google_place_id:BOS", None, Some("Acme"))
            .unwrap();
        g.create_entity("cx_sea", "google_place_id:SEA", None, Some("Acme"))
            .unwrap();
        g.index_name("acme", &["boston".into(), "us".into()], "cx_bos", Some("t"))
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
        g.index_name("cafe x", &[], "cx_sea", Some("t")).unwrap();
        assert_eq!(
            g.find_by_name("cafe x", &["oakland".into()]).unwrap(),
            vec!["cx_sea".to_string()]
        );
    }

    #[test]
    fn name_entities_reports_establishing_sets() {
        let g = Graph::open_in_memory().unwrap();
        g.create_entity("cx_a", "google_place_id:A", None, Some("X")).unwrap();
        g.create_entity("cx_b", "google_place_id:B", None, Some("X")).unwrap();
        g.index_name("x", &["100 a st".into(), "sf".into()], "cx_a", Some("t")).unwrap();
        g.index_name("x", &["sf".into()], "cx_b", Some("t")).unwrap();

        let ents = g.name_entities("x").unwrap();
        // Sorted by canonical id; the always-present "" bare row is excluded.
        assert_eq!(ents[0].0, "cx_a");
        assert_eq!(ents[0].1, vec!["100 a st".to_string(), "sf".to_string()]);
        assert_eq!(ents[1].0, "cx_b");
        assert_eq!(ents[1].1, vec!["sf".to_string()]);
        assert!(g.name_entities("nope").unwrap().is_empty());
    }

    #[test]
    fn cardinality_memory_roundtrips_and_is_specificity_exact() {
        let g = Graph::open_in_memory().unwrap();
        let cands = vec![
            (String::new(), "google_place_id:A".into(), None),
            (String::new(), "google_place_id:B".into(), Some("Joe's".into())),
        ];
        g.record_name_cardinality("joe's pizza", &["new york".into()], &cands)
            .unwrap();

        let got = match g.name_cardinality("joe's pizza", &["new york".into()]).unwrap().unwrap() {
            NameCardinality::Ambiguous(c) => c,
            other => panic!("expected ambiguous, got {other:?}"),
        };
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].1, "google_place_id:B");
        assert_eq!(got[1].2.as_deref(), Some("Joe's"));

        // A DIFFERENT (finer/coarser) qualifier set does not match — the coarse
        // ambiguity never answers a more-specific query.
        assert!(g
            .name_cardinality("joe's pizza", &["brooklyn".into(), "new york".into()])
            .unwrap()
            .is_none());
        assert!(g.name_cardinality("joe's pizza", &[]).unwrap().is_none());
    }

    #[test]
    fn unique_cardinality_roundtrips_and_validates_liveness() {
        let g = Graph::open_in_memory().unwrap();
        g.create_entity("cx_one", "google_place_id:ONE", None, Some("Kibatsu"))
            .unwrap();
        g.record_name_unique("kibatsu", &["san francisco".into()], "cx_one")
            .unwrap();

        match g.name_cardinality("kibatsu", &["san francisco".into()]).unwrap().unwrap() {
            NameCardinality::Unique(cid) => assert_eq!(cid, "cx_one"),
            other => panic!("expected unique, got {other:?}"),
        }

        // Exact qualifier-set match only.
        assert!(g.name_cardinality("kibatsu", &[]).unwrap().is_none());

        // A unique row whose id no longer resolves (deleted entity) is a miss,
        // never a dead hit.
        g.conn
            .execute("DELETE FROM entities WHERE canonical_id = 'cx_one'", [])
            .unwrap();
        assert!(g.name_cardinality("kibatsu", &["san francisco".into()]).unwrap().is_none());
    }

    #[test]
    fn cardinality_flips_between_unique_and_ambiguous() {
        let g = Graph::open_in_memory().unwrap();
        g.create_entity("cx_one", "google_place_id:ONE", None, Some("Nova"))
            .unwrap();
        // First: unique.
        g.record_name_unique("nova", &["berlin".into()], "cx_one").unwrap();
        assert!(matches!(
            g.name_cardinality("nova", &["berlin".into()]).unwrap().unwrap(),
            NameCardinality::Unique(_)
        ));
        // Later hub call says MULTIPLE → flips to ambiguous, clearing the id.
        let cands = vec![
            (String::new(), "google_place_id:X".into(), None),
            (String::new(), "google_place_id:Y".into(), None),
        ];
        g.record_name_cardinality("nova", &["berlin".into()], &cands).unwrap();
        match g.name_cardinality("nova", &["berlin".into()]).unwrap().unwrap() {
            NameCardinality::Ambiguous(c) => assert_eq!(c.len(), 2),
            other => panic!("expected ambiguous after flip, got {other:?}"),
        }
        // And back to unique again (newest truth wins).
        g.record_name_unique("nova", &["berlin".into()], "cx_one").unwrap();
        assert!(matches!(
            g.name_cardinality("nova", &["berlin".into()]).unwrap().unwrap(),
            NameCardinality::Unique(_)
        ));
    }

    #[test]
    fn merge_repoints_unique_cardinality_to_winner() {
        let g = Graph::open_in_memory().unwrap();
        g.create_entity("cx_loser", "google_place_id:L", None, Some("Kibatsu"))
            .unwrap();
        g.create_entity("cx_winner", "wikidata:Q1", None, Some("Kibatsu"))
            .unwrap();
        g.attach("google_place_id:L", "cx_loser").unwrap();
        g.attach("wikidata:Q1", "cx_winner").unwrap();
        g.record_name_unique("kibatsu", &["san francisco".into()], "cx_loser")
            .unwrap();

        g.merge_into("cx_winner", "cx_loser").unwrap();

        // The unique row now names the winner and still resolves.
        match g.name_cardinality("kibatsu", &["san francisco".into()]).unwrap().unwrap() {
            NameCardinality::Unique(cid) => assert_eq!(cid, "cx_winner"),
            other => panic!("expected unique pointing at winner, got {other:?}"),
        }
    }

    #[test]
    fn migrates_old_name_cardinality_without_status_column() {
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
        // Opening through Graph::open migrates in status + canonical_id and the
        // pre-existing row reads back as ambiguous (its historical meaning).
        let g = Graph::open(path).unwrap();
        match g.name_cardinality("joe's pizza", &["new york".into()]).unwrap().unwrap() {
            NameCardinality::Ambiguous(c) => assert_eq!(c.len(), 2),
            other => panic!("legacy row must read as ambiguous, got {other:?}"),
        }
        // And a fresh unique write works against the migrated table.
        g.create_entity("cx_u", "google_place_id:U", None, Some("Solo")).unwrap();
        g.record_name_unique("solo", &["reno".into()], "cx_u").unwrap();
        assert!(matches!(
            g.name_cardinality("solo", &["reno".into()]).unwrap().unwrap(),
            NameCardinality::Unique(_)
        ));
    }

    #[test]
    fn persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g.db");
        let path = path.to_str().unwrap();
        {
            let g = Graph::open(path).unwrap();
            g.create_entity("cx_a", "domain:a.com", None, None).unwrap();
            g.attach("domain:a.com", "cx_a").unwrap();
        }
        let g = Graph::open(path).unwrap();
        assert_eq!(g.find("domain:a.com").unwrap().as_deref(), Some("cx_a"));
    }
}
