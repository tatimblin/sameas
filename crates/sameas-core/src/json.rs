//! The JSON wire shape for a [`ResolveOutput`].
//!
//! Lives in the core (rather than in the CLI, where it started) so every front-end
//! — the `sameas` CLI's `--json`, and the HTTP Worker — emits the *same* document
//! for the same resolution. Two hand-rolled copies would drift.
//!
//! Deliberately hand-built rather than `#[derive(Serialize)]`: the wire shape is a
//! public contract that differs from the in-memory struct on purpose (`sameAs` is a
//! flat `kind:value` array, not typed `ExternalId`s; `confidence` is rounded;
//! `provenance` is an object, not a pair list; `status`/`confidence_reason` are
//! stable string tags). Deriving would leak the internal layout and make any
//! refactor a breaking API change.

use serde_json::{json, Map, Value};

use crate::confidence::reason_tag;
use crate::resolve::ResolveOutput;

/// Round a confidence to 2 decimals for stable, human-friendly output. The core
/// struct keeps full `f32` precision; only the wire form is rounded.
pub fn round2(x: f32) -> f64 {
    (x as f64 * 100.0).round() / 100.0
}

/// The canonical JSON document for a resolution.
///
/// `action` labels which operation produced it (`"resolve"`, `"entity"`,
/// `"ingest"`), so a client can tell a lookup from a write.
pub fn resolve_output_json(out: &ResolveOutput, action: &str) -> Value {
    let same_as: Vec<String> = out.same_as.iter().map(|i| i.key()).collect();
    let provenance: Map<String, Value> = out
        .provenance
        .iter()
        .map(|(key, source)| {
            (
                key.clone(),
                Value::String(source.clone().unwrap_or_else(|| "unknown".into())),
            )
        })
        .collect();
    let candidates: Vec<Value> = out
        .candidates
        .iter()
        .map(|c| {
            json!({
                "canonical_id": c.canonical_id,
                "anchor": c.anchor,
                "name": c.name,
            })
        })
        .collect();
    json!({
        "action": action,
        "canonical_id": out.canonical_id,
        "anchor": out.anchor,
        "type": out.entity_type,
        "name": out.name,
        "status": out.status.as_str(),
        "confidence": round2(out.confidence),
        "confidence_reason": reason_tag(&out.confidence_reason),
        "matched_via": out.matched_via,
        "hint": out.hint,
        "sameAs": same_as,
        "provenance": provenance,
        "candidates": candidates,
        "completion_count": same_as.len(),
        "harvested": out.harvested,
        "new_edges": out.new_edges,
    })
}
