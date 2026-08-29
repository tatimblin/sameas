import { beforeEach, expect, it } from "vitest";
import { env } from "cloudflare:workers";
import { post, postAnon, resetDb, scalar } from "./helpers";

// `POST /resolve/name` — the composition point.
//
// The route exists because the disambiguation machinery had no HTTP route: a
// consumer could not reach it, so a chain's brand domain silently minted a
// brand-level entity ("Souvla" the company standing in for one of its
// locations). What is asserted here is the ORCHESTRATION, which is the part no
// core test can cover:
//
//   1. the request's `identifiers`, committed with the STRICT grain rule, then
//   2. on a "needs a stronger identifier" fall-through only, the name search.
//
// No test here reaches a live hub: `vitest.config.mts` answers every outbound
// fetch from a fixture table and 400s anything else.

beforeEach(resetDb);

const BUCKET = "did:test:publisher";

/** Today's budget key, computed the same way `budget::utc_day` does. */
const today = () => String(Math.floor(Date.now() / 86_400_000));

interface Candidate {
  canonical_id: string;
  anchor: string;
  name: string | null;
  url: string | null;
}

interface NameResponse {
  action: string;
  status: string;
  canonical_id: string | null;
  confidence_reason: string;
  hint: string | null;
  identifier_hint: string | null;
  hub_error: string | null;
  sameAs: string[];
  sameAs_urls: string[];
  candidates: Candidate[];
  resolved_by: string;
  name_hub: string | null;
  hub_called: boolean;
  hub_calls_today?: number;
  ignored_identifiers: string[];
}

interface ErrorResponse {
  error: { code: string; message: string };
}

function body(fields: Record<string, unknown>): string {
  return JSON.stringify({ publisher_did: BUCKET, ...fields });
}

async function resolveName(fields: Record<string, unknown>) {
  const res = await post("/resolve/name", body(fields));
  return { res, json: (await res.json()) as NameResponse };
}

const entityCount = () => scalar(`SELECT COUNT(*) AS n FROM entities`);
const logRows = () => scalar(`SELECT COUNT(*) AS n FROM resolutions`);
const budgetUsed = (bucket = BUCKET) =>
  scalar(
    `SELECT COALESCE(SUM(calls), 0) AS n FROM hub_budget WHERE bucket = ?1`,
    bucket,
  );

// --- The gate ---------------------------------------------------------------

it("requires the bearer token", async () => {
  // Reads are open on this worker, but this route reaches PAID hubs and
  // `workers_dev = true` makes it publicly reachable, so an anonymous caller
  // could spend real money.
  const res = await postAnon("/resolve/name", body({ name: "Avatar" }));
  expect(res.status).toBe(401);
  expect((await res.json() as ErrorResponse).error.code).toBe("unauthorized");
  // And it did nothing: no entity, no log row, no budget spent.
  expect(await entityCount()).toBe(0);
  expect(await logRows()).toBe(0);
  expect(await budgetUsed()).toBe(0);
});

it("rejects a wrong token", async () => {
  const res = await post(
    "/resolve/name",
    body({ name: "Avatar" }),
    "wrong-token-entirely",
  );
  expect(res.status).toBe(401);
});

it("400s a body with no quota bucket", async () => {
  const res = await post("/resolve/name", JSON.stringify({ name: "Avatar" }));
  expect(res.status).toBe(400);
  const err = (await res.json()) as ErrorResponse;
  expect(err.error.code).toBe("invalid_input");
  expect(err.error.message).toContain("publisher_did");
  expect(await logRows()).toBe(0);
});

it("400s a body with nothing to resolve", async () => {
  const res = await post(
    "/resolve/name",
    body({ identifiers: ["https://www.facebook.com/souvla"] }),
  );
  expect(res.status).toBe(400);
});

// --- Step 1: the identifiers, under the strict grain rule -------------------

