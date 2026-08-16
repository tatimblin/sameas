import { expect, it } from "vitest";
import { dispatch } from "./helpers";

// BUG 1 REGRESSION.
//
// Passes only if the built shim bound its wasm exports. `GET /` returns before
// touching any binding (router.rs's liveness check), so a failure here is the shim
// and nothing else — with wasm-bindgen >=0.2.101, worker-build 0.1.1 emits a shim
// whose exports are never bound and this throws
// "Cannot read properties of undefined (reading 'fetch')" — the exact production
// symptom, on a request that reads no data at all.
//
// The structural counterpart is `checkShim()` in scripts/build.mjs, which gives a
// diagnostic naming the pin. This test proves the artifact actually runs; that one
// explains why it doesn't.

it("answers liveness without touching any binding", async () => {
  const res = await dispatch("/");
  expect(res.status).toBe(200);
  expect(await res.text()).toBe("sameas worker ready");
});

it("answers /health identically", async () => {
  const res = await dispatch("/health");
  expect(res.status).toBe(200);
  expect(await res.text()).toBe("sameas worker ready");
});

it("normalizes a trailing slash", async () => {
  // router.rs trims trailing slashes before matching.
  expect((await dispatch("/health/")).status).toBe(200);
});

it("returns a JSON error envelope for an unknown route", async () => {
  const res = await dispatch("/definitely-not-a-route");
  expect(res.status).toBe(404);
  expect(res.headers.get("content-type")).toContain("application/json");
  const body = (await res.json()) as { error: { code: string; message: string } };
  expect(body.error.code).toBe("not_found");
});
