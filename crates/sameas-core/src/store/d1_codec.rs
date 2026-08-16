//! D1 binding plumbing: parameter binding and result decoding.
//!
//! D1's WASM binding hands results back as a JS object rather than typed rows, so
//! every read stringifies the `results` array once and deserializes it with serde.
//! These helpers exist so [`super::d1::D1Store`] contains only SQL and no
//! JS-interop noise.
//!
//! Ported from the same helpers in the `agentweb-db` crate (`db/base/codec.rs`,
//! `db/base/binder.rs`, `db/base/chunk.rs`), which have been running this pattern
//! in production Workers — deliberately not reinvented.

use anyhow::{anyhow, Result};
use serde::Deserialize;
use wasm_bindgen::prelude::*;
use worker::{D1Database, D1PreparedStatement};

/// D1 caps prepared-statement parameters at ~100, so a `WHERE col IN (?, ?, …)`
/// over a large slice must be split. 50 leaves headroom for trailing bindings.
pub(crate) const CHUNK_SIZE: usize = 50;

/// Bind a text parameter.
///
/// D1 accepts only scalars (string / number / null / ArrayBuffer) and rejects
/// anything else with `D1_TYPE_ERROR: Type 'object' not supported`. Going through
/// `serde` is a trap here: it happily turns a `&&str` or a `Some("x")` into a JS
/// object, which builds and deploys fine and then fails on every request at
/// runtime. So text is converted directly instead.
pub(crate) fn bind_str(v: &str) -> JsValue {
    JsValue::from_str(v)
}

/// Bind an optional text parameter as **the empty string** when absent.
///
/// The SQL must therefore wrap the placeholder in `NULLIF(?N, '')` to turn it back
/// into a real SQL NULL. That indirection looks gratuitous but is load-bearing:
/// binding a JS `null` here does not work. Both `JsValue::NULL` (a const with a
/// fixed externref-table index) and `JsValue::null()` came back as the Worker's
/// `Env` object once the module was built as a `cdylib` through `worker-build`, and
/// D1 rejected it with `D1_TYPE_ERROR: Type 'object' not supported`. Every value
/// crossing this boundary is a plain string or number, so no externref slot is
/// consulted at all. This mirrors what `agentweb-db` does in production
/// (`NULLIF(?4, '')` in `db/edges.rs`).
///
/// Consequence: a stored empty string and SQL NULL are indistinguishable through
/// this backend. Harmless for the columns involved (`source`, `entity_type`,
/// `name`, `matched_via`, `input_desc`) — an empty provenance or name carries no
/// meaning the code relies on.
pub(crate) fn bind_opt(v: Option<&str>) -> JsValue {
    JsValue::from_str(v.unwrap_or(""))
}

/// Bind a numeric parameter.
pub(crate) fn bind_f64(v: f64) -> JsValue {
    JsValue::from_f64(v)
}

/// Build `?start`, `?start+1`, … placeholders for `count` slots.
pub(crate) fn placeholders(count: usize, start: usize) -> Vec<String> {
    (0..count).map(|i| format!("?{}", start + i)).collect()
}

/// Prepare `sql` and bind `params` in order.
///
/// The error names the failing SQL and the JS type of each binding: a
/// `D1_TYPE_ERROR` otherwise gives no clue which of a dozen statements produced it.
pub(crate) fn stmt(db: &D1Database, sql: &str, params: &[JsValue]) -> Result<D1PreparedStatement> {
    db.prepare(sql).bind(params).map_err(|e| {
        let types: Vec<String> = params
            .iter()
            .map(|p| {
                if p.is_null() {
                    "null".to_string()
                } else if p.as_string().is_some() {
                    "string".to_string()
                } else if p.as_f64().is_some() {
                    "number".to_string()
                } else {
                    format!("OTHER({p:?})")
                }
            })
            .collect();
        anyhow!(
            "d1: binding failed for `{}` with {} param(s) [{}]: {e}",
            sql.split_whitespace().collect::<Vec<_>>().join(" "),
            params.len(),
            types.join(", ")
        )
    })
}