it("a strong identifier resolves and returns sameAs_urls", async () => {
  const { res, json } = await resolveName({
    name: "Souvla",
    entity_type: "restaurant",
    identifiers: ["https://www.yelp.com/biz/souvla-hayes-valley-san-francisco"],
  });

  expect(res.status).toBe(200);
  expect(json.action).toBe("resolve_name");
  expect(json.status).toBe("new");
  expect(json.resolved_by).toBe("identifiers");
  expect(json.canonical_id).toBeTruthy();
  expect(json.sameAs).toContain("yelp:souvla-hayes-valley-san-francisco");
  // `sameAs_urls` is what the consumer writes into the record: a bare
  // `kind:value` fails agent-web's `anchor_is_merge_eligible` (no "://"), so a
  // record carrying only that would never cluster.
  expect(json.sameAs_urls).toContain(
    "https://www.yelp.com/biz/souvla-hayes-valley-san-francisco",
  );
  // No hub was involved at all — this is the free path.
  expect(json.hub_called).toBe(false);
  expect(await budgetUsed()).toBe(0);

  // Idempotent: the same identifier resolves to the same entity, second time as
  // a hit rather than a mint.
  const again = await resolveName({
    identifiers: ["yelp:souvla-hayes-valley-san-francisco"],
  });
  expect(again.json.status).toBe("hit");
  expect(again.json.canonical_id).toBe(json.canonical_id);
  expect(await entityCount()).toBe(1);
});

it("a brand domain PLUS an identity key resolves — the deliberate asymmetry", async () => {
  // The grain rule refuses a bare domain only when it is the SOLE strong key. A
  // co-present Identity key (here a Michelin deep link) rescues it and carries
  // the domain along, which is what keeps the 151 bare-domain seed records in
  // the corpus resolvable when they cite one page.
  const { json } = await resolveName({
    name: "Zuni Café",
    entity_type: "restaurant",
    identifiers: [
      "https://zunicafe.com",
      "https://guide.michelin.com/us/en/california/san-francisco/restaurant/zuni-cafe",
    ],
  });
  expect(json.status).toBe("new");
  expect(json.resolved_by).toBe("identifiers");
  expect(json.sameAs).toContain("domain:zunicafe.com");
  expect(json.sameAs.some((k) => k.startsWith("url:guide.michelin.com"))).toBe(
    true,
  );
});

it("an affiliation-only key over a known cluster is ambiguous, with candidates", async () => {
  // Seed one location the permissive way (`/ingest` is how the corpus loads):
  // a Yelp identity key plus the chain's shared brand domain.
  const seeded = await post(
    "/ingest",
    JSON.stringify({
      name: "Souvla Hayes Valley",
      sameAs: [
        { yelp: "souvla-hayes-valley-san-francisco" },
        { domain: "souvla.com" },
      ],
    }),
  );
  expect(seeded.status).toBe(200);

  // Now the publish path sends only the brand domain — the original bug.
  const { json } = await resolveName({
    name: "Souvla",
    entity_type: "restaurant",
    city: "San Francisco",
    identifiers: ["https://souvla.com"],
  });

  expect(json.status).toBe("unresolved");
  expect(json.confidence_reason).toBe("ambiguous_among_n");
  expect(json.resolved_by).toBe("identifiers");
  // Note the count: ONE. A strong key has exactly one owner and a second chain
  // location never acquires the shared domain, so the identifier step can only
  // ever reach one cluster per key. The multi-location picker is the name
  // search's job, not this step's.
  expect(json.candidates).toHaveLength(1);
  const c = json.candidates[0];
  // Both forms: `anchor` is echoed back verbatim on the retry, `url` is what
  // gets written into the record.
  expect(c.anchor).toBe("yelp:souvla-hayes-valley-san-francisco");
  expect(c.url).toBe(
    "https://www.yelp.com/biz/souvla-hayes-valley-san-francisco",
  );
  // Refused means refused: nothing was minted for the brand.
  expect(await entityCount()).toBe(1);
});

