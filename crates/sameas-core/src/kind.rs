//! The identifier-kind registry — the single source of truth for every kind of
//! external identifier `sameas` understands.
//!
//! Adding a new identifier kind is **one [`KindSpec`] entry in [`KINDS`] plus a
//! normalizer function** (and, optionally, a `url_match` recognizer). Nothing
//! else in the codebase encodes the closed set of kinds: `model`, `anchor`,
//! `resolve`, seed-JSON parsing, and the CLI all read from this registry.
//!
//! Example — the Yelp entry that was added purely through this mechanism:
//!
//! ```text
//! KindSpec {
//!     tag: "yelp",
//!     strong: true,
//!     anchor_rank: Some(4),
//!     normalize: normalize::yelp,
//!     url_match: Some(match_yelp),
//! }
//! ```

use crate::normalize;
use anyhow::Result;

/// Everything the rest of the system needs to know about one kind of external
/// identifier. A single registry entry replaces what used to be ~16 edits
/// spread across a closed `IdKind` enum.
pub struct KindSpec {
    /// Stable snake_case tag: the `kind:value` key prefix and the serialized
    /// JSON tag (e.g. `"yelp"`).
    pub tag: &'static str,
    /// `true` = a **strong** key that drives merges; `false` = a corroborator
    /// only (phone), which is recorded but never single-handedly merges
    /// distinct entities.
    pub strong: bool,
    /// `Some(rank)` if this kind can serve as a **public anchor** (lower =
    /// stronger). `None` = never chosen as a public anchor.
    pub anchor_rank: Option<u8>,
    /// Raw input → canonical value.
    pub normalize: fn(&str) -> Result<String>,
    /// Optional: recognize this kind inside a `sameAs` URL, returning the raw
    /// value to normalize. `None` = does not participate in URL harvesting
    /// (e.g. `domain`, which is the harvesting fallback).
    pub url_match: Option<fn(&str) -> Option<String>>,
}

/// The registry. Adding a key = adding one entry here + a normalizer.
///
/// Anchor ranks (lower = stronger): wikidata(0) < placekey(1) < domain(2) <
/// google_place_id(3) < yelp(4). Non-anchor kinds use `None`.
pub static KINDS: &[KindSpec] = &[
    KindSpec {
        tag: "placekey",
        strong: true,
        anchor_rank: Some(1),
        normalize: normalize::placekey,
        url_match: None, // Placekeys are not URL-shaped; no sameAs recognizer.
    },
    KindSpec {
        tag: "domain",
        strong: true,
        anchor_rank: Some(2),
        normalize: normalize::registrable_domain,
        url_match: None, // domain is the URL-harvesting fallback, not a matcher
    },
    KindSpec {
        tag: "google_place_id",
        strong: true,
        anchor_rank: Some(3),
        normalize: normalize::place_id,
        url_match: None,
    },
    KindSpec {
        tag: "imdb",
        strong: true,
        anchor_rank: None,
        normalize: normalize::imdb,
        url_match: Some(match_imdb),
    },
    KindSpec {
        tag: "phone",
        strong: false,
        anchor_rank: None,
        normalize: normalize::phone_e164,
        url_match: None,
    },
    KindSpec {
        tag: "wikidata",
        strong: true,
        anchor_rank: Some(0),
        normalize: normalize::qid,
        url_match: Some(match_wikidata),
    },
    KindSpec {
        tag: "tmdb",
        strong: true,
        anchor_rank: None,
        normalize: normalize::tmdb,
        url_match: Some(match_tmdb),
    },
    KindSpec {
        tag: "yelp",
        strong: true,
        anchor_rank: Some(4),
        normalize: normalize::yelp,
        url_match: Some(match_yelp),
    },
];

/// Look up the spec for a tag (`None` = unknown kind).
pub fn spec_for_tag(tag: &str) -> Option<&'static KindSpec> {
    KINDS.iter().find(|k| k.tag == tag)
}

// --- URL recognizers ------------------------------------------------------
//
// Each returns the raw value (here, the whole URL) to feed to the kind's
// normalizer when the URL belongs to this kind.

fn match_wikidata(url: &str) -> Option<String> {
    contains_host(url, "wikidata.org")
}

fn match_imdb(url: &str) -> Option<String> {
    contains_host(url, "imdb.com")
}

fn match_tmdb(url: &str) -> Option<String> {
    contains_host(url, "themoviedb.org")
}

fn match_yelp(url: &str) -> Option<String> {
    let lower = url.to_ascii_lowercase();
    if lower.contains("yelp.com/biz/") {
        Some(url.to_string())
    } else {
        None
    }
}

fn contains_host(url: &str, host: &str) -> Option<String> {
    if url.to_ascii_lowercase().contains(host) {
        Some(url.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_lookup() {
        assert!(spec_for_tag("yelp").is_some());
        assert!(spec_for_tag("domain").is_some());
        assert!(spec_for_tag("not_a_kind").is_none());
    }

    #[test]
    fn yelp_is_registered_correctly() {
        let spec = spec_for_tag("yelp").unwrap();
        assert!(spec.strong);
        assert_eq!(spec.anchor_rank, Some(4));
        assert!(spec.url_match.is_some());
    }

    #[test]
    fn placekey_is_registered_in_reserved_slot() {
        let spec = spec_for_tag("placekey").unwrap();
        assert!(spec.strong);
        // Rank 1: stronger than domain (2), weaker than wikidata (0).
        assert_eq!(spec.anchor_rank, Some(1));
        assert!(spec.url_match.is_none());
    }
}
