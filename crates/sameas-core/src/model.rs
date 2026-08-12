//! Identifier + record model.
//!
//! An [`ExternalId`] is a *typed, normalized* external identifier. Every id
//! reduces to a canonical string key of the form `kind:value` (for example
//! `domain:bluebottlecoffee.com`). Those keys are what the crosswalk graph
//! stores and unions over.
//!
//! The set of kinds is not hard-coded here — it lives in the [`crate::kind`]
//! registry. An `ExternalId` is just a `(&'static KindSpec, value)` pair, so
//! adding a kind never touches this file.

use crate::kind::{self, KindSpec};
use anyhow::{anyhow, bail, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::Path;

/// A typed, already-normalized external identifier, backed by a registry spec.
#[derive(Clone)]
pub struct ExternalId {
    spec: &'static KindSpec,
    value: String,
}

impl ExternalId {
    /// Build a normalized id from a kind tag + raw string. Unknown tag = error.
    /// This is the single generic constructor; the named helpers below delegate
    /// to it.
    pub fn new(tag: &str, raw: &str) -> Result<ExternalId> {
        let spec =
            kind::spec_for_tag(tag).ok_or_else(|| anyhow!("unknown identifier kind {tag:?}"))?;
        let value = (spec.normalize)(raw)?;
        Ok(ExternalId { spec, value })
    }

    /// The kind's stable snake_case tag (`"domain"`, `"yelp"`, …).
    pub fn kind_tag(&self) -> &'static str {
        self.spec.tag
    }

    /// The registry spec backing this id.
    pub fn spec(&self) -> &'static KindSpec {
        self.spec
    }

    /// The normalized value (without the kind prefix).
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Canonical `kind:value` key used by the crosswalk graph.
    pub fn key(&self) -> String {
        format!("{}:{}", self.spec.tag, self.value)
    }

    /// Strong keys drive merges; a weak key (phone) only corroborates.
    pub fn is_strong(&self) -> bool {
        self.spec.strong
    }

    /// Reconstruct an [`ExternalId`] from a stored `kind:value` key. The value
    /// is already canonical, so it is *not* re-normalized.
    pub fn from_key(key: &str) -> Option<ExternalId> {
        let (tag, value) = key.split_once(':')?;
        let spec = kind::spec_for_tag(tag)?;
        Some(ExternalId {
            spec,
            value: value.to_string(),
        })
    }

    // --- Thin named constructors (delegate to `new`) ---------------------
    //
    // Kept so existing call sites and tests keep compiling. New kinds do NOT
    // need a named helper — `ExternalId::new(tag, raw)` covers them.

    pub fn domain(raw: &str) -> Result<ExternalId> {
        Self::new("domain", raw)
    }

    pub fn google_place_id(raw: &str) -> Result<ExternalId> {
        Self::new("google_place_id", raw)
    }

    pub fn imdb(raw: &str) -> Result<ExternalId> {
        Self::new("imdb", raw)
    }

    pub fn phone(raw: &str) -> Result<ExternalId> {
        Self::new("phone", raw)
    }

    pub fn wikidata(raw: &str) -> Result<ExternalId> {
        Self::new("wikidata", raw)
    }

    pub fn tmdb(raw: &str) -> Result<ExternalId> {
        Self::new("tmdb", raw)
    }

    pub fn yelp(raw: &str) -> Result<ExternalId> {
        Self::new("yelp", raw)
    }
}

// Equality/hashing/ordering are based purely on `(tag, value)` — the spec is a
// stable singleton, but we compare by tag so semantics match the old enum.
impl PartialEq for ExternalId {
    fn eq(&self, other: &Self) -> bool {
        self.spec.tag == other.spec.tag && self.value == other.value
    }
}
impl Eq for ExternalId {}
impl Hash for ExternalId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.spec.tag.hash(state);
        self.value.hash(state);
    }
}
impl std::fmt::Debug for ExternalId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ExternalId({}:{})", self.spec.tag, self.value)
    }
}

/// A partial schema.org-style record: a type/name plus a bag of typed
/// identifiers (`sameAs`). This is the unit resolvers harvest and the graph
/// commits.
#[derive(Clone, Debug, Default)]
pub struct EntityRecord {
    pub entity_type: Option<String>,
    pub name: Option<String>,
    pub same_as: Vec<ExternalId>,
}

impl EntityRecord {
    pub fn from_json_str(s: &str) -> Result<EntityRecord> {
        let raw: RawRecord = serde_json::from_str(s)?;
        raw.into_record()
    }

