import { beforeEach, expect, it } from "vitest";
import { post, resetDb, scalar } from "./helpers";

// Runs `sameas_core::store::conformance::run_all` against `D1Store` — the first
// time the backend-agnostic contract suite has been exercised on the D1 backend at
// all. It covers, in one call: union-find transitivity, phone-never-merges, a merge
// re-pointing every referencing table, the derived membership views agreeing with
// the `member_rows` primitive, `apply_split` re-anchoring both sides (where the D1
// backend deliberately diverges — it precomputes anchors instead of reading its own
// uncommitted writes), name-cardinality liveness, `find_many` agreeing with `find`,
// and a source-less re-attach preserving provenance.
//
// The suite requires an EMPTY store: its cases delete and split their own fixtures,
// so `beforeEach(resetDb)` is load-bearing, not hygiene.

beforeEach(resetDb);

it("D1Store satisfies the GraphStore contract", async () => {
  const res = await post("/__conformance", "");
  // Read the body first: on failure it carries the assertion message from
  // run_all, which is far more useful than a bare status mismatch.
  const text = await res.text();
  expect(text, `conformance failed: ${text}`).toBe(`{"conformance":"ok"}`);
  expect(res.status).toBe(200);

  // Prove the suite actually ran against THIS database rather than silently
  // no-op'ing: its cases mint entities.
  expect(await scalar(`SELECT COUNT(*) AS n FROM entities`)).toBeGreaterThan(0);
});

it("rejects an unauthenticated conformance run", async () => {
  const res = await post("/__conformance", "", "wrong-token");
  expect(res.status).toBe(401);
});
