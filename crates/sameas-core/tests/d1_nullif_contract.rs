//! Guards the `bind_opt` ↔ `NULLIF(?N, '')` coupling in `store/d1.rs`.
//!
//! `d1_codec::bind_opt` binds the **empty string** for `None`, because a JS null
//! cannot cross the D1 boundary: under `cdylib` + `worker-build`, both
//! `JsValue::NULL` and `JsValue::null()` arrive as the Worker's `Env` object and
//! D1 rejects them with `D1_TYPE_ERROR: Type 'object' not supported`. Every SQL
//! statement receiving a `bind_opt` must therefore wrap that placeholder in
//! `NULLIF(?N, '')` to turn it back into a real NULL.
//!
//! The compiler cannot see this coupling — `bind_opt` returns an opaque `JsValue`
//! and the SQL is an unparsed `&str` — and violating it is **silent data
//! corruption**, not an error: an empty string is stored where NULL was meant, and
//! reads tolerate it invisibly (`Option<String>` deserializes `""` to `Some("")`,
//! never `None`). The worst case is `attach_with_source`, whose
//! `source = COALESCE(excluded.source, nodes.source)` would take `''` (which is
//! NOT NULL) and silently erase recorded provenance on every re-attach.
//!
//! **Deliberately a source lint, not a runtime check.** `bind_opt(None)` and
//! `bind_str("")` produce byte-identical `JsValue`s, so by the time a parameter
//! reaches `d1_codec::stmt` the intent is gone — and `d1.rs` has four *legitimate*
//! empty-string `bind_str` calls (the bare-name `name_index` row, and the
//! `qualifier_set_key(&[])` of an unqualified name) that such a check would
//! false-positive on. A `debug_assert` would also be compiled out of `--release`,
//! the only profile `custom_build.sh` ever builds.
//!
//! Runs under plain `cargo test --workspace` with no features and no wasm — this
//! file only reads text. `include_str!` makes it a compile-time dependency, so it
//! re-runs whenever `d1.rs` changes.

/// The D1 backend's source, embedded at compile time.
const D1_RAW: &str = include_str!("../src/store/d1.rs");

/// `d1.rs` with every comment line stripped.
///
/// Necessary because the module documents this very contract — it names
/// `bind_opt`, `NULLIF(?N, '')` and `JsValue::NULL` in prose — and counting those
/// mentions would make the lint fight its own documentation. Only real code counts.
fn d1_code() -> String {
    D1_RAW
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            !(t.starts_with("//") || t.starts_with("*") || t.starts_with("/*"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn every_bind_opt_has_a_matching_nullif() {
    let d1 = d1_code();
    let binds = d1.matches("bind_opt(").count();
    let nullifs = d1.matches("NULLIF(?").count();
    assert_eq!(
        binds, nullifs,
        "store/d1.rs has {binds} `bind_opt(` call(s) but {nullifs} `NULLIF(?` in its \
         SQL.\nEvery bind_opt MUST have `NULLIF(?N, '')` at its placeholder position, \
         or an absent value is stored as '' instead of NULL.\nSee the doc comment on \
         `d1_codec::bind_opt`."
    );
}

#[test]
fn each_bind_opt_placeholder_is_wrapped_in_nullif() {
    // `stmt(&self.db, "<sql>", &[<params>])` is a highly regular shape in this
    // file, so split on the call and check each one in isolation. Parameter
    // position N maps to placeholder ?N (1-indexed).
    let d1 = d1_code();
    let mut checked = 0usize;
    for call in d1.split("stmt(").skip(1) {
        let Some(params_at) = call.find("&[") else {
            continue;
        };
        let (sql, params) = call.split_at(params_at);
        // Only look at this call's own parameter list, not the rest of the file.
        let params = match params.find("],") {
            Some(end) => &params[..end],
            None => params,
        };
        for (i, param) in params.split(',').enumerate() {
            if !param.contains("bind_opt(") {
                continue;
            }
            let needle = format!("NULLIF(?{}", i + 1);
            assert!(
                sql.contains(&needle),
                "a `bind_opt` at placeholder ?{} is not wrapped in `{needle}, '')`.\n\
                 Without it, `None` is stored as '' instead of NULL.\nStatement:\n{}",
                i + 1,
                sql.trim()
            );
            checked += 1;
        }
    }
    // Belt and braces: if the splitter silently matched nothing (because the call
    // shape changed), fail loudly rather than pass vacuously.
    assert_eq!(
        checked,
        d1.matches("bind_opt(").count(),
        "the source splitter missed some `bind_opt` call sites — this test needs \
         updating for the new call shape, not deleting"
    );
}

#[test]
fn bind_opt_is_the_only_nullable_binding_helper() {
    let d1 = d1_code();
    // If a new optional-binding helper appears, it needs its own NULLIF audit and
    // this file must learn about it. `JsValue::NULL` / `JsValue::null()` must never
    // come back either — that is the original bug.
    for forbidden in ["JsValue::NULL", "JsValue::null()"] {
        assert!(
            !d1.contains(forbidden),
            "store/d1.rs contains `{forbidden}`. A JS null cannot be bound to D1 \
             here — it arrives as the Worker's `Env` object and D1 rejects it with \
             `D1_TYPE_ERROR: Type 'object' not supported`. Use `bind_opt` + \
             `NULLIF(?N, '')` instead."
        );
    }
}
