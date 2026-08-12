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