/// Run `all()` and return the `results` array as a JSON string (`"[]"` when the
/// statement produced no result set).
async fn all_json(stmt: &D1PreparedStatement) -> Result<String> {
    let promise = stmt
        .inner()
        .all()
        .map_err(|e| anyhow!("d1: all() failed: {e:?}"))?;
    let result = wasm_bindgen_futures::JsFuture::from(promise)
        .await
        .map_err(|e| anyhow!("d1: all() promise rejected: {e:?}"))?;
    let results_arr =
        js_sys::Reflect::get(&result, &JsValue::from_str("results")).unwrap_or(JsValue::null());
    if results_arr.is_undefined() || results_arr.is_null() {
        return Ok("[]".into());
    }
    let json_js = js_sys::JSON::stringify(&results_arr)
        .map_err(|_| anyhow!("d1: JSON.stringify of results failed"))?;
    Ok(json_js.as_string().unwrap_or_else(|| "[]".into()))
}

/// Deserialize every result row into `Vec<T>`.
pub(crate) async fn rows<T: for<'de> Deserialize<'de>>(stmt: D1PreparedStatement) -> Result<Vec<T>> {
    let json = all_json(&stmt).await?;
    serde_json::from_str(&json).map_err(|e| anyhow!("d1: decoding rows failed: {e}"))
}

/// Deserialize the first result row, or `None` when there are no rows.
///
/// An empty result set is `Ok(None)`, never an error — the callers' `Option`
/// returns mean "absent", and a missing row is not a failure.
pub(crate) async fn first_row<T: for<'de> Deserialize<'de>>(
    stmt: D1PreparedStatement,
) -> Result<Option<T>> {
    Ok(rows::<T>(stmt).await?.into_iter().next())
}

/// Execute a single write statement, discarding the result metadata.
pub(crate) async fn run(stmt: D1PreparedStatement) -> Result<()> {
    stmt.run()
        .await
        .map_err(|e| anyhow!("d1: statement failed: {e}"))?;
    Ok(())
}

/// Execute `statements` as ONE atomic unit.
///
/// Per Cloudflare's D1 docs, a batch is a SQL transaction: statements run
/// sequentially and non-concurrently, and if one fails the **entire sequence is
/// rolled back**. That is what lets `merge_into` / `apply_split` keep the
/// all-or-nothing guarantee they have under `rusqlite`'s explicit transaction.
///
/// It also collapses N round trips into one, which matters much more here than on
/// a local file.
pub(crate) async fn batch(db: &D1Database, statements: Vec<D1PreparedStatement>) -> Result<()> {
    if statements.is_empty() {
        return Ok(());
    }
    db.batch(statements)
        .await
        .map_err(|e| anyhow!("d1: batch failed (rolled back): {e}"))?;
    Ok(())
}

/// Run `build` once per [`CHUNK_SIZE`]-sized slice of `items`, unioning the rows.
///
/// `build` receives the chunk's 1-indexed placeholder strings and returns the SQL
/// plus any **extra** bindings that follow the chunk's own values.
/// `items` are bound as text via [`bind_str`] — deliberately concrete rather than
/// generic-over-`Serialize`, because serde would turn each `&String` into a JS
/// object that D1 rejects at runtime.
pub(crate) async fn chunked_rows<Row, F>(
    db: &D1Database,
    items: &[String],
    mut build: F,
) -> Result<Vec<Row>>
where
    Row: for<'de> Deserialize<'de>,
    F: FnMut(&[String]) -> (String, Vec<JsValue>),
{
    let mut out: Vec<Row> = Vec::new();
    for chunk in items.chunks(CHUNK_SIZE) {
        let ph = placeholders(chunk.len(), 1);
        let (sql, extra) = build(&ph);
        let mut bindings: Vec<JsValue> = chunk.iter().map(|s| bind_str(s)).collect();
        bindings.extend(extra);
        out.extend(rows::<Row>(stmt(db, &sql, &bindings)?).await?);
    }
    Ok(out)
}
