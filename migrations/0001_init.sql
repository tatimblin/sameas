-- The crosswalk graph schema, for the Cloudflare D1 (Worker) backend.
--
-- `SqliteStore` creates and migrates its own schema at `open()` (it is a local
-- file); D1 has no equivalent hook, so the Worker's schema is managed here and
-- applied with `wrangler d1 migrations apply`. `store::d1::D1Store` therefore runs
-- no DDL at all.
--
-- This is the POST-migration shape of the SQLite schema: the `source`, `status`,
-- and `canonical_id` columns that `SqliteStore::migrate` adds to pre-M2 databases
-- are already present, since a D1 database starts empty and has no history to
-- replay. Keep it in sync with `SCHEMA` in `crates/sameas-core/src/store/sqlite.rs`.
--
-- We store only ID-to-ID edges plus a minimal canonical anchor — never provider
-- content (see PROJECT_GOALS.md: "not a data warehouse of provider content").

-- The canonical entity: its anchor plus light display metadata.
CREATE TABLE IF NOT EXISTS entities (
    canonical_id TEXT PRIMARY KEY,
    anchor       TEXT NOT NULL,
    entity_type  TEXT,
    name         TEXT
);

-- Strong keys. `key` is unique, so a strong identifier belongs to exactly one
-- entity; union re-points losers to the winner. This IS the union-find.
CREATE TABLE IF NOT EXISTS nodes (
    key          TEXT PRIMARY KEY,
    canonical_id TEXT NOT NULL,
    source       TEXT,
    FOREIGN KEY(canonical_id) REFERENCES entities(canonical_id)
);
CREATE INDEX IF NOT EXISTS idx_nodes_canonical ON nodes(canonical_id);

-- Phone is a CORROBORATOR ONLY, so it lives outside the union-find: one phone may
-- edge to several entities without merging them.
CREATE TABLE IF NOT EXISTS phone_edges (
    phone_key    TEXT NOT NULL,
    canonical_id TEXT NOT NULL,
    source       TEXT,
    PRIMARY KEY (phone_key, canonical_id),
    FOREIGN KEY(canonical_id) REFERENCES entities(canonical_id)
);
CREATE INDEX IF NOT EXISTS idx_phone_canonical ON phone_edges(canonical_id);

-- Local name-resolution memory: one row per (name, qualifier) facet, plus a
-- `qualifier = ''` row so a bare-name query can still match.
CREATE TABLE IF NOT EXISTS name_index (
    name_norm    TEXT NOT NULL,
    qualifier    TEXT NOT NULL,
    canonical_id TEXT NOT NULL,
    source       TEXT,
    PRIMARY KEY (name_norm, qualifier, canonical_id),
    FOREIGN KEY(canonical_id) REFERENCES entities(canonical_id)
);
CREATE INDEX IF NOT EXISTS idx_name_index_name ON name_index(name_norm);

-- What a hub text-search revealed about the uniqueness of a (name, qualifier-set)
-- query, remembered so a later identical query costs no external call.
CREATE TABLE IF NOT EXISTS name_cardinality (
    name_norm     TEXT NOT NULL,
    qualifier_set TEXT NOT NULL,
    candidates    TEXT NOT NULL,
    status        TEXT NOT NULL DEFAULT 'ambiguous',
    canonical_id  TEXT,
    PRIMARY KEY (name_norm, qualifier_set)
);

-- Append-only log of resolution outcomes, one row per user-facing resolve. The
-- evidence gate for the optional fuzzy phase: `sameas stats` aggregates these into
-- an exact/hub/miss breakdown plus a headline miss rate. rowid gives insertion
-- order; no wall-clock is stored (kept deterministic + IDs-only).
CREATE TABLE IF NOT EXISTS resolutions (
    status_tag   TEXT NOT NULL,
    reason_tag   TEXT NOT NULL,
    matched_via  TEXT,
    confidence   REAL NOT NULL,
    input_desc   TEXT
);
