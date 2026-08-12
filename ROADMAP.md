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
  `sameas ingest <file|dir>`.
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

**Deliverables**
- **Wikidata adapter** — `IMDb tt… / website → QID` via SPARQL; harvest P345,
  P856, P1329, P4947 into the graph.
- **TMDb Find-by-ID adapter** — crosswalk imdb_id ↔ tmdb_id ↔ wikidata_id.
- **Google Place Details adapter** — `place_id → website, phone`.
- **Reverse-resolvers** — `phone/domain → place_id` (text search) and
  `address/name → Placekey`.
- Confidence gradient + phone-as-corroborator gating in union logic.

**Key components:** hub adapters behind the `Resolver` trait, SPARQL/HTTP
clients, confidence scoring, edge provenance.

**Exit criteria:** an IMDb ID resolves to a QID and completes to website/TMDb
with no prior graph state; a place_id completes to website + phone; unioning on
phone alone is refused.

---

## M3 — Long-tail, corrections & integrity
**Objective:** Handle entities in no public hub, keep the graph correct, and
measure whether a fuzzy layer is ever warranted.

**Deliverables**
- Synthetic local canonical IDs when no public anchor exists.
- Provisional entities for the ambiguous band (don't guess-merge).
- Gated union rules incl. the **entity-grain rule** (domain anchors org;
  geo/address distinguishes location — chains don't collapse).
- `sameas link` / `merge` / `split` with edge re-pointing.
- **Miss-rate instrumentation** — % of inputs answered by exact key vs. hub vs.
  unresolved. This is the evidence gate for the optional fuzzy phase.

**Key components:** synthetic-ID minting, provisional state, correction ops,
integrity constraints, metrics.

**Exit criteria:** a local restaurant with no public ID gets a stable synthetic
canonical; a wrong link is splittable; two chain locations sharing a domain stay
distinct; miss rate is reported.

---

## M4 — HTTP API layer
**Objective:** Expose `sameas-core` over HTTP for non-CLI consumers (e.g. the
ATProto AppView). Pure front-end — no new domain logic.

**Deliverables**
- `sameas-api` (bin): `axum` + `tokio` over the same core.
- Endpoints 1:1 with CLI capability: `POST /resolve`, `GET /entity/{id}`,
  `POST /link`, `POST /merge`, `POST /split`.
- Request/response DTOs, error mapping, config, request logging.

**Exit criteria:** every CLI capability is reachable over HTTP with equivalent
behavior; a consumer resolves a record and reads back the completed entity.

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
**Only if M3's measured miss rate is materially large.** Entities that share no
resolvable identifier but are the same real thing are the *residual* the exact
crosswalk can't close. Address it in order of cost, cheapest first:
1. String similarity on normalized name/address (`strsim`) + geo gate — no model.
2. Only if still insufficient: an embedding model (local ONNX via `ort`) as one
   signal, trained/evaluated against the miss-rate data collected in M3.

This is where the original "embedding" idea lives — demoted from centerpiece to a
conditional enhancement justified by data, not assumption.

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
M1 (crosswalk core, resolve/complete, CLI)
   └▶ M2 (hub bootstrapping)      ── requires M1 graph + resolver trait
          └▶ M3 (long-tail, corrections, miss-rate)  ── requires M1–M2 resolution paths
                 └▶ M5 (hardening)                    ── requires M4 surface + M3 metrics

M4 (HTTP API) ── front-end over sameas-core; can begin once M1 resolve exists, grows through M3.
Optional fuzzy phase ── gated on M3 miss-rate evidence.
```
