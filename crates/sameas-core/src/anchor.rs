//! Canonical-anchor selection and canonical-id minting.
//!
//! The canonical *anchor* is the strongest public identifier an entity carries,
//! so the identity stays portable and meaningful:
//!
//! ```text
//! Wikidata QID > Placekey > registrable domain > Google place_id > synthetic local:<uuid>
//! ```
//!
//! Placekey is not a modeled id kind in M1, so its slot is reserved. A
//! synthetic `local:<uuid>` anchor is minted only when no public anchor exists.
//!
//! Anchor eligibility and priority are driven entirely by the [`crate::kind`]
//! registry: a kind is a public-anchor candidate iff its `anchor_rank` is
//! `Some(_)` (lower = stronger). Adding an anchor-eligible kind is just a
//! registry entry with an `anchor_rank`.

use crate::kind::spec_for_tag;
use crate::model::ExternalId;

/// Pick the strongest public anchor key among `ids`, if any. Deterministic:
/// ties within a rank break on the smaller value string. A kind is eligible iff
/// its registry `anchor_rank` is `Some(_)`.
pub fn public_anchor(ids: &[ExternalId]) -> Option<String> {
    ids.iter()
        .filter_map(|id| id.spec().anchor_rank.map(|rank| (rank, id)))
        .min_by(|(ra, a), (rb, b)| ra.cmp(rb).then_with(|| a.value().cmp(b.value())))
        .map(|(_, id)| id.key())
}

/// Mint a fresh synthetic anchor. Used only when no public anchor is present.
pub fn mint_local() -> String {
    format!("local:{}", uuid::Uuid::new_v4())
}

/// Choose an anchor for a brand-new entity: strongest public anchor, else a
/// freshly minted synthetic local id.
pub fn choose_anchor(ids: &[ExternalId]) -> String {
    public_anchor(ids).unwrap_or_else(mint_local)
}

/// Recompute an entity's anchor from its current members, preserving the
/// existing (synthetic) anchor when no public anchor is present. The canonical
/// id never changes as a result — only the anchor label may sharpen.
pub fn recompute_anchor(members: &[ExternalId], current: &str) -> String {
    public_anchor(members).unwrap_or_else(|| current.to_string())
}

/// Priority rank of an existing anchor *key* (`kind:value`), used to pick the
/// winner when unioning two entities. Lower is stronger. Driven by the registry
/// `anchor_rank`; synthetic `local:` anchors sort below every public anchor,
/// and anything unrecognized sorts last.
pub fn anchor_key_rank(anchor: &str) -> u8 {
    match anchor.split_once(':') {
        Some(("local", _)) => 100,
        Some((tag, _)) => spec_for_tag(tag)
            .and_then(|spec| spec.anchor_rank)
            .unwrap_or(200),
        None => 200,
    }
}

/// FNV-1a 32-bit hash — small, dependency-free, stable across runs.
fn fnv1a(s: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in s.bytes() {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Derive a stable, human-ish canonical id from an anchor. Deterministic for
/// public anchors (so movies/places reproduce across runs) and stable-within-a-
/// run for synthetic anchors (the anchor is stored once).
pub fn canonical_id_for(anchor: &str) -> String {
    format!("cx_{:08x}", fnv1a(anchor))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_ordering() {
        let ids = vec![
            ExternalId::domain("bluebottlecoffee.com").unwrap(),
            ExternalId::google_place_id("ChIJabc").unwrap(),
            ExternalId::wikidata("Q4926426").unwrap(),
            ExternalId::phone("+15106533394").unwrap(),
        ];
        assert_eq!(public_anchor(&ids).as_deref(), Some("wikidata:Q4926426"));

        let ids = vec![
            ExternalId::domain("bluebottlecoffee.com").unwrap(),
            ExternalId::google_place_id("ChIJabc").unwrap(),
        ];
        assert_eq!(public_anchor(&ids).as_deref(), Some("domain:bluebottlecoffee.com"));

        let ids = vec![ExternalId::google_place_id("ChIJabc").unwrap()];
        assert_eq!(public_anchor(&ids).as_deref(), Some("google_place_id:ChIJabc"));
    }

    #[test]
    fn yelp_ranks_just_below_google_place_id() {
        // Yelp is a public anchor (rank 4), weaker than google_place_id (3).
        let ids = vec![
            ExternalId::yelp("blue-bottle-coffee-san-francisco").unwrap(),
            ExternalId::google_place_id("ChIJabc").unwrap(),
        ];
        assert_eq!(
            public_anchor(&ids).as_deref(),
            Some("google_place_id:ChIJabc")
        );

        // But yelp still beats non-anchor kinds and is chosen when alone.
        let ids = vec![ExternalId::yelp("blue-bottle-coffee-san-francisco").unwrap()];
        assert_eq!(
            public_anchor(&ids).as_deref(),
            Some("yelp:blue-bottle-coffee-san-francisco")
        );

        // anchor_key_rank agrees: wikidata < domain < google_place_id < yelp.
        assert!(anchor_key_rank("wikidata:Q1") < anchor_key_rank("domain:x.com"));
        assert!(anchor_key_rank("domain:x.com") < anchor_key_rank("google_place_id:ChIJ"));
        assert!(anchor_key_rank("google_place_id:ChIJ") < anchor_key_rank("yelp:slug"));
        assert!(anchor_key_rank("yelp:slug") < anchor_key_rank("local:uuid"));
    }

    #[test]
    fn phone_only_has_no_public_anchor() {
        let ids = vec![ExternalId::phone("+15106533394").unwrap()];
        assert_eq!(public_anchor(&ids), None);
        assert!(choose_anchor(&ids).starts_with("local:"));
    }

    #[test]
    fn canonical_id_is_stable_for_anchor() {
        assert_eq!(
            canonical_id_for("wikidata:Q4926426"),
            canonical_id_for("wikidata:Q4926426")
        );
        assert_ne!(
            canonical_id_for("wikidata:Q4926426"),
            canonical_id_for("wikidata:Q83495")
        );
    }
}
