# sameas

`sameas` resolves **one partial identifier about an entity into a canonical ID
and the completed set of linked identifiers**. Give it a restaurant's website, a
Google Place ID, an IMDb page, or a phone number, and it returns a stable
`canonical_id`, the canonical `anchor`, and the entity's full `sameAs[]` set.

Internally it is an **identity crosswalk graph**: a union-find over typed
external identifiers, persisted in SQLite. Resolving an input attaches it to a
cluster and returns the whole cluster — **resolve *is* completion**.

See [`PROJECT_GOALS.md`](./PROJECT_GOALS.md) and [`ROADMAP.md`](./ROADMAP.md) for
the full vision. This repository implements **Milestones M1, M2, and the M3
disambiguation core**.

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

### M3 scope — disambiguation (core)

Keeps distinct real-world things distinct, and treats confidence as a control
signal for the calling app. Type-agnostic — works for any schema.org Thing.

- **Entity-grain rule** (`kind.rs` `Grain`) — each kind is `Identity` (names one
  thing: `wikidata`/`imdb`/`tmdb`/`placekey`/`google_place_id`/`yelp`),
  `Affiliation` (may be shared: `domain`), or `Weak` (`phone`). A shared domain
  never merges two entities with disjoint identity keys, and anchor selection
  prefers Identity keys — so two locations of a chain that share `kibatsu.com`
  stay **distinct** (and don't collide on one canonical id).
- **Resolve-or-refuse** — resolution needs ≥1 strong key. Name-only or phone-only
  input **refuses**: `canonical_id: null`, `status: "unresolved"`, a
  `confidence_reason`, and `candidates` to confirm. Strong-key entities with no
  public anchor get a **deterministic** anchor (reproducible, no random UUID).
- **Confidence + reason** (`confidence.rs`) — a `0.0`–`1.0` score with a
  `confidence_reason` (`exact_strong_key`, `hub_crosswalk`, `placekey_city_only`,
  `ambiguous_among_n`, `needs_stronger_identifier`, …). A low score says *what to
  fix*, so an app can ask its user for a stronger identifier and re-resolve.

Deferred to a later milestone: `link`/`merge`/`split` corrections and miss-rate
metrics (`sameas stats`). Fuzzy/name-based matching stays deferred and
evidence-gated.

### Correctness invariants

- **Phone is a corroborator only.** Two otherwise-distinct entities are never
  merged on a shared phone. The phone edge is recorded, but only strong keys
  (domain, place_id, imdb, wikidata, tmdb, yelp, placekey) drive merges.
- **Entity grain (no false chain merges).** A shared **Affiliation** key (a
  domain) never merges two entities with disjoint **Identity** keys; distinct
  locations/things stay distinct even when they share a domain.
- **Refuse over guess.** No strong key → no minted entity; return `unresolved` +
  a reason instead of guessing (false-merge avoidance is the primary invariant).
- **Stable identity.** Any identifier in a cluster resolves to the same
  `canonical_id`. Union-find is transitive through SQLite.
- **Deterministic anchors.** Canonical id is derived from the anchor (Identity
  key preferred); reproducible across runs.

### Not yet implemented

`link` / `merge` / `split` corrections and miss-rate metrics (`sameas stats`)
remain (rest of M3); no HTTP API (M4); and **no ML / embeddings / ONNX / tokio /
axum**. Standalone `phone → place_id` reverse resolution is deferred (its
non-merge semantics need provisional entities). Fuzzy/name-based matching stays
deferred and evidence-gated.

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
(IMDb → QID + TMDb and place_id → website + phone from an empty graph), the
entity-grain rule (two chain locations sharing a domain stay distinct; two movies
sharing a studio domain stay distinct), resolve-or-refuse, ambiguity candidates,
and a full CLI demo flow.

## Demo (offline, deterministic)

```sh
bash examples/demo.sh
```

Runs against a throwaway DB and tells the "partial info → completed entity"
story: ingest a restaurant, resolve it by phone (completion), by place_id (same
canonical id), by domain via an HTML fixture (harvest), show the cluster, then
the movie path by IMDb. It ends with an identity-stability check, then an **M2
hub-bootstrapping** section (offline via `examples/fixtures/hubs`) showing an
IMDb id, a place_id, and a name+city all completing from an **empty** graph, and
an **M3 disambiguation** section showing two chain locations that share a domain
staying distinct and a too-thin query being refused with a "needs more info"
signal.

## Live hub calls (opt-in)

By default everything runs offline against fixtures. To call the real hubs, build
with `--features live-fetch` and supply API keys via env vars; then use
`--complete` **without** `--hub-fixtures`:

```sh
cargo build --features live-fetch
export GOOGLE_PLACES_API_KEY=…   # Google Maps Platform key with Places API (New) enabled
export PLACEKEY_API_KEY=…        # optional, for address → Placekey
export TMDB_API_KEY=…            # optional, for movie crosswalk

BIN=./target/debug/sameas
"$BIN" --db /tmp/live.db resolve --place-id "ChIJN1t_tDeuEmsRUsoyG83frY4" --complete
"$BIN" --db /tmp/live.db resolve --name "Blue Bottle Coffee" --type restaurant \
  --address "300 Webster St" --city Oakland --region CA --country US --complete
```

The live transport uses **Google Places API (New) v1** (`places.googleapis.com`,
`X-Goog-Api-Key` + field-mask headers), the Placekey API (`apikey` header), and
Wikidata SPARQL (descriptive `User-Agent`); it reuses one connection-pooled client
with timeouts, gzip, and bounded retry/backoff on 429/5xx (honoring `Retry-After`).
A bad key surfaces a clear `authentication/authorization denied (HTTP 401/403)`
error rather than a silent empty result.

Gated live smoke test (skips if the key env var is absent):
```sh
GOOGLE_PLACES_API_KEY=… cargo test -p sameas-cli --features live-fetch -- --ignored
```

> **Notes.** Place Details `websiteUri`/phone fall in Google's **Enterprise** SKU,
> so each live Place Details call is billed at that tier. Placekey needs a **street
> address** (or lat/long) — a name+city query resolves via the Google `place_id`
> instead. Live calls cost money and are non-deterministic, so they're never part
> of `cargo test`/CI (kept offline via fixtures).

## Resolving by name (local name index)

Names aren't identifiers, so resolving a **name + qualifier(s)** reaches an
external hub the *first* time — then the result is recorded in a **local name
index** so the next identical query is served from the graph with **zero external
calls**:

```sh
# First time: reaches the hub, then caches the name → entity mapping
sameas resolve --name "Blue Bottle Coffee" --type restaurant --city Oakland --region CA --complete
# Again (no --complete, no network): served locally, confidence_reason local_name_match
sameas resolve --name "Blue Bottle Coffee" --type restaurant --city Oakland --region CA
```

`--type` picks **which hub** answers, by NSID leaf (case-insensitive):
`place`/`localBusiness`/`foodEstablishment`/`restaurant` → Google Places Text
Search (the only **billable** hub); `movie`/`tvSeries` → TMDb `/search/multi`;
anything else — **including no `--type` at all** → Wikidata `wbsearchentities`.
The fallback is free on purpose: an unrecognized or missing type degrades to a
worse answer, never to a surprise bill.

```sh
# Ambiguous both ways at once: a different franchise (the Nickelodeon series) and
# a same-franchise sequel. Every candidate label carries the year.
sameas resolve --name Avatar --type movie --complete
sameas resolve --name Avatar --type movie --qualifier 2009 --complete   # → one
```

The qualifier is **type-agnostic** — a free-form facet, not a fixed
city/state/year schema. Use the generic, repeatable `--qualifier` for whatever
disambiguates the entity:

```sh
sameas resolve --name "Yosemite"   --qualifier California --complete   # park: name + state
sameas resolve --name "Joe's Pizza" --qualifier Brooklyn  --complete   # NYC:  name + borough
```

Matching is **exact-normalized** (lowercase/trim) on the name plus **≥1 shared
qualifier token** — deterministic, no fuzzy matching. A name matching several
distinct entities returns them as `candidates` (ambiguous) rather than guessing.
Alias/typo tolerance (e.g. `SF` ↔ `San Francisco`) is a deferred, evidence-gated
layer pointed *inward* at this index.

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
sameas resolve --name "Blue Bottle Coffee" --type restaurant --city Oakland --region CA \
  --country US --complete --hub-fixtures examples/fixtures/hubs
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
