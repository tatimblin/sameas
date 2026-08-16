// Applies `migrations/*.sql` to the test D1. Runs once per test FILE
// (`setupFiles` semantics) against a database shared by every file.
//
// Safe to re-run: `applyD1Migrations` creates `d1_migrations` IF NOT EXISTS, reads
// the applied names, and skips them. That is why `resetDb()` in test/helpers.ts
// must never delete from `d1_migrations` — doing so would replay every migration
// on the next file, which is harmless for today's `IF NOT EXISTS` DDL but would
// hard-fail the moment a migration adds an `ALTER TABLE`.
import { applyD1Migrations } from "cloudflare:test";
import { env } from "cloudflare:workers";

await applyD1Migrations(env.DB, env.TEST_MIGRATIONS);
