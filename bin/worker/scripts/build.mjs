// Pre-flight for the Vitest run: build the Rust→WASM worker, then assert three
// things about the artifact that no test inside workerd can check.
//
// This lives Node-side because the tests themselves run *inside* workerd, where
// there is no filesystem — a `shim.test.ts` that reads `build/worker/shim.mjs`
// is impossible. Anything asserting over shim.mjs, wrangler.toml, or migrations/
// has to happen here, at config load.

import { execFileSync } from "node:child_process";
import { readFileSync, readdirSync } from "node:fs";
import path from "node:path";

/**
 * BUG 1 GUARD. `worker-build` 0.1.1 post-processes wasm-bindgen's JS output into
 * the Worker shim, and wasm-bindgen 0.2.101+ changed that codegen to an
 * init-function form worker-build never calls — producing a shim whose wasm
 * exports are never bound. It builds and deploys CLEANLY, then throws
 * "Cannot read properties of undefined (reading 'fetch')" on EVERY request,
 * including ones that touch nothing.
 *
 * Asserts the POSITIVE invariant (`new WebAssembly.Instance(`), not the broken
 * form's marker: that output is minified and its identifiers change every build.
 *
 * @returns {string} the shim source, so callers can make further assertions
 */
export function checkShim(dir) {
  const shimPath = path.join(dir, "build/worker/shim.mjs");
  const shim = readFileSync(shimPath, "utf8");
  if (!/new WebAssembly\.Instance\(/.test(shim)) {
    throw new Error(
      `${shimPath} never instantiates the wasm module, so its exports are ` +
        `unbound and every request will fail with "Cannot read properties of ` +
        `undefined (reading 'fetch')".\n\n` +
        `This is what wasm-bindgen >=0.2.101 does to worker-build 0.1.1's shim ` +
        `codegen. Check that bin/worker/Cargo.toml still pins ` +
        `wasm-bindgen = "=0.2.100".`,
    );
  }
  return shim;
}

/**
 * A byte sequence that is present in a `--features test-endpoints` build of
 * `index.wasm` and absent from a plain one.
 *
 * **It is NOT the route path.** `"__conformance"` — the obvious choice, and what
 * this guard used until it was tested — never appears in the wasm at all, so the
 * guard passed a test-flavored build that answered `POST /__conformance` with 200.
 * Route-matching literals are compared as byte slices with known lengths, and none
 * of `/__conformance`, `/stats`, or `/ingest` survives as a searchable string. (A
 * grep for `/resolve` DOES hit — on the source path `.../src/resolve.rs`, not the
 * route. That near-miss is how the original check looked like it worked.)
 *
 * `conformance_failed` is the `error_json` code in the `#[cfg]`-gated
 * `handlers::conformance`, and it survives because error codes are formatted into
 * a JSON body rather than length-compared. Verified in both directions: absent
 * from a clean build, present in a test build.
 *
 * If this ever needs changing, re-verify by BUILDING BOTH WAYS and grepping —
 * `assert_no_test_endpoints_rejects_a_test_build` in `test/guard.test.ts` pins it.
 */
const TEST_ENDPOINT_MARKER = "conformance_failed";

/**
 * Fail if a test-only route leaked into a deployable artifact. The
 * `/__conformance` route is `#[cfg(feature = "test-endpoints")]` and that feature
 * is passed only by the vitest config — but a stale `build/` from a test run could
 * otherwise be deployed as-is, and that route MUTATES (it merges and splits
 * fixtures).
 *
 * Checks `index.wasm`, NOT `shim.mjs`: the marker is compiled into the wasm (the
 * shim only forwards `fetch`), so grepping the JS silently passes a test-flavored
 * artifact. Also runs `checkShim` so one call covers both invariants.
 */
export function assertNoTestEndpoints(dir) {
  checkShim(dir);
  const wasmPath = path.join(dir, "build/worker/index.wasm");
  // Read as latin1 so byte sequences survive without utf-8 replacement.
  const wasm = readFileSync(wasmPath, "latin1");
  if (wasm.includes(TEST_ENDPOINT_MARKER)) {
    throw new Error(
      `${wasmPath} contains the test-only /__conformance route ` +
        `(marker ${JSON.stringify(TEST_ENDPOINT_MARKER)}).\n` +
        `It was built with --features test-endpoints (i.e. by the test harness). ` +
        `Rebuild without WORKER_BUILD_FEATURES — \`npm run build\` — before deploying.`,
    );
  }
}

/** Exported for the guard's own test. */
export { TEST_ENDPOINT_MARKER };

/**
 * `readD1Migrations` takes an explicit path and never reads wrangler's
 * `migrations_dir`, so the tested schema and the deployed schema can drift
 * silently. Keep them provably equal.
 */
function checkMigrationsPath(dir, migrationsDir) {
  const toml = readFileSync(path.join(dir, "wrangler.toml"), "utf8");
  const declared = /^\s*migrations_dir\s*=\s*"([^"]+)"/m.exec(toml)?.[1];
  if (!declared) {
    throw new Error("wrangler.toml declares no migrations_dir");
  }
  const deployPath = path.resolve(dir, declared);
  if (path.resolve(migrationsDir) !== deployPath) {
    throw new Error(
      `Migrations path drift: vitest reads ${migrationsDir}, but wrangler ` +
        `deploys ${deployPath} (migrations_dir = "${declared}"). The tests ` +
        `would run against a different schema than production.`,
    );
  }
  const sql = readdirSync(deployPath).filter((f) => f.endsWith(".sql"));
  if (sql.length === 0) {
    throw new Error(`No .sql migrations found in ${deployPath}`);
  }
}

/**
 * Build + verify. Called from `vitest.config.mts` so EVERY entry point
 * (`vitest`, `vitest run one.test.ts`, watch mode, IDE runners) tests a fresh
 * binary — the vitest pool resolves the worker via wrangler.toml's `main` with no
 * freshness check, and wrangler's `[build]` block is honored only by
 * `dev`/`deploy`. Making the build a shell `&&` prefix on one npm script (as
 * agent-web does) leaves every other entry point silently testing stale wasm.
 *
 * `SAMEAS_SKIP_BUILD=1` skips the cargo build when iterating on TS only.
 */
export async function buildAndVerify({ dir, migrationsDir }) {
  checkMigrationsPath(dir, migrationsDir);
  if (process.env.SAMEAS_SKIP_BUILD !== "1") {
    execFileSync("./custom_build.sh", {
      cwd: dir,
      stdio: "inherit",
      // Enables the #[cfg(feature = "test-endpoints")] /__conformance route.
      // Never set for `wrangler deploy`, so production never carries it.
      env: { ...process.env, WORKER_BUILD_FEATURES: "--features test-endpoints" },
    });
  }
  checkShim(dir);
}
