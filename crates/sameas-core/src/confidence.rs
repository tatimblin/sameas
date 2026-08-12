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
/// Placekey derived from a full street address.
pub const PLACEKEY_ADDRESS: f32 = 0.70;
/// New entity minted with a public anchor, no corroboration.
pub const NEW_PUBLIC_ANCHOR: f32 = 0.60;
/// Placekey / text-search derived from name + city only (coarse).
pub const PLACEKEY_CITY: f32 = 0.40;
/// Phone-only corroboration — a hypothesis, never a merge.
pub const PHONE_ONLY: f32 = 0.30;
/// New entity with a synthetic anchor only (no public identifier).
pub const SYNTHETIC_ONLY: f32 = 0.20;
/// Direct lookup by canonical id (`entity <id>`) — we were handed the identity.
pub const DIRECT_LOOKUP: f32 = 1.0;
