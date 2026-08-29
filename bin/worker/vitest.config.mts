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
    // A hub that answers "nothing" is a legitimate, tested outcome (the route
    // must report a miss, not an error) and still costs no network.
    return { searchinfo: { search }, search: [] };
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
          outboundService(request: Request) {
            const url = new URL(request.url);
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