it("echoing a candidate's ref back resolves to that candidate", async () => {
  await post(
    "/ingest",
    JSON.stringify({
      name: "Souvla Hayes Valley",
      sameAs: [
        { yelp: "souvla-hayes-valley-san-francisco" },
        { domain: "souvla.com" },
      ],
    }),
  );
  const first = await resolveName({
    name: "Souvla",
    identifiers: ["https://souvla.com"],
  });
  const ref = first.json.candidates[0].anchor;

  // The retry: the user picked one, the consumer echoes `ref` verbatim.
  const retry = await resolveName({ name: "Souvla", identifiers: [ref] });
  expect(retry.json.status).toBe("hit");
  expect(retry.json.resolved_by).toBe("identifiers");
  // Bound to the existing entity — it did NOT mint a second one.
  expect(await entityCount()).toBe(1);
});

it("the strict grain rule is opt-in: /ingest still accepts a bare domain", async () => {
  // THE REGRESSION GUARD for this route's central footgun. `CommitOpts::default()`
  // is permissive (`allow_affiliation_only: true`) — deliberately, because bulk
  // ingest and seeding are full of domain-only records. `/resolve/name` must pass
  // `false` EXPLICITLY; forgetting the field is a silent return of the original
  // bug, not a compile error. These two assertions differ only in which endpoint
  // sees the identical record.
  const ingested = await post(
    "/ingest",
    JSON.stringify({ name: "Souvla", sameAs: [{ domain: "souvla.com" }] }),
  );
  expect(ingested.status).toBe(200);
  expect((await ingested.json() as NameResponse).status).toBe("new");
  expect(await entityCount()).toBe(1);

  await resetDb();

  const { json } = await resolveName({
    name: "Souvla",
    entity_type: "restaurant",
    identifiers: ["https://souvla.com"],
  });
  expect(json.status).toBe("unresolved");
  expect(json.canonical_id).toBeNull();
  expect(await entityCount()).toBe(0);
});

// --- Step 2: the fall-through to the name search -----------------------------

it("an affiliation-only key with NO cluster falls through to the name search", async () => {
  // The Souvla-on-an-empty-graph case: `needs_stronger_identifier` with an empty
  // candidate list is the frozen fall-through signal. `resolved_by` proves step 2
  // actually ran — both steps can refuse with the same reason, so the reason
  // alone would not distinguish them.
  const { json } = await resolveName({
    name: "Souvla",
    entity_type: "restaurant",
    city: "San Francisco",
    identifiers: ["https://souvla.com"],
  });

  expect(json.status).toBe("unresolved");
  expect(json.resolved_by).toBe("name_search");
  expect(json.name_hub).toBe("google_places");
  expect(json.candidates).toEqual([]);
  // Step 1's refusal reason survives the fall-through. It is the sentence the
  // consumer shows its user ("souvla.com names a brand, not one location"), and
  // the answering step's own `hint` would otherwise be the only thing left.
  expect(json.identifier_hint).toContain("souvla.com");
  expect(json.identifier_hint).toContain("brand");
  expect(await entityCount()).toBe(0);
});

it("a hub whose key is absent is not called, and says so", async () => {
  // Behavior with a key absent must be sane and legible, never a panic and never
  // a doomed request: no outbound call, no budget spent, and a hint naming the
  // secret an operator has to set.
  const { res, json } = await resolveName({
    name: "Souvla",
    entity_type: "restaurant",
    identifiers: ["https://souvla.com"],
  });
  expect(res.status).toBe(200);
  expect(json.hub_called).toBe(false);
  expect(json.hint).toContain("GOOGLE_PLACES_API_KEY");
  expect(await budgetUsed()).toBe(0);
});

