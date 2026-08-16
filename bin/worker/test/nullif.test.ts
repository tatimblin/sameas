import { beforeEach, expect, it } from "vitest";
import { dispatch, resetDb, scalar } from "./helpers";

// BUG 2 REGRESSION.
//
// `d1_codec::bind_opt` binds the EMPTY STRING for `None`, because a JS null cannot
// cross the D1 boundary — under cdylib + worker-build both `JsValue::NULL` and
// `JsValue::null()` arrive as the Worker's `Env` object and D1 rejects them with
// `D1_TYPE_ERROR: Type 'object' not supported`. The SQL must therefore wrap each
// such placeholder in `NULLIF(?N, '')`.
//
// Drop a NULLIF and these tests fail. They are the only runtime signal: reads
// tolerate the corruption invisibly, because `Option<String>` deserializes `""` to
// `Some("")` and never to `None`.
//
// Assertions go through `env.DB` directly rather than through an endpoint, so a
// bug in the read path cannot make a corrupt write look clean.

beforeEach(resetDb);

it("stores an absent entity_type/name as SQL NULL, not ''", async () => {
  // `GET /resolve` builds `EntityRecord { same_as: [id], ..Default::default() }`,
  // so both fields are None all the way down to create_entity(cid, anchor, None, None).
  const res = await dispatch("/resolve?wikidata=Q11111");
  expect(res.status).toBe(200);

  expect(
    await scalar(
      `SELECT COUNT(*) AS n FROM entities
        WHERE entity_type IS NULL AND name IS NULL`,
    ),
  ).toBe(1);

  // The assertion that actually fails when the NULLIF is dropped.
  expect(
    await scalar(
      `SELECT COUNT(*) AS n FROM entities WHERE entity_type = '' OR name = ''`,
    ),
  ).toBe(0);
});

it("stores an absent matched_via as SQL NULL, not ''", async () => {
  // A fresh resolve has no matched_via, so `.first()` is None -> bind_opt(None).
  await dispatch("/resolve?wikidata=Q22222");

  expect(
    await scalar(`SELECT COUNT(*) AS n FROM resolutions WHERE matched_via IS NULL`),
  ).toBe(1);
  expect(
    await scalar(`SELECT COUNT(*) AS n FROM resolutions WHERE matched_via = ''`),
  ).toBe(0);

  // input_desc IS Some(..) on this path, so it must NOT be null. Without this the
  // test would pass by asserting "everything is null", which proves nothing.
  expect(
    await scalar(
      `SELECT COUNT(*) AS n FROM resolutions WHERE input_desc = ?1`,
      "wikidata:Q22222",
    ),
  ).toBe(1);
});

it("never stores an empty string in any nullable text column", async () => {
  // A broad sweep over every nullable column the write paths touch, so a NEW
  // bind_opt added without its NULLIF is caught even if no test targets it.
  await dispatch("/resolve?wikidata=Q33333");
  await dispatch("/resolve?place_id=NULLIF_TEST_PLACE");

  expect(
    await scalar(`SELECT COUNT(*) AS n FROM nodes WHERE source = ''`),
  ).toBe(0);
  expect(
    await scalar(`SELECT COUNT(*) AS n FROM phone_edges WHERE source = ''`),
  ).toBe(0);
  expect(
    await scalar(`SELECT COUNT(*) AS n FROM name_index WHERE source = ''`),
  ).toBe(0);
  expect(
    await scalar(
      `SELECT COUNT(*) AS n FROM resolutions
        WHERE matched_via = '' OR input_desc = ''`,
    ),
  ).toBe(0);
  expect(
    await scalar(
      `SELECT COUNT(*) AS n FROM entities WHERE entity_type = '' OR name = ''`,
    ),
  ).toBe(0);
});

it("preserves a supplied source rather than nulling it", async () => {
  // The other direction: bind_opt(Some(..)) must survive the NULLIF unchanged.
  // `commit_record` attaches with source "input".
  await dispatch("/resolve?wikidata=Q44444");
  expect(
    await scalar(`SELECT COUNT(*) AS n FROM nodes WHERE source = 'input'`),
  ).toBe(1);
});
