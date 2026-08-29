//! Per-caller daily budget for outbound hub calls.
//!
//! `POST /resolve/name` is the only route that can spend money: a Google Places
//! text search plus up to [`PLACE_DETAILS_FANOUT_CAP`] Place Details calls, all on
//! Google's Enterprise SKU. The route is token-gated, but one shared binding token
//! is *one* bucket — a single misbehaving consumer would spend the whole budget.
//! So the caller names its own bucket per request and each bucket gets its own
//! daily allowance.
//!
//! [`PLACE_DETAILS_FANOUT_CAP`]: sameas_core::complete::PLACE_DETAILS_FANOUT_CAP
//!
//! **The bucket is opaque.** The consumer passes a publisher DID; this crate never
//! parses, validates or interprets it, and stores it in `hub_budget` and nowhere
//! else. sameas must not know what a DID is (PROJECT_GOALS non-goal #3).
//!
//! **Reserved before the call, not after, and never refunded.** A hub call that
//! fails still cost a subrequest and, for Places, may still have been billed — so
//! the counter is incremented up front. Over-counting a failure is the safe
//! direction; under-counting is how a retry loop spends a month's budget.
//!
//! The no-refund half of that used to be forced: a hub failure was swallowed by
//! `.unwrap_or_default()` in `sameas-core`, so the handler could not have refunded
//! selectively even if it wanted to — it never learned that anything had gone
//! wrong. `ResolveOutput::hub_error` removes that constraint, and the policy is
//! kept anyway, on purpose. The full argument lives at the reservation site in
//! `handlers::resolve_name` (step 2c); the short version is that this counter
//! meters attempts to spend rather than answers received, and refunding would make
//! a *broken* hub the cheapest one to hammer.
//!
//! **This is the kill switch.** `HUB_DAILY_BUDGET = "0"` denies every bucket, which
//! stops all outbound hub spend without deleting secrets or redeploying.

use serde::Deserialize;
use worker::D1Database;

/// The one column [`reserve`]'s statement returns.
#[derive(Deserialize)]
struct CallsRow {
    calls: f64,
}

/// Daily hub calls per bucket when `HUB_DAILY_BUDGET` is unset or unparseable.
pub const DEFAULT_DAILY_HUB_BUDGET: u32 = 200;

/// Milliseconds in a UTC day.
const DAY_MS: f64 = 86_400_000.0;

/// The `day` component of the budget key: whole UTC days since the Unix epoch.
///
/// A day *number* rather than a formatted date — no calendar library in wasm, no
/// locale, no timezone. Rolls over at 00:00 UTC, which is a deliberate choice over
/// a rolling 24h window: a fixed boundary needs one row per bucket per day and no
/// per-call timestamps (the graph stores no wall clock anywhere else either).
pub fn utc_day(now_ms: f64) -> i64 {
    if !now_ms.is_finite() || now_ms <= 0.0 {
        return 0;
    }
    (now_ms / DAY_MS).floor() as i64
}

/// Parse the `HUB_DAILY_BUDGET` var. Anything unparseable falls back to the
/// default rather than erroring: a typo in a var must not take the route down,
/// and the default is finite, so it cannot mean "unlimited" by accident.
pub fn parse_limit(raw: Option<&str>) -> u32 {
    raw.and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(DEFAULT_DAILY_HUB_BUDGET)
}

/// The outcome of trying to reserve one hub call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reservation {
    /// Reserved. `used` is this bucket's call count for today, including this one.
    Granted { used: u32 },
    /// The bucket is out of budget for today; nothing was written.
    Exhausted,
}