it("a free hub IS called, reserves budget, and returns candidates", async () => {
  // No `entity_type` routes to Wikidata — free, keyless, type-agnostic — so this
  // is the one path that reaches a hub under test. The response comes from the
  // fixture table in vitest.config.mts; nothing touches the network.
  const { json } = await resolveName({ name: "Avatar" });

  expect(json.resolved_by).toBe("name_search");
  expect(json.name_hub).toBe("wikidata");
  expect(json.hub_called).toBe(true);
  expect(json.status).toBe("unresolved");
  expect(json.confidence_reason).toBe("ambiguous_among_n");
  expect(json.candidates).toHaveLength(2);
  expect(json.candidates[0].anchor).toBe("wikidata:Q24871");
  expect(json.candidates[0].url).toContain("Q24871");
  // The label carries the disambiguator — without the year, a user cannot tell
  // the 2009 film from the animated series.
  expect(json.candidates[0].name).toContain("2009");

  // Budget was reserved BEFORE the call, once.
  expect(json.hub_calls_today).toBe(1);
  expect(await budgetUsed()).toBe(1);
});

it("a repeat query is answered locally, spending nothing", async () => {
  // The first brake: `resolve_name` writes the ambiguous verdict into the
  // cardinality memory, so the identical query never reaches a hub again.
  await resolveName({ name: "Avatar" });
  expect(await budgetUsed()).toBe(1);

  const { json } = await resolveName({ name: "Avatar" });
  expect(json.resolved_by).toBe("name_local");
  expect(json.hub_called).toBe(false);
  expect(json.confidence_reason).toBe("ambiguous_among_n");
  expect(json.candidates).toHaveLength(2);
  expect(await budgetUsed()).toBe(1);
});

it("a hub that finds nothing is a reported miss, not an error", async () => {
  const { res, json } = await resolveName({ name: "Nothing At All Here" });
  expect(res.status).toBe(200);
  expect(json.status).toBe("unresolved");
  expect(json.confidence_reason).toBe("needs_stronger_identifier");
  expect(json.hub_called).toBe(true);
  expect(await entityCount()).toBe(0);
});

it("a hub that finds nothing reports NO hub_error", async () => {
  // The control for the two tests below. A miss and a failure are both
  // `unresolved` + `needs_stronger_identifier` + zero candidates; `hub_error` is
  // the ONLY thing that separates them, so it has to be absent here.
  const { json } = await resolveName({ name: "Nothing At All Here" });
  expect(json.hub_error).toBeNull();
  expect(json.hint).toContain("supply an identifier");
});

it("a hub that REFUSES us says so, and does not blame the caller", async () => {
  // The staging bug, end to end through the real FetchTransport: the hub is
  // reached, denies the request, and the route used to hand back a confident
  // "supply a stronger identifier" with an empty candidate list — the exact
  // shape a genuine zero-result has.
  const { res, json } = await resolveName({ name: "Forbidden" });

  // Still a 200 with a usable document: a hub outage is non-fatal by contract.
  expect(res.status).toBe(200);
  expect(json.status).toBe("unresolved");
  expect(json.hub_called).toBe(true);
  expect(json.candidates).toHaveLength(0);
  expect(await entityCount()).toBe(0);

  // The classification survives the whole trip — hub tag, HTTP status, error
  // class and body snippet — rather than being flattened to "hub failed".
  expect(json.hub_error).toBeTruthy();
  expect(json.hub_error).toContain("wikidata:");
  expect(json.hub_error).toContain("HTTP 403");
  expect(json.hub_error).toContain("authentication/authorization denied");
  expect(json.hub_error).toContain("Requests from referer");

  // And the hint no longer tells the user to supply a better identifier for a
  // lookup that never happened.
  expect(json.hint).toBeTruthy();
  expect(json.hint?.toLowerCase()).not.toContain("supply an identifier");
  expect(json.hint).toContain("FAILED");
});