    pub fn from_path(path: &Path) -> Result<EntityRecord> {
        let s = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
        Self::from_json_str(&s)
    }

    /// Strong identifiers only (everything except phone).
    pub fn strong_ids(&self) -> impl Iterator<Item = &ExternalId> {
        self.same_as.iter().filter(|id| id.is_strong())
    }

    /// Phone identifiers only.
    pub fn phone_ids(&self) -> impl Iterator<Item = &ExternalId> {
        self.same_as.iter().filter(|id| !id.is_strong())
    }
}

// --- Deserialization of seed records -------------------------------------
//
// Kind-agnostic: each `sameAs` element is a single-entry `{"<tag>": "<value>"}`
// object; the tag is dispatched through the registry. Adding a future kind
// requires ZERO changes here — `{"yelp": "..."}` just works.

#[derive(Deserialize)]
struct RawRecord {
    #[serde(rename = "type")]
    entity_type: Option<String>,
    name: Option<String>,
    #[serde(rename = "sameAs", default)]
    same_as: Vec<HashMap<String, String>>,
}

impl RawRecord {
    fn into_record(self) -> Result<EntityRecord> {
        let mut same_as = Vec::with_capacity(self.same_as.len());
        for entry in self.same_as {
            if entry.len() != 1 {
                bail!(
                    "each sameAs entry must be a single {{\"<kind>\": \"<value>\"}} object, \
                     got {} key(s)",
                    entry.len()
                );
            }
            let (tag, value) = entry.into_iter().next().expect("len == 1");
            same_as.push(ExternalId::new(&tag, &value)?);
        }
        Ok(EntityRecord {
            entity_type: self.entity_type,
            name: self.name,
            same_as,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_roundtrips() {
        let id = ExternalId::domain("bluebottlecoffee.com").unwrap();
        assert_eq!(id.key(), "domain:bluebottlecoffee.com");
        assert_eq!(ExternalId::from_key(&id.key()), Some(id));
    }

    #[test]
    fn new_normalizes_via_registry() {
        let id = ExternalId::new(
            "yelp",
            "https://www.yelp.com/biz/blue-bottle-coffee-san-francisco?x=1",
        )
        .unwrap();
        assert_eq!(id.kind_tag(), "yelp");
        assert_eq!(id.value(), "blue-bottle-coffee-san-francisco");
        assert_eq!(id.key(), "yelp:blue-bottle-coffee-san-francisco");
        assert!(id.is_strong());
    }

    #[test]
    fn new_rejects_unknown_kind() {
        assert!(ExternalId::new("myspace", "whatever").is_err());
    }

    #[test]
    fn parses_seed_record() {
        let json = r#"{
            "type": "LocalBusiness",
            "name": "Blue Bottle Coffee",
            "sameAs": [
                {"domain": "https://www.bluebottlecoffee.com/menu"},
                {"phone": "+1-510-653-3394"},
                {"wikidata": "https://www.wikidata.org/wiki/Q4926426"}
            ]
        }"#;
        let rec = EntityRecord::from_json_str(json).unwrap();
        assert_eq!(rec.entity_type.as_deref(), Some("LocalBusiness"));
        assert_eq!(rec.same_as.len(), 3);
        let keys: Vec<String> = rec.same_as.iter().map(|i| i.key()).collect();
        assert!(keys.contains(&"domain:bluebottlecoffee.com".to_string()));
        assert!(keys.contains(&"phone:+15106533394".to_string()));
        assert!(keys.contains(&"wikidata:Q4926426".to_string()));
        assert_eq!(rec.strong_ids().count(), 2);
        assert_eq!(rec.phone_ids().count(), 1);
    }

    #[test]
    fn parses_seed_record_with_yelp_no_code_changes() {
        // A future/new key flows through deserialization with zero changes.
        let json = r#"{
            "type": "LocalBusiness",
            "sameAs": [
                {"yelp": "https://www.yelp.com/biz/blue-bottle-coffee-san-francisco"}
            ]
        }"#;
        let rec = EntityRecord::from_json_str(json).unwrap();
        assert_eq!(rec.same_as.len(), 1);
        assert_eq!(
            rec.same_as[0].key(),
            "yelp:blue-bottle-coffee-san-francisco"
        );
    }

    #[test]
    fn rejects_unknown_tag_in_seed() {
        let json = r#"{"sameAs": [{"myspace": "x"}]}"#;
        assert!(EntityRecord::from_json_str(json).is_err());
    }
}
