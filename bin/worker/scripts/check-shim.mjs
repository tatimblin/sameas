// Deploy guard: fail if the built artifact still carries the test-only route.
// Wired into `npm run deploy` after `wrangler deploy` (which rebuilds via the
// [build] block WITHOUT WORKER_BUILD_FEATURES), so a test-flavored artifact can
// never ship unnoticed.
import path from "node:path";
import { fileURLToPath } from "node:url";
import { assertNoTestEndpoints } from "./build.mjs";

const workerDir = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
assertNoTestEndpoints(workerDir);
console.log("check-shim: ok — wasm exports bound, no test endpoints");
