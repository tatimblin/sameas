# Roadmap: `sameas` — Entity Resolution & Completion

Companion to [PROJECT_GOALS.md](./PROJECT_GOALS.md). Each milestone carries
enough detail — objective, deliverables, key components, exit criteria — to spin
up an implementation plan. Milestones are sequential; cross-cutting concerns run
throughout.

## Architecture at a glance
```
input (identifier or partial record)
   │  harvest identifiers ─ normalize keys (PSL domain, E.164 phone, place_id, tt…, Q…)
   ▼
crosswalk graph = union-find over typed external IDs   (SQLite: external_id → canonical_id)
   │      0 hits → adopt public anchor / mint synthetic     >1 hits → gated union
   ▼
bootstrap completion from hubs when an edge is missing:
   Wikidata (SPARQL P345/P856/P1329/P4947) · TMDb Find-by-ID · Google Place Details · reverse-resolvers
   ▼
out: { canonical_id, anchor, sameAs[], matched_via, confidence }

  sameas-core (lib) ──▶ CLI now · HTTP API later (M4)
```
**Delivery: CLI-first.** All logic lives in a reusable `sameas-core` library,
driven by a local `sameas` CLI over a SQLite crosswalk graph. The HTTP API is a
thin front-end added in M4. Store **only ID-to-ID edges** plus a minimal
canonical anchor — never a warehouse of provider content.

**Stack:** Rust; `sameas-core` lib + `sameas-cli` bin; SQLite crosswalk;
`reqwest`/`scraper`/`serde_json` for adapters; `publicsuffix`, `phonenumber` for
normalization; `axum`/`tokio` only at M4. No ML toolchain in the core.

---

## M1 — Crosswalk core + resolve/complete (CLI)
**Objective:** A local `sameas` binary that resolves any known identifier to a
canonical entity and returns the completed identifier set — from the local graph.

**Deliverables** *(implemented)*
- `sameas-core` (lib) + `sameas-cli` (bin) workspace.
- **`KindSpec` registry (`kind.rs`)** — the single source of truth for every
  identifier kind (`tag`, `strong` vs. corroborator, `anchor_rank`, `normalize`,
  optional `url_match`). Kinds shipped: `domain`, `google_place_id`, `imdb`,
  `phone`, `wikidata`, `tmdb`, `yelp`. `model`/`anchor`/`resolve`/seed-JSON/CLI
  all read from it — **adding a kind is one registry entry + a normalizer**.
- Identifier model: spec-backed `ExternalId` + a schema.org record model
  (Place/LocalBusiness, Organization, Movie) with a `sameAs[]` field.
- Normalizers: URL→registrable domain (PSL), phone→E.164, bare `place_id`,
  `tt…` IMDb, `Q…` QID, TMDb, Yelp biz slug.
- Crosswalk graph: union-find persisted in SQLite as `external_id → canonical_id`
  (+ canonical anchor). ID-to-ID edges only; phone edges kept outside the
  union-find (corroborator).
