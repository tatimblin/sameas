# sameas

`sameas` resolves **one partial identifier about an entity into a canonical ID
and the completed set of linked identifiers**. Give it a restaurant's website, a
Google Place ID, an IMDb page, or a phone number, and it returns a stable
`canonical_id`, the canonical `anchor`, and the entity's full `sameAs[]` set.

Internally it is an **identity crosswalk graph**: a union-find over typed
external identifiers, persisted in SQLite. Resolving an input attaches it to a
cluster and returns the whole cluster — **resolve *is* completion**.

See [`PROJECT_GOALS.md`](./PROJECT_GOALS.md) and [`ROADMAP.md`](./ROADMAP.md) for
the full vision. This repository implements **Milestones M1 and M2**.

## M1 scope

- **`sameas-core`** (library)
  - `kind` — the `KindSpec` **registry** (`KINDS`): the single source of truth
    for every identifier kind. Adding a kind is one registry entry + a
    normalizer (see [Adding an identifier kind](#adding-an-identifier-kind)).
  - `model` — spec-backed `ExternalId` (`domain`, `google_place_id`, `imdb`,
    `phone`, `wikidata`, `tmdb`, `yelp`, `placekey`) + `EntityRecord`.
  - `normalize` — URL → registrable domain (Public Suffix List), phone → E.164,
    IMDb `tt…`, Wikidata `Q…`, TMDb, bare place_id, Yelp biz slug, Placekey.
  - `graph` — union-find over `external_id → canonical_id` in SQLite; ID-to-ID
    edges only, each with a `source` (provenance). Phone edges live outside the
    union-find (corroborator only).
  - `anchor` — deterministic canonical-anchor selection, driven by each kind's
    registry `anchor_rank` (Wikidata QID > Placekey > registrable domain >
    Google place_id > Yelp biz > synthetic `local:<uuid>`).
  - `resolve` — `Resolver` trait; `DirectRecordResolver` (harvest a record's
    `sameAs`) and `DomainResolver` (parse schema.org JSON-LD / OpenGraph /
    `<link rel=canonical>` from an HTML fixture, or over HTTP behind the
    `live-fetch` feature). **A domain is a plain graph key by default** —
    resolving `--domain` is a normalize-and-look-up, no page fetch. Harvesting a
    page for extra `sameAs` is strictly opt-in (`--fixture` or `--fetch`).
- **`sameas-cli`** (binary `sameas`) — `resolve`, `entity`, `ingest`.

### M2 scope — hub bootstrapping

When the local graph lacks an edge, `sameas resolve … --complete` reaches
external identity hubs to fill it in, so an input completes **from an empty
graph**:

- `hubs/wikidata.rs` — IMDb `tt…` / website / QID → Wikidata item via SPARQL,
  harvesting P345 (IMDb), P856 (website), P1329 (phone), P4947 (TMDb).
- `hubs/tmdb.rs` — TMDb Find-by-ID crosswalk (`imdb ↔ tmdb ↔ wikidata`).
- `hubs/places.rs` — Google Place Details (`place_id → website, phone`) and a
  reverse text search (`name/address` or `phone → place_id`).
- `hubs/placekey.rs` — `name/address → Placekey` anchor (the primary path for a
  **name + city** input, paired with a Google `place_id` lookup so it still
  completes to website/phone).
- `confidence` — a `0.0`–`1.0` score on every resolution (exact strong key ≈
  0.95; name+city ≈ 0.40; phone-only ≈ 0.30).
- `transport` — `HttpTransport` with an offline `FixtureTransport` (CI + demo)
  and a live `ReqwestTransport` behind the `live-fetch` feature.

All adapters sit behind the M1 `Resolver` trait. Completion is opt-in, runs a
bounded idempotent BFS over the cluster, and is best-effort (a hub that is
unavailable yields no completion, never an error).

### Correctness invariants

- **Phone is a corroborator only.** Two otherwise-distinct entities are never
  merged on a shared phone. The phone edge is recorded, but only strong keys
  (domain, place_id, imdb, wikidata, tmdb, yelp) drive merges.
- **Stable identity.** Any identifier in a cluster resolves to the same
  `canonical_id`. Union-find is transitive through SQLite.
- **Deterministic anchors.** Canonical id is derived from the anchor; a
  synthetic `local:<uuid>` is minted only when no public anchor is present.

### Not yet implemented

No synthetic-ID corrections / `merge` / `split` / miss-rate metrics (M3), no HTTP
API (M4), and **no ML / embeddings / ONNX / tokio / axum**. Standalone
`phone → place_id` merging is deferred to M3 (its non-merge semantics need
provisional entities).

## Build

```sh
cargo build            # workspace (offline resolvers only)
cargo build --features live-fetch   # enable HTTP fetch (DomainResolver + hubs)
```

## Test

```sh
cargo test             # unit tests + end-to-end CLI integration test
```

Covers registrable-domain extraction, phone→E.164, IMDb/QID/TMDb/Placekey
normalization, JSON-LD `sameAs` extraction, union-find transitivity through
SQLite, the `source`-column migration on a pre-M2 DB, anchor priority ordering,
phone-alone-does-not-merge, the hub adapters' JSON parsing, the exit criteria
(IMDb → QID + TMDb and place_id → website + phone from an empty graph), and a
full CLI demo flow that asserts the same `canonical_id` across phone / place_id /
domain resolves.

## Demo (offline, deterministic)

```sh
bash examples/demo.sh
```

Runs against a throwaway DB and tells the "partial info → completed entity"
story: ingest a restaurant, resolve it by phone (completion), by place_id (same
canonical id), by domain via an HTML fixture (harvest), show the cluster, then
the movie path by IMDb. It ends with an identity-stability check, then an **M2
hub-bootstrapping** section (offline via `examples/fixtures/hubs`) showing an
IMDb id, a place_id, and a name+city all completing from an **empty** graph.

## CLI usage

```sh
# Resolve a single identifier (creates or hits an entity)
sameas resolve --phone "+1-510-653-3394"
sameas resolve --place-id "ChIJ..."
sameas resolve --imdb tt0133093
sameas resolve --input record.json

# Generic kind:value flag — works for ANY registered kind, no per-kind CLI code.
# This is how new identifier kinds are resolved (e.g. Yelp):
sameas resolve --id yelp:blue-bottle-coffee-san-francisco
sameas resolve --id wikidata:Q4926426

# A domain is just a key — pure graph lookup, no page fetch:
sameas resolve --domain example.com
# ...opt in to harvest extra sameAs from the page (never implicit):
sameas resolve --domain example.com --fixture path/to/page.html   # offline
sameas resolve --domain example.com --fetch                        # needs: --features live-fetch

# Hub bootstrapping (M2): complete missing edges from external hubs.
# Opt-in via --complete; offline against canned fixtures, or live with keys.
sameas resolve --imdb tt0133093 --complete --hub-fixtures examples/fixtures/hubs
sameas resolve --place-id "ChIJ..." --complete --hub-fixtures examples/fixtures/hubs
# Name + address (address may be just a city) -> Placekey anchor + place_id:
sameas resolve --name "Blue Bottle Coffee" --city Oakland --region CA --country US \
  --complete --hub-fixtures examples/fixtures/hubs
# Live (needs: --features live-fetch, and TMDB_API_KEY / GOOGLE_PLACES_API_KEY /
# PLACEKEY_API_KEY env vars): drop --hub-fixtures.
# sameas resolve --imdb tt0133093 --complete

# Load seed record(s) into the graph
sameas ingest examples/seed/blue-bottle.json      # file or directory

# Inspect an entity and its members
sameas entity cx_780d70d9

# Options
--db <path>   SQLite crosswalk path (default ./sameas.db)
--json        machine-readable output (default is a human table)
```

Seed records are schema.org-style JSON with a typed `sameAs` array. Each entry
is a single-key `{"<kind>": "<value>"}` object; the kind tag is dispatched
through the registry, so **adding a new kind requires no deserialization
changes** — `{"yelp": "..."}` just works:

```json
{
  "type": "LocalBusiness",
  "name": "Blue Bottle Coffee",
  "sameAs": [
    { "domain": "https://www.bluebottlecoffee.com" },
    { "google_place_id": "ChIJN1t_tDeuEmsRUsoyG83frY4" },
    { "phone": "+1-510-653-3394" },
    { "wikidata": "https://www.wikidata.org/wiki/Q4926426" },
    { "yelp": "https://www.yelp.com/biz/blue-bottle-coffee-san-francisco" }
  ]
}
```

## Adding an identifier kind

Identifier kinds live in a single **registry** — `KINDS`, a
`&[KindSpec]` in `sameas-core/src/kind.rs`. That registry is the only place the
set of kinds is enumerated: `model` (the `ExternalId` type + seed-JSON parsing),
`anchor` (public-anchor eligibility and priority), `resolve` (URL harvesting),
and the CLI (`--id kind:value`) all read from it. Adding a kind is therefore
**one `KindSpec` entry + a normalizer function** (plus an optional `url_match`
recognizer). No enum edits, no per-kind CLI flag, no deserialization changes.

A `KindSpec` is:

```rust
pub struct KindSpec {
    pub tag: &'static str,                 // key prefix + serialized JSON tag
    pub strong: bool,                      // true = drives merges; false = corroborator
    pub anchor_rank: Option<u8>,           // Some(rank) => public-anchor candidate (lower = stronger)
    pub normalize: fn(&str) -> Result<String>,        // raw -> canonical value
    pub url_match: Option<fn(&str) -> Option<String>>, // recognize this kind in a sameAs URL
}
```

**Worked example — Yelp** (the whole change was these two things):

1. Add a normalizer in `normalize.rs` (`https://www.yelp.com/biz/<slug>?…` →
   `<slug>`, bare slug accepted too):

   ```rust
   pub fn yelp(raw: &str) -> Result<String> { /* strip scheme/host/query, take biz slug */ }
   ```

2. Add one registry entry in `kind.rs`:

   ```rust
   KindSpec {
       tag: "yelp",
       strong: true,            // a Yelp biz id identifies a single business
       anchor_rank: Some(4),    // public anchor, just below google_place_id (3)
       normalize: normalize::yelp,
       url_match: Some(match_yelp), // recognizes yelp.com/biz/<slug> in sameAs URLs
   }
   ```

That is the entire diff. Yelp now:

- parses from seed JSON (`{"yelp": "…"}`),
- resolves from the CLI (`sameas resolve --id yelp:<slug>`),
- is harvested from a page's `sameAs` URLs,
- participates in anchor selection at its rank, and
- gets a thin named constructor `ExternalId::yelp(raw)` (optional convenience;
  `ExternalId::new("yelp", raw)` works without it).