// --- The Google Places POST path ---------------------------------------------
//
// Everything above this line that reaches a hub reaches it with a GET (the free
// hubs). Google Places Text Search is a POST carrying an `X-Goog-Api-Key` header,
// an `X-Goog-FieldMask` header and a JSON body — and it is the call the Souvla
// publish actually made. It had NO coverage anywhere: `FixtureTransport` matches
// on the URL and ignores headers, so no core test can see a dropped key header,
// and the Worker suite could not reach the branch at all because no
// `GOOGLE_PLACES_API_KEY` was bound.
//
// `GOOGLE_PLACES_API_KEY` is set per-test rather than in the miniflare bindings so
// the "a hub whose key is absent is not called" test above keeps meaning what it
// says. The stub in vitest.config.mts answers a malformed request the way Google
// would, so a transport bug surfaces as a specific `hub_error`.
async function withPlacesKey<T>(fn: () => Promise<T>): Promise<T> {
  const e = env as unknown as Record<string, string | undefined>;
  const previous = e.GOOGLE_PLACES_API_KEY;
  e.GOOGLE_PLACES_API_KEY = "test-places-key-not-a-real-secret";
  try {
    return await fn();
  } finally {
    if (previous === undefined) delete e.GOOGLE_PLACES_API_KEY;
    else e.GOOGLE_PLACES_API_KEY = previous;
  }
}

it("the Souvla publish returns the five locations to pick from", async () => {
  // The bug this whole feature exists to fix, replayed at the route: a review
  // carrying only the brand domain must come back AMBIGUOUS with a choosable
  // list, not `needs_stronger_identifier` with nothing in it.
  const { json } = await withPlacesKey(() =>
    resolveName({
      name: "Souvla",
      entity_type: "restaurant",
      city: "San Francisco",
      identifiers: ["https://souvla.com"],
    }).then((r) => r),
  );

  // No hub_error: the POST — key header, field mask, JSON body — was built
  // correctly and accepted. This is the assertion that exonerates (or convicts)
  // `FetchTransport::post_json`, which nothing else in either repo exercises.
  expect(json.hub_error).toBeNull();

  expect(json.status).toBe("unresolved");
  expect(json.confidence_reason).toBe("ambiguous_among_n");
  expect(json.candidates).toHaveLength(5);
  // Choosable: the address is what tells one branch of a chain from another.
  expect(json.candidates[0].name).toContain("517 Hayes St");
  expect(json.candidates[0].anchor).toBe("google_place_id:ChIJ_SOUVLA_HAYES");
  expect(json.candidates[0].url).toContain("ChIJ_SOUVLA_HAYES");
  // Step 1's "souvla.com is a brand" sentence still rides along.
  expect(json.identifier_hint).toContain("brand");
  // Refusing to guess means nothing was written.
  expect(await entityCount()).toBe(0);
});

it("a Places key the hub rejects is reported, not silently empty", async () => {
  // The leading hypothesis for the staging failure: a key that works from a
  // laptop and is denied from Cloudflare's edge. The stub 403s any request whose
  // `X-Goog-Api-Key` is not the expected one, so this is that failure exactly.
  const e = env as unknown as Record<string, string | undefined>;
  e.GOOGLE_PLACES_API_KEY = "a-key-google-does-not-accept";
  try {
    const { json } = await resolveName({
      name: "Souvla",
      entity_type: "restaurant",
      identifiers: ["https://souvla.com"],
    });
    expect(json.hub_called).toBe(true);
    expect(json.candidates).toEqual([]);
    expect(json.hub_error).toContain("google_places:");
    expect(json.hub_error).toContain("HTTP 403");
    expect(json.hub_error).toContain("API key missing or not authorized");
    // The response no longer reads as a verdict on the author's identifier.
    expect(json.hint).toContain("FAILED");
    expect(json.hint?.toLowerCase()).not.toContain("supply an identifier");
  } finally {
    delete e.GOOGLE_PLACES_API_KEY;
  }
});

