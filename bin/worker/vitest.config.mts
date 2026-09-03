import path from "node:path";
import { fileURLToPath } from "node:url";
import { readD1Migrations, cloudflareTest } from "@cloudflare/vitest-pool-workers";
import { defineConfig } from "vitest/config";
import { buildAndVerify } from "./scripts/build.mjs";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

// The crosswalk schema lives at the WORKSPACE root, shared with
// `wrangler d1 migrations apply` (wrangler.toml: migrations_dir = "../../migrations").
// `readD1Migrations` does not read `migrations_dir`, so this path could drift from
// the deployed one — `buildAndVerify` asserts the two agree.
const MIGRATIONS_DIR = path.join(__dirname, "../../migrations");

/**
 * The `GOOGLE_PLACES_API_KEY` bound below. Not a secret and not a real key — the
 * outbound stub only ever compares it to itself, and nothing in this config can
 * reach the network. Its job is to prove the key travels from the Worker secret,
 * through `PlaceTextSearchResolver`, into an `X-Goog-Api-Key` header that actually
 * arrives — the one link in that chain no fixture test can check, because
 * `FixtureTransport` matches on the URL and ignores headers entirely.
 */
const PLACES_TEST_KEY = "test-places-key-not-a-real-secret";

/** The five SF Souvla locations: `[place_id, displayName, formattedAddress]`. */
const SOUVLA_PLACES: [string, string, string][] = [
  ["ChIJ_SOUVLA_HAYES", "Souvla", "517 Hayes St, San Francisco, CA 94102"],
  ["ChIJ_SOUVLA_DIVIS", "Souvla", "531 Divisadero St, San Francisco, CA 94117"],
  ["ChIJ_SOUVLA_VALENCIA", "Souvla", "758 Valencia St, San Francisco, CA 94110"],
  ["ChIJ_SOUVLA_MARINA", "Souvla", "2272 Chestnut St, San Francisco, CA 94123"],
  ["ChIJ_SOUVLA_FIDI", "Souvla", "101 California St, San Francisco, CA 94111"],
];

/**
 * Canned hub responses, keyed by the URL the adapter builds. `null` means "no
 * fixture" — see `outboundService` below, which turns that into a 400.
 *
 * Kept deliberately small: the hub *parsers* are unit-tested in
 * `sameas-core` against their own fixtures. What these exist for is the part
 * only the Worker can exercise — that `POST /resolve/name` routes to a hub at
 * all, reserves budget before calling it, and turns the answer into candidates.
 *
 * `Avatar` with no `entity_type` routes to Wikidata (`name_hub_for`'s free,
 * type-agnostic fallback), which is why the fixture is a `wbsearchentities`
 * response. The two items are the plan's ambiguity cases (b) — unrelated
 * entities colliding on a name — and (c) — one franchise, many works.
 */
function hubFixture(url: URL): Record<string, unknown> | null {
  if (url.hostname === "query.wikidata.org") {
    return sparqlFixture(url.searchParams.get("query") ?? "");
  }
  if (
    url.hostname === "www.wikidata.org" &&
    url.searchParams.get("action") === "wbsearchentities"
  ) {
    const search = (url.searchParams.get("search") ?? "").toLowerCase();
    if (search === "avatar") {
      return {
        searchinfo: { search: "Avatar" },
        search: [
          {
            id: "Q24871",
            label: "Avatar",
            description: "2009 film by James Cameron",
          },
          {
            id: "Q104123",
            label: "Avatar: The Last Airbender",
            description: "American animated television series",
          },
        ],
      };
    }
    if (search === "uber") {
      // The organization case, and the reason the type gate exists: Wikidata's
      // own ranking hands back a company, an album and a preposition.
      return {
        searchinfo: { search: "Uber" },
        search: [
          {
            id: "Q17431399",
            label: "Uber",
            description: "American transportation network company",
          },
          {
            id: "Q7877036",
            label: "Uber",
            description: "1998 album by Nomeansno",
          },
          { id: "Q2475886", label: "Über", description: "German preposition" },
        ],
      };
    }
    // A hub that answers "nothing" is a legitimate, tested outcome (the route
    // must report a miss, not an error) and still costs no network.
    return { searchinfo: { search }, search: [] };
  }
  return null;
}

