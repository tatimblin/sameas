// Ambient types for the test suite.
//
// `@cloudflare/vitest-pool-workers/types` declares `cloudflare:test`; the
// `cloudflare:workers` runtime module (from which the tests import `env` and
// `exports`) comes from `@cloudflare/workers-types/experimental`. Both are pulled
// in by reference here rather than via `compilerOptions.types`, because the pool
// ships its declarations under a subpath export that `types` cannot name.
/// <reference types="@cloudflare/vitest-pool-workers/types" />
/// <reference types="@cloudflare/workers-types/experimental" />

/** The bindings this worker sees under test. No KV — it has only the D1 crosswalk. */
interface TestEnv {
  DB: D1Database;
  /** Supplied by vitest.config.mts's miniflare bindings, not by wrangler.toml. */
  AUTH_TOKEN: string;
  TEST_MIGRATIONS: import("cloudflare:test").D1Migration[];
}

// `cloudflare:test`'s deprecated `env` is typed as `Cloudflare.Env`, so declaring
// that namespace keeps both import styles consistent.
declare namespace Cloudflare {
  interface Env extends TestEnv {}
}

// Narrow `env` to this worker's bindings. `exports` is deliberately NOT
// redeclared: upstream types it as `Cloudflare.Exports`, a
// `Record<string, ExportValue>` **type alias** rather than an interface, so it can
// be neither augmented nor overridden from here. `test/helpers.ts` casts it once at
// the single point of use instead.
declare module "cloudflare:workers" {
  export const env: TestEnv;
}
