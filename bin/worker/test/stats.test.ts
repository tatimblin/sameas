import { beforeEach, expect, it } from "vitest";
import { env } from "cloudflare:workers";
import { dispatch, post, resetDb, scalar } from "./helpers";

// BUG 3 REGRESSION.
//
// The Worker never called `record_resolution`, so `/stats` always reported zero —
// making the miss-rate metric structurally undecidable. That metric is the
// documented evidence gate for ever adding a fuzzy-matching layer, so a silently
// empty log would make the decision unmeasurable rather than merely wrong.
//
// These also turn the exclusion rule into an assertion: `/resolve` logs,
// `/entity` and `/ingest` deliberately do not (a direct id lookup and a seed load
// are not user-facing queries and would skew the rate).

beforeEach(resetDb);

const rows = () => scalar(`SELECT COUNT(*) AS n FROM resolutions`);

interface Stats {
  total: number;
  exact: number;
  hub: number;
  miss: number;
  miss_rate: number;
  by_reason: Record<string, number>;
  entities: number;
  edges: number;
}

it("/resolve writes exactly one resolutions row, with the right values", async () => {
  expect(await rows()).toBe(0);

  const res = await dispatch("/resolve?wikidata=Q10001");
  expect(res.status).toBe(200);
  expect(await rows()).toBe(1);

  // Not merely "a row" — the correct one. A fresh public-anchor mint is
  // `new_public_anchor`, which reason_bucket() classifies as EXACT.
  expect(
    await scalar(
      `SELECT COUNT(*) AS n FROM resolutions
        WHERE status_tag = 'new'
          AND reason_tag = 'new_public_anchor'
          AND input_desc = ?1
          AND confidence > 0`,
      "wikidata:Q10001",
    ),
  ).toBe(1);
});

it("/stats reflects the logged resolve", async () => {
  await dispatch("/resolve?wikidata=Q10002");

  const s = (await (await dispatch("/stats")).json()) as Stats;
  // The exact assertion that was broken: /stats must not read zero after a resolve.
  expect(s.total).toBe(1);
  expect(s.exact).toBe(1);
  expect(s.miss).toBe(0);
  expect(s.miss_rate).toBe(0);
  expect(s.by_reason).toEqual({ new_public_anchor: 1 });
  expect(s.entities).toBe(1);
});

it("a repeat resolve logs a second row in the exact bucket", async () => {
  await dispatch("/resolve?wikidata=Q10003");
  await dispatch("/resolve?wikidata=Q10003");

  expect(await rows()).toBe(2);
  const s = (await (await dispatch("/stats")).json()) as Stats;
  expect(s.total).toBe(2);
  // The second call is an idempotent hit on an existing strong key.
  expect(s.by_reason.exact_strong_key).toBe(1);
  expect(s.by_reason.new_public_anchor).toBe(1);
});

it("a refused resolve is logged as a MISS", async () => {
  // Phone alone is a corroborator, never an identity — the resolver refuses. This
  // is what makes miss_rate non-zero, i.e. what makes the metric useful at all.
  //
  // The tag is `needs_stronger_identifier` rather than `phone_only`: a record
  // carrying a phone *among other keys* is `phone_only`, but a bare phone
  // identifier with nothing to corroborate is "we need a stronger key". Both sit
  // in the MISS bucket, which is what the metric turns on.
  const res = await dispatch("/resolve?id=phone:%2B15106533394");
  expect(res.status).toBe(200);
  const body = (await res.json()) as {
    status: string;
    confidence_reason: string;
    canonical_id: string | null;
  };
  expect(body.status).toBe("unresolved");
  expect(body.canonical_id).toBeNull();
  expect(body.confidence_reason).toBe("needs_stronger_identifier");

  const s = (await (await dispatch("/stats")).json()) as Stats;
  expect(s.total).toBe(1);
  expect(s.miss).toBe(1);
  expect(s.exact).toBe(0);
  expect(s.miss_rate).toBe(1);
  expect(s.by_reason).toEqual({ needs_stronger_identifier: 1 });
});

it("/entity writes NO resolutions row", async () => {
  const r = (await (await dispatch("/resolve?wikidata=Q10004")).json()) as {
    canonical_id: string;
  };
  const before = await rows();

  expect((await dispatch(`/entity/${r.canonical_id}`)).status).toBe(200);
  expect(await rows()).toBe(before);
});

it("/ingest writes NO resolutions row", async () => {
  const res = await post(
    "/ingest",
    JSON.stringify({ sameAs: [{ wikidata: "Q10005" }] }),
  );
  expect(res.status).toBe(200);
  expect(await rows()).toBe(0);
});

it("404s and 400s write NO resolutions row", async () => {
  await dispatch("/nope");
  await dispatch("/resolve"); // no identifier -> 400
  await dispatch("/resolve?id=noColon"); // malformed -> 400
  expect(await rows()).toBe(0);
});

it("logging is best-effort and never fails the resolve", async () => {
  // The call site is `let _ = g.record_resolution(...)`. Drop the table and the
  // resolution must still succeed — instrumentation must never break the product.
  await env.DB.exec("DROP TABLE resolutions");
  const res = await dispatch("/resolve?wikidata=Q10006");
  expect(res.status).toBe(200);
  const body = (await res.json()) as { canonical_id: string | null };
  expect(body.canonical_id).toBeTruthy();
});
