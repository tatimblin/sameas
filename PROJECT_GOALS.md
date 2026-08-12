# Project Goals: `sameas` — Entity Resolution & Completion

## Summary
`sameas` is a standalone, protocol-agnostic library (later an API) that takes
**one piece of partial information about an entity and returns a canonical ID
plus the completed set of linked identifiers**. Give it a restaurant's website,
a Google Place ID, an IMDb page, or a phone number; it resolves that to a stable
canonical entity and hands back everything else it knows about that entity — its
other identifiers (schema.org `sameAs` set) and canonical anchor.

Internally it is an **identity crosswalk graph**: a union-find over typed
external identifiers. The same operation that returns an ID also *completes* the
entity, because resolving an input attaches it to a cluster and returns the whole
cluster.

> **Terminology.** The project is `sameas`. It began as an "embeddings API for
> schema.org sameAs," but embeddings/ML are **not** the core — they are an
> optional, evidence-gated future addition (see Non-goals and the roadmap's
> optional phase). The core is exact-identifier reconciliation.

## Goals
1. **Resolve from partial input** — accept any single identifier (website/domain,
   Google Place ID, IMDb ID, phone number, Wikidata QID, TMDb ID) or a partial
   schema.org record, and return a stable `canonical_id`.
2. **Complete the entity** — return the entity's full linked-identifier set
   (`sameAs`) and canonical anchor, so a caller who knows one identifier learns
   all the others.
3. **Standalone & protocol-agnostic** — vocabulary is schema.org Things +
   external identifiers only. No knowledge of any consumer's protocol (no
   AT-URIs, DIDs, lexicons, firehose). The ATProto project is one consumer.
4. **Compose, don't reinvent** — anchor on existing identity hubs (Wikidata QID,
   Placekey, Google Place ID) rather than minting a new global ID scheme. Build
   only the thin crosswalk + normalization + reverse-resolution layer that no
   existing free service provides.
5. **Explainable & correctable** — every resolution reports which key matched and
   a confidence; bad links can be split, missed links merged.

## Non-goals (explicitly out of scope)
- **Not an embeddings / semantic-search service.** Fuzzy text matching and
  learned embeddings are deferred and *evidence-gated*: built only if the
  measured exact-identifier miss rate proves large (see roadmap optional phase).
  Since the inputs are exact identifiers, the core needs no ML.
- **Not a mass batch-dedup engine.** This is a per-entity lookup, not an
  O(n²) corpus clustering job — so no blocking/clustering/Spark in the core.
- **Not a data warehouse of provider content.** Store only ID-to-ID edges plus a
  minimal canonical anchor; provider content (names/addresses) is transient.
- Not an ATProto service, review aggregator, or UI — those are consumers.

## How it works
```
input (one identifier or partial record)
  │
  ├─ harvest all identifiers present (a domain page yields sameAs[], socials, ...)
  ├─ normalize each key   (URL→registrable domain, phone→E.164, bare place_id, tt…, Q…)
  ├─ look up each key in the crosswalk graph (union-find: external_id → canonical)
  │      • 1 hit            → attach input to that entity
  │      • >1 distinct hits → gated union (never union on phone alone)
  │      • 0 hits           → adopt strongest public anchor, else mint synthetic ID
  └─ return { canonical_id, anchor, sameAs[], matched_via, confidence }
```

### Canonical anchor priority
The canonical ID is the strongest available public anchor, so it stays portable
and meaningful — a synthetic local ID is minted only when none exists:
```
Wikidata QID  >  Placekey (places)  >  registrable domain (orgs)  >  Google place_id  >  synthetic local id
```

### Completion sources
1. **Local graph first** (cheap, offline): once an entity's edges are known, any
   input returns them all. The shared graph compounds — every resolution makes
   future completions richer.
2. **External hubs to bootstrap** when the graph lacks an edge:
   - **Wikidata** (CC0) — `IMDb tt… / website → QID` via SPARQL, then harvest
     P345 (IMDb), P856 (website), P1329 (phone), P4947 (TMDb). One hop completes
     the movie crosswalk for free.
   - **TMDb `Find by ID`** — crosswalks imdb_id ↔ tmdb_id ↔ wikidata_id.
   - **Google Place Details** — `place_id → website, phone`.
   - **Reverse-resolvers** (thin, custom) — `phone/domain → place_id / Placekey`.

## Entity-type strategy
- **Movies — mostly solved by composing free services.** Canonical = Wikidata
  QID (or IMDb `tt`). IMDb ↔ TMDb ↔ Wikidata are fully cross-linked; near-zero ML.
  Never scrape IMDb (ToS) — resolve IMDb IDs via Wikidata/TMDb.
- **Restaurants/places — the genuinely custom part, but small.** No single free
  ID ingests website + place_id + phone. Anchor on place_id or Placekey; the
  custom work is normalizers + reverse-resolvers + a long-tail crosswalk for
  local businesses the public hubs omit.
- **Entity grain rule:** *domain anchors the organization; geo/address
  distinguishes the location.* Prevents merging every chain location into one.

## Interface (library / CLI now; HTTP API later)
```
resolve <identifier | record>   -> { canonical_id, anchor, sameAs[], matched_via, confidence }
entity  <canonical_id>           -> { anchor, sameAs[], members[] }
link    <id_a> <id_b>            -> assert two identifiers are the same entity
merge   <canonical_ids...>       -> combine entities
split   <identifier>             -> detach a mis-linked identifier
```
`resolve` *is* completion — the returned `sameAs[]` is the completed identifier
set. No `embed`/`score` in the public surface.

## Confidence & safety
- **Confidence is a gradient by input.** Strong keys (QID, place_id, IMDb) →
  high; **phone is a corroborator only, never a sole merge anchor** (chains,
  forwarding numbers, reassignment). Completion from a weak key is a hypothesis,
  flagged as such.
- **False-merge bias** is the primary invariant: conservative gated union;
  ambiguous cases create a provisional entity rather than guess; split path for
  corrections.

## Prior art we compose (not rebuild)
- **Wikidata** — free CC0 identity hub; properties P345/P856/P1329/P4947. Gap:
  no Google Place ID and no Placekey property → bridge via website/phone/name.
- **Placekey** — free open ID for physical places, keyed on address/name.
- **TMDb Find-by-ID**, **Google Places** (paid, place_id cacheable).
- ER libraries (Splink/Zingg/dedupe) solve within-your-own-data fuzzy dedup —
  not needed to hit external authorities.

## Success metrics
- % of resolutions answered from an exact key vs. requiring a hub lookup vs.
  unresolved (the **miss rate** — this is the evidence gate for any future fuzzy
  layer).
- Completion coverage: given input key K, how many other identifiers returned.
- False-merge rate (primary safety metric) below target.
- Resolution latency (local-graph vs. hub-bootstrapped).

## Key risks
- **Phone unreliability** — corroborator only.
- **IMDb ToS** — never scrape; resolve via Wikidata/TMDb.
- **Google Places cost + caching** — store IDs-only edges to sidestep most of it.
- **Long-tail coverage** — entities in no public hub need synthetic IDs and are
  where the crosswalk earns its keep.