it("a failed hub call still consumes budget — the deliberate no-refund policy", async () => {
  // Reserved before the call and never given back. The counter meters attempts
  // to spend, not answers received: we cannot know what the hub billed, and
  // refunding would make a BROKEN hub the cheapest one to retry against. The
  // cost of the policy is bounded (one day of one bucket's allowance) and is now
  // visible in the same response, via `hub_error`.
  expect(await budgetUsed()).toBe(0);
  const { json } = await resolveName({ name: "Forbidden" });
  expect(json.hub_error).toBeTruthy();
  expect(json.hub_calls_today).toBe(1);
  expect(await budgetUsed()).toBe(1);
});

// --- The budget --------------------------------------------------------------

it("an exhausted bucket is refused before any hub call", async () => {
  const bucket = "did:test:spent";
  await env.DB.prepare(
    `INSERT INTO hub_budget (bucket, day, calls) VALUES (?1, ?2, 999999)`,
  )
    .bind(bucket, today())
    .run();

  const res = await post(
    "/resolve/name",
    JSON.stringify({ publisher_did: bucket, name: "Avatar" }),
  );
  expect(res.status).toBe(429);
  const err = (await res.json()) as ErrorResponse;
  expect(err.error.code).toBe("quota_exhausted");
  // Not a resolution outcome, so it must not skew the miss-rate metric.
  expect(await logRows()).toBe(0);
});

it("the budget is per bucket, so one caller cannot starve another", async () => {
  const spent = "did:test:spent";
  await env.DB.prepare(
    `INSERT INTO hub_budget (bucket, day, calls) VALUES (?1, ?2, 999999)`,
  )
    .bind(spent, today())
    .run();

  const blocked = await post(
    "/resolve/name",
    JSON.stringify({ publisher_did: spent, name: "Avatar" }),
  );
  expect(blocked.status).toBe(429);

  const { res, json } = await resolveName({ name: "Avatar" });
  expect(res.status).toBe(200);
  expect(json.hub_called).toBe(true);
  expect(await budgetUsed()).toBe(1);
  expect(await budgetUsed(spent)).toBe(999999);
});

it("identifier answers and unreachable hubs are never budgeted", async () => {
  // The budget covers exactly one thing: an outbound call. An identifier that
  // resolves locally (either step's cheap path) and a hub that is not configured
  // both cost nothing, which is what keeps a per-caller cap from blocking
  // ordinary publishes.
  await resolveName({
    identifiers: ["https://www.yelp.com/biz/souvla-hayes-valley-san-francisco"],
  });
  await resolveName({
    identifiers: ["yelp:souvla-hayes-valley-san-francisco"],
  });
  await resolveName({
    name: "Souvla",
    entity_type: "restaurant",
    identifiers: ["https://souvla.com"],
  });
  expect(await budgetUsed()).toBe(0);
});

it("reports which identifiers it could not use", async () => {
  const { json } = await resolveName({
    name: "Souvla",
    identifiers: [
      "https://www.facebook.com/souvla",
      "https://www.yelp.com/biz/souvla-hayes-valley-san-francisco",
    ],
  });
  expect(json.status).toBe("new");
  expect(json.ignored_identifiers).toEqual(["https://www.facebook.com/souvla"]);
});

// Runs LAST in this file: it drops a table, and the surrounding tests share one
// D1 instance. It re-creates it before returning, so the file leaves the schema
// exactly as it found it.
it("an unreadable budget table fails CLOSED", async () => {
  await env.DB.exec("DROP TABLE hub_budget");
  try {
    const res = await post(
      "/resolve/name",
      JSON.stringify({ publisher_did: BUCKET, name: "Avatar" }),
    );
    // The opposite of `record_resolution`, which is best-effort: losing a metric
    // is not losing money. Spend that cannot be accounted for must not happen.
    expect(res.status).toBe(503);
    expect((await res.json() as ErrorResponse).error.code).toBe(
      "quota_unavailable",
    );
  } finally {
    await env.DB.exec(
      `CREATE TABLE IF NOT EXISTS hub_budget (bucket TEXT NOT NULL, day TEXT NOT NULL, calls INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (bucket, day));`,
    );
  }
});
