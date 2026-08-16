import { beforeEach, expect, it } from "vitest";
import { post, postAnon, resetDb, scalar } from "./helpers";

// Covers the `Request`/`Env` shell of `require_token`, which cannot be unit-tested
// on the host target (wasm-bindgen stubs every extern with a panic off wasm32, and
// `Env` has no constructor). The pure decision function `auth_outcome` is covered by
// host tests in router.rs; this file proves the wiring around it.
//
// Also pins the JSON error envelope: the auth paths used `Response::error`, which
// emits text/plain, while every other error path emits JSON — so a client could not
// parse errors uniformly.

beforeEach(resetDb);

const BODY = JSON.stringify({ sameAs: [{ wikidata: "Q20001" }] });

it("accepts the configured token", async () => {
  const res = await post("/ingest", BODY);
  expect(res.status).toBe(200);
  expect(await scalar(`SELECT COUNT(*) AS n FROM entities`)).toBe(1);
});

it("rejects a wrong token with a 401 JSON envelope", async () => {
  const res = await post("/ingest", BODY, "wrong-token-of-different-length");
  expect(res.status).toBe(401);
  expect(res.headers.get("content-type")).toContain("application/json");
  const body = (await res.json()) as { error: { code: string } };
  expect(body.error.code).toBe("unauthorized");
  // And it wrote nothing.
  expect(await scalar(`SELECT COUNT(*) AS n FROM entities`)).toBe(0);
});

it("rejects a same-length wrong token", async () => {
  // Guards the byte comparison rather than the length check.
  const res = await post("/ingest", BODY, "test-token-not-a-real-secreT");
  expect(res.status).toBe(401);
});

it("rejects a missing Authorization header", async () => {
  const res = await postAnon("/ingest", BODY);
  expect(res.status).toBe(401);
  const body = (await res.json()) as { error: { code: string } };
  expect(body.error.code).toBe("unauthorized");
});

it("rejects a token-shaped prefix", async () => {
  // Without the length check, `zip` would stop at the shorter input and this
  // prefix would authenticate.
  const res = await post("/ingest", BODY, "test-token");
  expect(res.status).toBe(401);
});

it("rejects an invalid record body with a 400 envelope", async () => {
  const res = await post("/ingest", "{not json");
  expect(res.status).toBe(400);
  const body = (await res.json()) as { error: { code: string } };
  expect(body.error.code).toBe("invalid_input");
});

it("rejects a record with no identifiers", async () => {
  const res = await post("/ingest", JSON.stringify({ sameAs: [] }));
  expect(res.status).toBe(400);
  const body = (await res.json()) as { error: { code: string } };
  expect(body.error.code).toBe("invalid_input");
});
