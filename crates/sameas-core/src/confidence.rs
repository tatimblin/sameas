//! Confidence scoring.
//!
//! Confidence is a `0.0–1.0` score describing how well the **input attaches to
//! the returned entity** — the weakest link, not the richness of the completed
//! cluster. A coarse "name + city" match is low-confidence even when it lands on
//! a fully cross-linked entity, because the *identity* attachment is only as
//! good as the text/address match that produced it.
//!
//! The scores are a documented gradient, not a probabilistic model (hence a
//! plain scale rather than calibrated probabilities). Reverse-resolver /
//! name+address paths override the commit-time score with the coarser values.

/// Resolved/attached by an exact strong key (in-graph hit or freshly minted).
pub const EXACT_STRONG: f32 = 0.95;
/// Completed through a strong-key hub crosswalk (e.g. imdb → wikidata → tmdb).
pub const HUB_CROSSWALK: f32 = 0.90;
/// Placekey derived from a full street address — precise, high confidence.
pub const PLACEKEY_ADDRESS: f32 = 0.85;
/// A text/name query the hub resolved to a SINGLE place — a confident (if
/// delegated) match. Not as certain as a user-supplied exact id.
pub const PLACE_UNIQUE: f32 = 0.80;
/// New entity minted with a public anchor, no corroboration.
pub const NEW_PUBLIC_ANCHOR: f32 = 0.60;
/// A repeat name+qualifier query served from the local name index (cached prior
/// resolution). Conservative/medium — still a name match; the broader name-match
/// confidence tier is an open decision.
pub const LOCAL_NAME: f32 = 0.60;
/// Text/name query resolved but only coarsely (kept for a future match-score
/// signal; the count-based path uses `PLACE_UNIQUE`/`AMBIGUOUS`).
pub const PLACEKEY_CITY: f32 = 0.40;
/// Phone-only corroboration — a hypothesis, never a merge.
pub const PHONE_ONLY: f32 = 0.30;
/// New entity with a synthetic anchor only (no public identifier).
pub const SYNTHETIC_ONLY: f32 = 0.20;
/// Direct lookup by canonical id (`entity <id>`) — we were handed the identity.
pub const DIRECT_LOOKUP: f32 = 1.0;
/// Strong key present but no public anchor — reproducible synthetic anchor.
pub const SYNTHETIC_STRONG: f32 = 0.55;
/// Weak/reverse signal matched several distinct entities — ambiguous.
pub const AMBIGUOUS: f32 = 0.25;
/// Nothing resolvable supplied — caller must provide a stronger identifier.
pub const NEEDS_MORE: f32 = 0.15;

/// Why a confidence score came out the way it did. Pairing the number with a
/// reason is what makes a low score actionable — it says *what* to fix, not just
/// that the attachment was weak.
#[derive(Clone, Debug, PartialEq)]
pub enum ConfidenceReason {
    DirectLookup,
    ExactStrongKey,
    HubCrosswalk,
    PlacekeyAddress,
    PlaceUniqueMatch,
    LocalNameMatch,
    NewPublicAnchor,
    PlacekeyCityOnly,
    PhoneOnly,
    SyntheticStrongKey,
    AmbiguousAmongN(usize),
    NeedsStrongerIdentifier,
}

/// Map a reason to its confidence score. The constants remain the single source
/// of the numbers; the enum is what makes a low score actionable.
pub fn score(reason: &ConfidenceReason) -> f32 {
    match reason {
        ConfidenceReason::DirectLookup => DIRECT_LOOKUP,
        ConfidenceReason::ExactStrongKey => EXACT_STRONG,
        ConfidenceReason::HubCrosswalk => HUB_CROSSWALK,
        ConfidenceReason::PlacekeyAddress => PLACEKEY_ADDRESS,
        ConfidenceReason::PlaceUniqueMatch => PLACE_UNIQUE,
        ConfidenceReason::LocalNameMatch => LOCAL_NAME,
        ConfidenceReason::NewPublicAnchor => NEW_PUBLIC_ANCHOR,
        ConfidenceReason::PlacekeyCityOnly => PLACEKEY_CITY,
        ConfidenceReason::PhoneOnly => PHONE_ONLY,
        ConfidenceReason::SyntheticStrongKey => SYNTHETIC_STRONG,
        ConfidenceReason::AmbiguousAmongN(_) => AMBIGUOUS,
        ConfidenceReason::NeedsStrongerIdentifier => NEEDS_MORE,
    }
}

/// A stable snake_case tag for output (JSON/table).
pub fn reason_tag(reason: &ConfidenceReason) -> &'static str {
    match reason {
        ConfidenceReason::DirectLookup => "direct_lookup",
        ConfidenceReason::ExactStrongKey => "exact_strong_key",
        ConfidenceReason::HubCrosswalk => "hub_crosswalk",
        ConfidenceReason::PlacekeyAddress => "placekey_address",
        ConfidenceReason::PlaceUniqueMatch => "place_unique_match",
        ConfidenceReason::LocalNameMatch => "local_name_match",
        ConfidenceReason::NewPublicAnchor => "new_public_anchor",
        ConfidenceReason::PlacekeyCityOnly => "placekey_city_only",
        ConfidenceReason::PhoneOnly => "phone_only",
        ConfidenceReason::SyntheticStrongKey => "synthetic_strong_key",
        ConfidenceReason::AmbiguousAmongN(_) => "ambiguous_among_n",
        ConfidenceReason::NeedsStrongerIdentifier => "needs_stronger_identifier",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_maps_to_constant() {
        assert_eq!(score(&ConfidenceReason::ExactStrongKey), EXACT_STRONG);
    }

    #[test]
    fn ambiguous_ignores_count() {
        assert_eq!(score(&ConfidenceReason::AmbiguousAmongN(3)), AMBIGUOUS);
    }

    #[test]
    fn reason_tag_is_stable_snake_case() {
        assert_eq!(
            reason_tag(&ConfidenceReason::NeedsStrongerIdentifier),
            "needs_stronger_identifier"
        );
    }
}
