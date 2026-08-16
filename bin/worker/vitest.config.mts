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
