import { exports, env } from "cloudflare:workers";

/** Must match the `AUTH_TOKEN` miniflare binding in vitest.config.mts. */
export const AUTH = "test-token-not-a-real-secret";

// `exports` is typed upstream as `Record<string, ExportValue>` (a type alias, so
// not augmentable), and `ExportValue` does not carry `fetch`. Narrow it once here
// rather than casting at every call site.
const worker = exports as unknown as { default: Fetcher };

/** Dispatch through the real entrypoint (`exports.default`, not `SELF`). */
export function dispatch(pathAndQuery: string, init?: RequestInit) {
  return worker.default.fetch(
    new Request(`https://worker.test${pathAndQuery}`, init),
  );
}

/** POST with a bearer token; pass a different token to test rejection. */
export function post(pathAndQuery: string, body: string, token: string = AUTH) {
  return dispatch(pathAndQuery, {
    method: "POST",
    headers: { authorization: `Bearer ${token}` },
    body,
  });
}

/** POST with no Authorization header at all. */
export function postAnon(pathAndQuery: string, body: string) {
  return dispatch(pathAndQuery, { method: "POST", body });
}

/**
 * One scalar from a direct D1 read, bypassing the Rust layer entirely — so an
 * assertion about stored bytes cannot be satisfied by a bug in the read path.
 */
export async function scalar(sql: string, ...binds: unknown[]): Promise<number> {
  const row = await env.DB.prepare(sql)
    .bind(...binds)
    .first<{ n: number }>();
  return row?.n ?? 0;
}

/**
 * Empty every application table, deriving the list from `sqlite_master` rather
 * than hand-maintaining one (agent-web's `truncateAll` names five tables and goes
 * stale the moment a migration adds a sixth).
 *
 * Exclusions:
 * - `d1_migrations` — deleting it makes apply-migrations.ts replay everything.
 * - `sqlite_%` / `_cf_%` — SQLite and D1/miniflare internals.
 *
 * `entities` is deleted LAST: `nodes`, `phone_edges` and `name_index` all
 * reference it (migrations/0001_init.sql). D1 does not enforce foreign keys today,
 * but ordering correctly costs nothing and does not depend on that staying true.
 */
const TABLES_LAST = ["entities"];

export async function resetDb(): Promise<void> {
  const { results } = await env.DB.prepare(
    `SELECT name FROM sqlite_master
      WHERE type = 'table'
        AND name NOT LIKE 'sqlite_%'
        AND name NOT LIKE '_cf_%'
        AND name != 'd1_migrations'`,
  ).all<{ name: string }>();

  const names = results
    .map((r) => r.name)
    .sort((a, b) => {
      const rank = (n: string) => (TABLES_LAST.includes(n) ? 1 : 0);
      return rank(a) - rank(b) || a.localeCompare(b);
    });

  if (names.length === 0) {
    throw new Error("resetDb: no application tables — migrations did not apply");
  }
  // One batch is one transaction, so a partial wipe cannot leave a half-empty
  // graph for the next test to trip over.
  await env.DB.batch(names.map((n) => env.DB.prepare(`DELETE FROM "${n}"`)));
}