- Resolvers: **DirectRecordResolver** (harvest a record's `sameAs[]`) +
  **DomainResolver** (parse JSON-LD / OpenGraph / `<link rel=canonical>` from a
  fixture, or HTTP behind `live-fetch`). Domain is a plain key by default;
  page-harvesting is opt-in (`--fixture`/`--fetch`).
- Canonical anchor selection driven by each kind's `anchor_rank`
  (QID > Placekey* > domain > place_id > yelp > synthetic). *Placekey reserved.*
- CLI: `sameas resolve` (`--domain`/`--phone`/`--place-id`/`--imdb`/`--input`,
  plus generic `--id kind:value` for any registered kind), `sameas entity <id>`,
  `sameas ingest <file|dir>`. (M3 adds `link`/`merge`/`split`/`stats`.)
- **Demo**: seed records + HTML fixture + `examples/demo.sh` showing one
  identifier in → canonical ID + completed identifiers out, and the same entity
  reached from phone / place_id / domain / yelp.

**Key components:** kind registry, identifier/record model, normalizer library,
union-find over SQLite, resolver adapters, anchor selection, CLI.

**Exit criteria:** given any identifier already in the graph (or harvestable from
a supplied record/domain), the binary returns `canonical_id` + the completed
`sameAs[]` + `matched_via`; **`examples/demo.sh` runs end-to-end and reproduces
its documented output** — the showable result.

---

## M2 — Hub bootstrapping (completion from authorities)
**Objective:** Fill missing edges by reaching external identity hubs, so movies
"just work" and places bootstrap without pre-seeding the graph.

**Deliverables** *(implemented)*
- **Wikidata adapter** — `IMDb tt… / website → QID` via SPARQL; harvest P345,
  P856, P1329, P4947 into the graph. (`hubs/wikidata.rs`)
- **TMDb Find-by-ID adapter** — crosswalk imdb_id ↔ tmdb_id ↔ wikidata_id.
  (`hubs/tmdb.rs`)
- **Google Place Details adapter** — `place_id → website, phone`.
  (`hubs/places.rs`)
- **Reverse-resolvers** — `name/address → Placekey` (`hubs/placekey.rs`, the
  primary path for the name+city input) and `phone/domain → place_id`
  (`hubs/places.rs`, `PlaceTextSearchResolver`). The name+address query resolves
  to a **Placekey anchor** *and* a Google `place_id` (via text search) in one
  record, so completion (website/phone via Place Details) fills in even from a
  cold graph. Standalone `phone → place_id` merging is deferred to M3 (its
  non-merge semantics need provisional entities).
- **Confidence gradient** (`confidence.rs`) — a `0.0`–`1.0` score reflecting the
  input→entity attachment (exact strong key ≈ 0.95; name+city ≈ 0.40; phone-only
  ≈ 0.30). **Phone-as-corroborator gating** is structural (phone lives outside
  the union-find); confidence labels it.
- **Edge provenance** — a `source` column on `nodes`/`phone_edges` records where
  each edge came from (`input`/`wikidata`/`tmdb`/`google_places`/…), surfaced in
  `resolve`/`entity` output.

**Delivery notes.** All hubs sit behind the [`Resolver`] trait and a
`transport::HttpTransport` abstraction — `FixtureTransport` (offline, canned JSON
under `examples/fixtures/hubs`) for CI + demo, `ReqwestTransport` behind the
`live-fetch` feature for real HTTP. Completion is opt-in (`sameas resolve …
--complete`), runs a bounded, idempotent BFS over the cluster, and is
best-effort (an unavailable hub yields no completion, never an error).

**Key components:** hub adapters behind the `Resolver` trait, SPARQL/HTTP
clients, confidence scoring, edge provenance.

**Exit criteria:** an IMDb ID resolves to a QID and completes to website/TMDb
with no prior graph state; a place_id completes to website + phone; unioning on
phone alone is refused.

---

## M3 — Long-tail, corrections & integrity
**Objective:** Handle entities in no public hub, keep the graph correct, and
measure whether a fuzzy layer is ever warranted.

**Deliverables** *(implemented)*
- **Entity-grain rule** *(implemented)* — each kind has a `grain`
  (`Identity`/`Affiliation`/`Weak`, `kind.rs`); a shared **Affiliation** key (a
  chain/studio domain) never merges two entities with disjoint **Identity** keys,
  and anchor selection prefers Identity keys so two locations don't collide on the
  same canonical id. Type-agnostic (works for places, movies, products, …).
- **Resolve-or-refuse** *(implemented)* — resolution requires ≥1 strong key. With
  no strong key (name-only / phone-only) the resolver **refuses**: `canonical_id =
  None`, `status = unresolved`, a `confidence_reason`, and `candidates` for the
  caller to confirm. Strong-key entities with no public anchor get a
  **deterministic** anchor (no random UUID), so they reproduce across runs.
- **Confidence as a control signal** *(implemented)* — a `0.0`–`1.0` score plus a
  `confidence_reason` (`confidence.rs`); a low score says *why* (unique-but-thin,
  `ambiguous_among_n`, `needs_stronger_identifier`) so the calling app can ask its
  end user for a stronger identifier and re-resolve.
- **Ambiguity signal** *(implemented)* — a name/text query matching several
  places returns `candidates` and refuses rather than guess-merging.
- **Correction ops** *(implemented)* — `sameas link` / `merge` / `split` over the
  same core (`correct.rs`). `link` asserts two keys name one entity
  (create/attach/merge as needed); `merge` collapses entities keeping the
  strongest anchor and re-anchoring over the union; `split` detaches named
  strong keys onto a fresh entity — **the only recovery path for a false merge**.
  A **same-kind identity-conflict guard** refuses `merge`/`link` that would join
  two distinct locations/films (two `google_place_id`s, two QIDs) unless
  `--force`; cross-kind links (place_id ↔ QID) are exactly what the operator
  means and pass. Weak (phone) keys are rejected as a link/split basis
  (corroborator only).
- **Miss-rate instrumentation** *(implemented)* — `sameas stats` over an
  append-only `resolutions` log (`graph.rs`); every user-facing `resolve` records
  its finalized `(status, confidence_reason, matched_via, confidence)`. `stats`
  buckets outcomes into **exact / hub / miss** and reports the headline **miss
  rate** — the evidence gate for the optional fuzzy phase. Logging is best-effort
  (never fails a resolve); `entity`/`ingest` are excluded (not user queries).

**Key components:** grain-gated union, deterministic anchors, resolve-or-refuse,
confidence reason + candidates, correction ops (`correct.rs`), miss-rate log +
`stats`.

**Exit criteria:** two chain locations sharing a domain stay distinct *(met)*; a
place with only a name/city returns "needs a stronger identifier" rather than a
guessed merge *(met)*; a strong-key entity with no public anchor reproduces its
canonical id *(met)*; a false merge can be undone with `split` and the two
entities reproduce their own anchors *(met)*; the miss rate is reported by
`sameas stats` *(met)*.

> **Note on synthetic IDs.** The original wording ("a local restaurant with no
> public ID gets a stable synthetic canonical") was refined by a product decision:
> rather than mint a name-hash synthetic (which risks merging two same-name
> places), hub-less physical places anchor on a **Placekey** derived from their
> address (a strong key), and inputs with nothing resolvable are refused with a
> "needs more info" signal. Fuzzy/name-based matching stays deferred.

---

## M4 — HTTP API layer *(partially shipped)*
**Objective:** Expose `sameas-core` over HTTP for non-CLI consumers (e.g. the
ATProto AppView). Pure front-end — no new domain logic.

**Deliverables**
- ~~`sameas-api` (bin): `axum` + `tokio`~~ — **superseded.** The front-end is
  `bin/worker`, a Cloudflare Worker (Rust→WASM) over `store::d1::D1Store`, since
  the consumer is itself a Worker and a same-account **service binding** removes
  both the egress hop and the need to expose a public origin. `axum`/`tokio`
  never entered the tree.
- Shipped: `GET /resolve` (per-kind and `?id=kind:value`), `GET /entity/{id}`,
  `GET /stats`, `POST /ingest` (token-gated), and `POST /resolve/name`
  *(token-gated)* — the disambiguation route: a strict-grain commit over the
  caller's identifiers, falling through to a hub-routed name search that returns
  **candidates** rather than guessing which location/work was meant.
- Still open: `POST /link` / `/merge` / `/split` — the correction ops exist in the
  core (`correct.rs`) and are reachable only from the CLI.
- Hub API keys (`GOOGLE_PLACES_API_KEY`, `TMDB_API_KEY`, `PLACEKEY_API_KEY`) are
  Worker secrets, per environment. `/resolve/name` is the only route that can
  spend: bearer-gated, with a per-caller daily call budget (`hub_budget`;
  `HUB_DAILY_BUDGET = "0"` is the kill switch) and local-first caching in front of
  every hub.

**Exit criteria:** every CLI capability is reachable over HTTP with equivalent
behavior; a consumer resolves a record and reads back the completed entity.
*(Resolution, entity reads, ingest, stats and name disambiguation: met. The
correction ops: not yet.)*

---

## M5 — Hardening & operability
**Objective:** Production-ready compliance, limits, observability.

**Deliverables**
- Provider ToS/cost discipline: place_id stored (allowed), other Google/IMDb
  content transient with TTL; IDs-only persistence enforced.
- API-key auth, rate limits, quotas (M4 surface).
- Monitoring: latency, exact-key vs. hub-lookup ratio, miss rate, completion
  coverage, false-merge alerts.
- Hub-cache refresh policy (e.g. re-check place_id after 12 months).

**Exit criteria:** ToS-compliant storage verified; dashboards live; hub lookups
are cached and refreshed within policy.

---

## Optional (evidence-gated) — Fuzzy matching
**Only if M3's measured miss rate is materially large** — and now that
`sameas stats` exists, that is a number, not a guess. The gate reads the log:
run the corpus, read `sameas stats`, and only proceed if the **miss** bucket is
both large *and* dominated by `needs_stronger_identifier` on inputs that are
genuinely "same entity, different spelling" (as opposed to `ambiguous_among_n`,
which wants better disambiguation, or thin hub coverage, which wants more hubs —
neither is fixed by fuzzy matching). Entities that share no resolvable identifier
but are the same real thing are the *residual* the exact crosswalk can't close.
Address it in order of cost, cheapest first:
1. **String similarity, no model** — `strsim` (Jaro-Winkler / edit distance) on
   normalized name/address, **gated by geography** (the geo gate substitutes for
   the identity signal exact keys provided). Likely closes most of the residual.
2. **Only if still insufficient: embeddings.** A local ONNX model (via `ort`)
   produces name/address embeddings as **one signal among several**, trained and
   evaluated against the miss-rate data captured by `stats` in M3.
   - **Vector store: `sqlite-vec`** (or similar) — the natural fit here: it loads
     into the *same* SQLite file (no separate service, preserving the CLI-first,
     one-file ethos), and at per-entity-lookup scale its exact brute-force KNN is
     ample (no ANN index needed). Store a geohash/S2 cell as a partition column so
     KNN runs only within nearby cells — the geo gate at retrieval time.
   - **Retrieval, not decision.** `sqlite-vec` returns nearest *candidates*; they
     flow into the existing conservative machinery (confidence + gated union +
     `split` escape hatch) exactly like every other candidate. Fuzzy matching
     never silently unions — it proposes, the gate disposes.
   - **Note:** an embedding is a lossy derivation of name/address content, so this
     is a *deliberate* relaxation of "store IDs, not content" (far less exposure
     than raw provider strings, but a decision to make explicitly, not by drift).

This is where the original "embedding" idea lives — demoted from centerpiece to a
conditional enhancement justified by data, not assumption. **Nothing here is built
until `stats` justifies it.**

---

## Cross-cutting (all milestones)
- **Compose, don't rebuild** — anchor on Wikidata/Placekey/place_id; build only
  the crosswalk + normalization + reverse-resolution layer.
- **Store IDs, not content** — ID-to-ID edges + anchor only; sidesteps most ToS
  exposure.
- **False-merge safety** — gated union, phone as corroborator only, provisional
  entities, split path.
- **CLI-first / reusable core** — all logic in `sameas-core`; CLI now, HTTP API
  (M4) attaches with no domain-logic change.
- **Explainability** — every resolution returns `matched_via` + confidence.
- **Extensible by registry** — new identifier kinds are one `KindSpec` entry +
  a normalizer (`kind.rs`); no enum edits, per-kind CLI flags, or deserialization
  changes. Yelp was added this way as the first registry-added kind.
- **Evidence before ML** — no fuzzy/embedding work until the miss rate justifies it.

## Dependency order
```
M1 (crosswalk core, resolve/complete, CLI)                    ── done
   └▶ M2 (hub bootstrapping)      ── requires M1 graph + resolver trait   ── done
          └▶ M3 (long-tail, corrections, miss-rate)  ── requires M1–M2 paths ── done
                 ├▶ M4 (HTTP API)   ── requires M3 correction ops in core (link/merge/split)
                 │                     so the endpoints stay "no new domain logic"
                 └▶ M5 (hardening)  ── requires M4 surface + M3 miss-rate metrics

Optional fuzzy phase ── gated on M3's *measured* miss rate (`sameas stats`), not assumption.
```
M4's `POST /link|/merge|/split` are a thin front-end over M3's correction ops —
which is why those ops landed in the core (`correct.rs`) as part of closing M3,
before M4 begins.
