-- Per-caller daily budget for OUTBOUND hub calls (`POST /resolve/name`).
--
-- Additive: a new table only. Nothing here is read by `sameas-core` — the graph
-- schema in 0001 is untouched — because a spend budget is a *deployment* concern
-- of the Worker front-end, not a property of the crosswalk graph.
--
-- `bucket` is an OPAQUE caller-supplied string (the consumer passes its
-- publisher's DID, but sameas neither parses nor validates it: PROJECT_GOALS
-- non-goal #3 — no AT-URIs, DIDs or lexicons in this system). It is a quota key
-- and nothing else, so it is never joined to `entities` and never returned.
--
-- `day` is the integer number of whole UTC days since the Unix epoch, stored as
-- TEXT (`floor(now_ms / 86_400_000)`). A day *number* rather than a formatted
-- date because it needs no calendar library in wasm and no locale; TEXT because
-- D1 bindings for large integers are lossy and the value is only ever compared
-- for equality. Rows are therefore self-expiring by key: a new day starts a new
-- row rather than resetting an old one. Old rows are dead weight, not state — a
-- future cleanup can delete `day < N` with no coordination.
CREATE TABLE IF NOT EXISTS hub_budget (
    bucket TEXT    NOT NULL,
    day    TEXT    NOT NULL,
    calls  INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (bucket, day)
);