/**
 * Canned SPARQL answers for the ORGANIZATION path, matched on the query text.
 *
 * Three different queries reach `query.wikidata.org` for one org resolution and
 * they are told apart by shape, not by a URL: the encoded SPARQL string IS the
 * request, and hand-writing three exact encodings here would break on any
 * whitespace change in the adapters. What each one is for:
 *
 *   * `P31/P279*` — the type gate. `wbsearchentities` ranks by relevance, so
 *     "Uber" comes back as a company, an album and a German preposition; this says
 *     only the company is organization-shaped.
 *   * `VALUES ?anysite` + `?itemLabel` — the website reverse lookup: who publishes
 *     `uber.com`.
 *   * `VALUES ?item { wd:… }` — the forward crosswalk, whose P856 is the payload
 *     the whole path exists to fetch (the QID/origin pair a consumer clusters on).
 */
function sparqlFixture(query: string): Record<string, unknown> | null {
  const item = (qid: string, extra: Record<string, unknown> = {}) => ({
    item: { value: `http://www.wikidata.org/entity/${qid}` },
    ...extra,
  });
  if (query.includes("wdt:P31/wdt:P279*")) {
    // Of whatever was asked about, only Uber the company is an organization.
    return {
      results: {
        bindings: query.includes("wd:Q17431399") ? [item("Q17431399")] : [],
      },
    };
  }
  if (query.includes("VALUES ?anysite") && query.includes("?itemLabel")) {
    if (!query.includes("uber.com")) {
      return { results: { bindings: [] } };
    }
    return {
      results: {
        bindings: [
          item("Q17431399", {
            itemLabel: { value: "Uber" },
            itemDescription: {
              value: "American transportation network company",
            },
          }),
        ],
      },
    };
  }
  if (query.includes("VALUES ?item { wd:Q17431399 }")) {
    return {
      results: {
        bindings: [
          item("Q17431399", { website: { value: "https://www.uber.com/" } }),
        ],
      },
    };
  }
  return null;
}