/// Reserve one hub call for `bucket` on `day`, or report exhaustion.
///
/// One statement, so check-and-increment cannot interleave: the `WHERE` rides on
/// the `DO UPDATE`, so a bucket at the limit updates no row and `RETURNING` yields
/// nothing. A read-then-write pair would let two concurrent requests both observe
/// `limit - 1` and both spend.
///
/// Errors are the caller's cue to **fail closed** — an unreadable budget table
/// means spend cannot be accounted for, and unaccounted spend on an Enterprise SKU
/// is exactly what this exists to prevent. (Contrast `record_resolution`, which is
/// best-effort: losing a metric is not losing money.)
pub async fn reserve(
    db: &D1Database,
    bucket: &str,
    day: i64,
    limit: u32,
) -> anyhow::Result<Reservation> {
    // Guard the insert branch: with `limit == 0` the ON CONFLICT clause never
    // runs on the first call of the day, so the row would be created at 1 and the
    // kill switch would leak exactly one call per bucket per day.
    if limit == 0 {
        return Ok(Reservation::Exhausted);
    }
    let stmt = db
        .prepare(
            "INSERT INTO hub_budget (bucket, day, calls) VALUES (?1, ?2, 1)
             ON CONFLICT(bucket, day) DO UPDATE SET calls = hub_budget.calls + 1
               WHERE hub_budget.calls < ?3
             RETURNING calls",
        )
        .bind(&[
            wasm_bindgen::JsValue::from(bucket),
            wasm_bindgen::JsValue::from(day.to_string()),
            wasm_bindgen::JsValue::from(limit as f64),
        ])
        .map_err(|e| anyhow::anyhow!("binding the hub budget statement: {e}"))?;

    // Read the whole result set rather than `first(Some("calls"))`: when the
    // `WHERE` refuses the update there is NO row, and `first` hands that back as a
    // JS `null` which serde then tries to read as the column's type — turning
    // "budget exhausted" into a deserialization error, i.e. a 503 where a 429
    // belongs. An empty `results` array is unambiguous.
    //
    // `f64`, not an integer type: D1 hands INTEGER back as a JS number, and asking
    // serde for an i64 is a round-trip through a representation JS does not have.
    let out = stmt
        .all()
        .await
        .map_err(|e| anyhow::anyhow!("reserving a hub call for the budget: {e}"))?;
    let rows: Vec<CallsRow> = out
        .results()
        .map_err(|e| anyhow::anyhow!("reading back the reserved hub call: {e}"))?;

    Ok(match rows.first() {
        Some(row) => Reservation::Granted {
            used: row.calls.max(0.0) as u32,
        },
        None => Reservation::Exhausted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_day_is_a_stable_day_number() {
        // 1970-01-01T00:00:00Z and the last millisecond of that day are day 0.
        assert_eq!(utc_day(1.0), 0);
        assert_eq!(utc_day(DAY_MS - 1.0), 0);
        assert_eq!(utc_day(DAY_MS), 1);
        // 2026-08-28T00:00:00Z
        assert_eq!(utc_day(1_787_961_600_000.0), 20_694);
    }

    #[test]
    fn utc_day_never_panics_on_a_junk_clock() {
        // `Date::now()` cannot realistically produce these, but a total function
        // here means a clock oddity degrades to "one shared bucket-day", never a
        // wasm trap inside a request.
        assert_eq!(utc_day(f64::NAN), 0);
        assert_eq!(utc_day(-1.0), 0);
        assert_eq!(utc_day(f64::INFINITY), 0);
    }

    #[test]
    fn limit_parsing_falls_back_rather_than_failing() {
        assert_eq!(parse_limit(Some("5")), 5);
        assert_eq!(parse_limit(Some("  5 ")), 5);
        assert_eq!(parse_limit(Some("0")), 0, "0 is the kill switch, not a typo");
        assert_eq!(parse_limit(None), DEFAULT_DAILY_HUB_BUDGET);
        assert_eq!(parse_limit(Some("")), DEFAULT_DAILY_HUB_BUDGET);
        assert_eq!(parse_limit(Some("lots")), DEFAULT_DAILY_HUB_BUDGET);
        assert_eq!(parse_limit(Some("-1")), DEFAULT_DAILY_HUB_BUDGET);
    }
}
