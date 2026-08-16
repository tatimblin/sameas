// Proves the deploy guard has teeth, by building BOTH ways and asserting the
// guard's verdict flips.
//
// This exists because the guard was vacuous and nothing noticed: it grepped
// index.wasm for "__conformance", which never appears there, so it passed a
// test-flavored build whose POST /__conformance answered 200. A guard that cannot
// fail is worse than no guard — it reads as proof.
//
// Not a vitest test: it needs two full `worker-build` runs (~1-2 min) and vitest
// runs inside workerd with no filesystem or subprocess access. Wired into CI and
// runnable as `npm run verify:guard`.
//
// Leaves a CLEAN production build behind, so it is safe to run before a deploy.
import { execFileSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { assertNoTestEndpoints, TEST_ENDPOINT_MARKER } from "./build.mjs";

const workerDir = path.dirname(path.dirname(fileURLToPath(import.meta.url)));

function build(features) {
  const env = { ...process.env };
  if (features) env.WORKER_BUILD_FEATURES = features;
  else delete env.WORKER_BUILD_FEATURES;
  execFileSync("./custom_build.sh", { cwd: workerDir, env, stdio: "pipe" });
}

function verdict() {
  try {
    assertNoTestEndpoints(workerDir);
    return "pass";
  } catch (e) {
    return `reject: ${e.message.split("\n")[0]}`;
  }
}

let failures = 0;

// 1. A test-flavored build MUST be rejected. This is the assertion the old guard
//    silently failed.
build("--features test-endpoints");
const withFeature = verdict();
if (withFeature === "pass") {
  console.error(
    `FAIL: guard PASSED a --features test-endpoints build.\n` +
      `      The marker ${JSON.stringify(TEST_ENDPOINT_MARKER)} is not present in ` +
      `index.wasm, so the guard is vacuous.\n` +
      `      Build both ways, grep for a string that differs, and update ` +
      `TEST_ENDPOINT_MARKER in scripts/build.mjs.`,
  );
  failures++;
} else {
  console.log(`ok: test build rejected (${withFeature})`);
}

// 2. A clean build MUST pass — otherwise the guard blocks every deploy and would
//    be disabled by the next person who hits it.
build(null);
const clean = verdict();
if (clean !== "pass") {
  console.error(`FAIL: guard rejected a CLEAN production build.\n      ${clean}`);
  failures++;
} else {
  console.log("ok: clean build accepted");
}

if (failures > 0) process.exit(1);
console.log("verify-guard: ok — the deploy guard rejects test builds and accepts clean ones");