export default defineConfig({
  plugins: [
    cloudflareTest(async () => {
      // Build the Rust→WASM worker HERE rather than in an npm script, so every
      // entry point tests fresh code, and assert the shim binds its wasm exports.
      // See scripts/build.mjs for why both of those must happen Node-side.
      await buildAndVerify({ dir: __dirname, migrationsDir: MIGRATIONS_DIR });

      return {
        // Nothing here is a remote binding and a test run must never make a
        // Cloudflare API call. The pool defaults this to `true`; it filters to
        // zero bindings for this config anyway, but an explicit `false` deletes
        // the code path rather than trusting that to stay true.
        remoteBindings: false,
        wrangler: { configPath: "./wrangler.toml" },
        miniflare: {
          // EVERY outbound fetch the worker makes is answered from here. Two
          // reasons this is not optional:
          //
          // 1. `POST /resolve/name` reaches identity hubs, and Google Places is
          //    BILLABLE (Enterprise SKU). A test run must not be able to spend
          //    money — or to depend on a third party's uptime for a green build.
          // 2. The core's hub adapters are best-effort by contract (a hub error
          //    is a miss, never a failure), so a leaked live call would not fail
          //    loudly. It would silently pass, having really called out.
          //
          // Anything without a fixture gets a 400 — non-transient, so
          // `FetchTransport` does not burn its retry budget on it — and the body
          // names the URL, so an unfixtured call reads as a clear miss instead of
          // a mystery. This is the only network any test can see.
          async outboundService(request: Request) {
            const url = new URL(request.url);
            // Google Places Text Search (New) — the ONLY POST-with-headers-and-body
            // hub, and until now the only `FetchTransport` method with no coverage
            // at all (the free hubs are GETs, and the Places branch was unreachable
            // in tests because no `GOOGLE_PLACES_API_KEY` was bound). That gap is
            // exactly where the Souvla staging failure lives.
            //
            // This stub VALIDATES rather than just answering: it checks the four
            // things the transport has to get right and, when one is wrong, replies
            // the way Google itself would. A `FetchTransport` bug therefore shows up
            // as a specific `hub_error` in the test output instead of a silent
            // empty list — which is the whole point of the change under test.
            if (url.href === "https://places.googleapis.com/v1/places:searchText") {
              const deny = (status: number, msg: string) =>
                new Response(
                  JSON.stringify({ error: { status: "DENIED", message: msg } }),
                  { status, headers: { "content-type": "application/json" } },
                );
              if (request.method !== "POST") {
                return deny(405, `expected POST, got ${request.method}`);
              }
              // Google 403s a request whose key header never arrived. So do we —
              // if the header is dropped in `FetchTransport::build`, the test sees
              // the same failure production would.
              if (request.headers.get("x-goog-api-key") !== PLACES_TEST_KEY) {
                return deny(403, "API key missing or not authorized");
              }
              // A missing/blank field mask is a 400 INVALID_ARGUMENT at Google, and
              // is invisible to every fixture test (FixtureTransport ignores headers).
              // NB `X-Goog-FieldMask` lowercases to `x-goog-fieldmask` — one
              // word, no hyphen before "mask".
              if (!request.headers.get("x-goog-fieldmask")) {
                return deny(400, "X-Goog-FieldMask is required for this method");
              }
              let parsed: unknown;
              try {
                parsed = await request.json();
              } catch {
                return deny(400, "body was not JSON");
              }
              const q = (parsed as { textQuery?: unknown }).textQuery;
              if (typeof q !== "string" || q.length === 0) {
                return deny(400, `textQuery missing: ${JSON.stringify(parsed)}`);
              }
              if (!q.toLowerCase().includes("souvla")) {
                return Response.json({});
              }
              // The five SF locations the hand-replay returns. `formattedAddress`
              // is what makes them choosable without a Place Details call each.
              return Response.json({
                places: SOUVLA_PLACES.map(([id, name, address]) => ({
                  id,
                  displayName: { text: name },
                  formattedAddress: address,
                })),
              });
            }
            // A hub that is REACHABLE but REFUSES us — the failure mode the
            // `hub_error` plumbing exists for, and the one that is otherwise
            // indistinguishable from a genuine zero-result. 403 rather than 5xx on
            // purpose: `FetchTransport` treats 403 as non-transient, so this is
            // exactly one outbound attempt and the test does not depend on retry
            // timing. Body shaped like Google's PERMISSION_DENIED so the assertion
            // about the snippet reaching the caller is about a realistic string.
            if (
              url.hostname === "www.wikidata.org" &&
              (url.searchParams.get("search") ?? "").toLowerCase() === "forbidden"
            ) {
              return new Response(
                JSON.stringify({
                  error: {
                    status: "PERMISSION_DENIED",
                    message: "Requests from referer <empty> are blocked.",
                  },
                }),
                { status: 403, headers: { "content-type": "application/json" } },
              );
            }
            const fixture = hubFixture(url);
            if (fixture) {
              return Response.json(fixture);
            }
            return new Response(
              `no hub fixture for ${url.href} — tests never call a live hub`,
              { status: 400 },
            );
          },
          bindings: {
            TEST_MIGRATIONS: await readD1Migrations(MIGRATIONS_DIR),
            // `router.rs` reads this via `env.secret("AUTH_TOKEN")`. workers-rs
            // 0.5 aliases Secret to the same StringBinding as Var, so a plain
            // miniflare string satisfies it.
            //
            // Committed deliberately, not kept in `.dev.vars`: the value only
            // ever reaches a throwaway temp-dir SQLite, and hard-coding it means
            // `npm ci && npm test` works on a bare checkout and in CI with no
            // secrets. Do NOT move this to wrangler.toml [vars] — that would
            // ship a token to production.
            AUTH_TOKEN: "test-token-not-a-real-secret",
          },
        },
      };
    }),
  ],
  test: {
    setupFiles: ["./test/apply-migrations.ts"],
    // Every test file shares ONE Miniflare D1 (one instance per Vitest project;
    // D1 is a Durable Object keyed by database_id), and `isolatedStorage` does
    // not exist in vitest-pool-workers 0.16.3. Parallel files would interleave
    // one file's resetDb() with another's writes, so `beforeEach(resetDb)` alone
    // is not enough.
    fileParallelism: false,
  },
});
