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
//!     grain: Grain::Identity,
//!     normalize: normalize::yelp,
//!     url_match: Some(match_yelp),
//!     to_url: Some(url_yelp),
//! }
//! ```

use crate::normalize;
use anyhow::Result;

/// What an identifier says about the *thing* it labels — its role in
/// disambiguation (M3, type-agnostic entity disambiguation).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grain {
    /// Names one specific real-world thing (drives identity).
    Identity,
    /// May be shared across many things (a chain/studio/brand domain).
    Affiliation,
    /// Corroborator only; lives outside the union-find (phone).
    Weak,
}

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
    /// What this kind says about the thing it labels: identity, affiliation, or
    /// weak corroborator (drives type-agnostic disambiguation).
    pub grain: Grain,
    /// Raw input → canonical value.
    pub normalize: fn(&str) -> Result<String>,
    /// Optional: recognize this kind inside a `sameAs` URL, returning the raw
    /// value to normalize. `None` = does not participate in URL harvesting
    /// (e.g. `domain`, which is the harvesting fallback).
    pub url_match: Option<fn(&str) -> Option<String>>,
    /// Optional **inverse** of `url_match`: project an already-**normalized
    /// value** (not a raw input) back to this kind's canonical public URL.
    ///
    /// Why this exists: a consumer that stores our identifiers inside a
    /// schema.org record needs a *URL*, because `sameAs` is a URL by definition
    /// and downstream clustering keys on one. A flat `kind:value` string has no
    /// scheme, so writing it into a record yields an identifier that can never
    /// match anything — the records share a key and still never collapse. The
    /// `kind:value` form stays the wire/echo/provenance key; the URL form is
    /// what gets written into records.
    ///
    /// `None` (the field) or a `None` result = **no canonical URL exists for
    /// this value**, and it is simply omitted. Two rules make that the right
    /// default rather than a gap:
    ///
    /// * Never invent a URL. If the value cannot be spliced back verbatim
    ///   (percent-encoding would rewrite it, or the projection would be claimed
    ///   by a *different* kind on the way back in), return `None`. An omitted
    ///   identifier costs one anchor; a wrong one mints a duplicate entity.
    /// * **Round-trip or nothing.** For every kind that projects, feeding the
    ///   projected URL back through the registry's matchers must reproduce the
    ///   exact same `kind:value` — otherwise a user picking a candidate would
    ///   silently bind to a *different* key than the one they chose. This is
    ///   asserted for every projecting kind in `url_projections_round_trip`.
    pub to_url: Option<fn(&str) -> Option<String>>,
}

/// The registry. Adding a key = adding one entry here + a normalizer.
///
/// Anchor ranks (lower = stronger): wikidata(0) < placekey(1) < domain(2) <
/// google_place_id(3) < yelp(4) < url(5). Non-anchor kinds use `None`.
pub static KINDS: &[KindSpec] = &[
    KindSpec {
        tag: "placekey",
        strong: true,
        anchor_rank: Some(1),
        grain: Grain::Identity,
        normalize: normalize::placekey,
        url_match: None, // Placekeys are not URL-shaped; no sameAs recognizer.
        // A Placekey is an opaque grid token, not a resource anyone publishes a
        // page for. There is no URL to project to.
        to_url: None,
    },
    KindSpec {
        tag: "domain",
        strong: true,
        anchor_rank: Some(2),
        grain: Grain::Affiliation,
        normalize: normalize::registrable_domain,
        url_match: None, // domain is the URL-harvesting fallback, not a matcher
        // DELIBERATELY None — do not "fix" this by emitting `https://<domain>`.
        //
        // Three independent reasons, any one of which is sufficient:
        //   1. `Grain::Affiliation`. A domain names a chain/brand/studio, not one
        //      thing. `souvla.com` is every Souvla location at once, so projecting
        //      it would write a brand-level identifier into a record about one
        //      restaurant — exactly the mis-resolution this projection exists to
        //      prevent.
        //   2. A bare origin is not a usable anchor downstream anyway: the
        //      consumer's merge-eligibility test requires a path or a query, and a
        //      registrable domain has neither by construction.
        //   3. It is an active hazard, not a neutral omission. An anchor cited by
        //      more than a few dozen records is treated as a supernode and dropped
        //      from the merge graph — and a brand domain is shared by *every*
        //      location of the chain. Emitting it would eventually shatter working
        //      clusters rather than build them.
        //
        // A single-location business is not harmed: its own site is reached as a
        // path-bearing `url:` key, which does project.
        to_url: None,
    },
    KindSpec {
        tag: "google_place_id",
        strong: true,
        anchor_rank: Some(3),
        grain: Grain::Identity,
        normalize: normalize::place_id,
        url_match: Some(match_google_place_id),
        to_url: Some(url_google_place_id),
    },
    KindSpec {
        tag: "imdb",
        strong: true,
        anchor_rank: None,
        grain: Grain::Identity,
        normalize: normalize::imdb,
        url_match: Some(match_imdb),
        to_url: Some(url_imdb),
    },
    KindSpec {
        tag: "phone",
        strong: false,
        anchor_rank: None,
        grain: Grain::Weak,
        normalize: normalize::phone_e164,
        url_match: None,
        // `tel:` is a URI but not an http(s) resource, and phone is a weak
        // corroborator that never drives identity. Nothing to project.
        to_url: None,
    },
    KindSpec {
        tag: "wikidata",
        strong: true,
        anchor_rank: Some(0),
        grain: Grain::Identity,
        normalize: normalize::qid,
        url_match: Some(match_wikidata),
        to_url: Some(url_wikidata),
    },
    KindSpec {
        tag: "tmdb",
        strong: true,
        anchor_rank: None,
        grain: Grain::Identity,
        normalize: normalize::tmdb,
        url_match: Some(match_tmdb),
        to_url: Some(url_tmdb),
    },
    KindSpec {
        tag: "yelp",
        strong: true,
        anchor_rank: Some(4),
        grain: Grain::Identity,
        normalize: normalize::yelp,
        url_match: Some(match_yelp),
        to_url: Some(url_yelp),
    },
    // The generic fallback for a URL at a host no kind above recognizes. `sameAs` is a URL
    // by definition in schema.org and the set of sources is open-ended, so the *default*
    // for a URL must be safe: `normalize::specific_url` accepts only a path-bearing URL
    // (which names one page) and rejects a bare host (which names a site).
    //
    // `Grain::Identity`, because a specific page identifies a specific thing — that is what
    // lets two reviewers who each cite only the same Michelin listing converge, while two
    // restaurants listed on the same site stay distinct.
    //
    // `anchor_rank: Some(5)` — last, below every dedicated kind. A `url:` key is portable
    // but opaque: `wikidata:Q4926426` means something to another consumer, whereas
    // `url:guide.michelin.com/...` is only as durable as one site's URL scheme. It anchors
    // only when nothing better exists.
    //
    // `url_match: None` on purpose. `guess_id_from_url` tries each kind's matcher in
    // registry order and takes the first hit, so a `url` matcher would shadow yelp/imdb/
    // wikidata. It is reached as an explicit fallback instead — see `guess_id_from_url`.
    KindSpec {
        tag: "url",
        strong: true,
        anchor_rank: Some(5),
        grain: Grain::Identity,
        normalize: normalize::specific_url,
        url_match: None,
        to_url: Some(url_specific_url),
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

/// Claims only TMDb **movie** URLs.
///
/// TMDb numbers films and series in separate namespaces, but `normalize::tmdb`
/// reduces any TMDb URL to a bare number — so matching every host path made
/// `/tv/246` and `/movie/246` both normalize to `tmdb:246`, fusing a series and an
/// unrelated film into one entity. That is a false merge, the failure class this
/// project treats as its primary invariant, and it was reachable purely by
/// round-tripping a stored TV identifier back through `guess_id_from_url`.
///
/// A `/tv/` (or any non-movie) TMDb URL now falls through to the generic `url`
/// kind: path-bearing, `Grain::Identity`, never speculatively crosswalked — which
/// is also how the name-search path already stores series.
fn match_tmdb(url: &str) -> Option<String> {
    let host = contains_host(url, "themoviedb.org")?;
    if url.to_ascii_lowercase().contains("/movie/") {
        Some(host)
    } else {
        None
    }
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

/// A Google Maps place link → the bare Place ID.
///
/// Registry ordering note: `guess_id_from_url` walks `KINDS` in order and takes the
/// first matcher that hits *and* normalizes, and this entry sits ahead of
/// imdb/wikidata/tmdb/yelp. It is therefore deliberately narrow — it requires the
/// registrable domain to be `google.<tld>` **and** a `q=place_id:` parameter — so it
/// can never shadow another kind's URL.
///
/// **Only the `?q=place_id:<id>` form**, which is exactly what [`url_google_place_id`]
/// emits and the only Maps URL a Place ID is *recoverable* from. The `?cid=<n>` form
/// carries a different, numeric identifier that cannot be converted to a Place ID
/// without calling Google; it deliberately falls through to the generic `url` kind
/// rather than being guessed at. Before this matcher existed, a Maps URL fed back in
/// classified as `url:` — a *different key* than the `google_place_id:` it came from,
/// which meant echoing a candidate's own URL minted a second entity.
fn match_google_place_id(raw: &str) -> Option<String> {
    // Parsed, not substring-matched: `https://evil.example/google.com?q=place_id:X`
    // is how a crafted string would otherwise smuggle in a Google identity.
    let url = url::Url::parse(raw.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let registrable = psl::domain_str(&host)?;
    if !registrable.starts_with("google.") {
        return None;
    }
    // `query_pairs` percent-decodes, so `q=place_id%3AChIJ…` works too.
    let q = url.query_pairs().find(|(k, _)| k == "q")?.1;
    let id = q.strip_prefix("place_id:")?.trim();
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

// --- URL projections (`to_url`) -------------------------------------------
//
// Each takes an already-NORMALIZED value (never a raw input) and returns the
// canonical public URL for it, or `None` when no faithful URL can be built. See
// the `KindSpec::to_url` docs for the two rules these all obey.

/// Characters that survive being spliced into a URL and parsed back out
/// unchanged (RFC 3986 unreserved). Anything else — a space, `?`, `&`, `#`, a
/// non-ASCII character — would be percent-encoded on the way back in, so the
/// re-parsed value would no longer equal the stored one and the round trip
/// would break. We omit the projection instead of emitting a URL that decodes
/// to a different key.
fn is_url_safe_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~'))
}

/// `yelp:blue-bottle-coffee-san-francisco` → the Yelp business page.
fn url_yelp(value: &str) -> Option<String> {
    is_url_safe_token(value).then(|| format!("https://www.yelp.com/biz/{value}"))
}

/// `google_place_id:ChIJ…` → the Maps deep link.
///
/// The `?q=place_id:` form is chosen because it is the one Maps URL that is
/// *derivable in both directions* from the id alone — [`match_google_place_id`]
/// reads the id straight back out. It also satisfies the consumer's
/// merge-eligibility rule twice over: `/maps/place` is a non-root path *and*
/// the query is non-empty, so it survives either clause being tightened.
fn url_google_place_id(value: &str) -> Option<String> {
    // Place IDs are base64url-ish tokens in practice, but `normalize::place_id`
    // stores whatever it is handed verbatim, so the token check is load-bearing.
    is_url_safe_token(value)
        .then(|| format!("https://www.google.com/maps/place/?q=place_id:{value}"))
}

/// `wikidata:Q83495` → the Wikidata item page.
fn url_wikidata(value: &str) -> Option<String> {
    is_url_safe_token(value).then(|| format!("https://www.wikidata.org/wiki/{value}"))
}

/// `imdb:tt0133093` → the IMDb title page.
fn url_imdb(value: &str) -> Option<String> {
    is_url_safe_token(value).then(|| format!("https://www.imdb.com/title/{value}/"))
}

/// `tmdb:603` → the TMDb page.
///
/// CAVEAT, and it is a real one: `normalize::tmdb` reduces both
/// `/movie/<id>` and `/tv/<id>` to the same bare number, so the media type is
/// **not recoverable** from the stored value and this projection has to assume
/// `/movie/`. For a TV id the emitted link is wrong (it 404s), though the round
/// trip still holds — `match_tmdb`/`normalize::tmdb` read the same number back
/// out either way, so clustering is unaffected and no duplicate entity is
/// minted. The underlying defect is that `tmdb:603` already conflates movie 603
/// with TV series 603 as graph keys; fixing the projection means fixing the
/// normalizer to carry the media type, which is out of scope here.
fn url_tmdb(value: &str) -> Option<String> {
    is_url_safe_token(value).then(|| format!("https://www.themoviedb.org/movie/{value}"))
}

/// `url:guide.michelin.com/…/a16` → `https://guide.michelin.com/…/a16`.
///
/// The stored value is `host + path[?query]` with the scheme dropped, so the
/// projection re-adds `https://`. Two guards keep the round trip exact:
///
/// * re-normalizing the projection must reproduce the value byte for byte
///   (catches anything the URL parser would rewrite);
/// * no *dedicated* kind may claim the projection, because `guess_id_from_url`
///   tries those matchers first — a `url:` value that happens to live on
///   `yelp.com/biz/…` would come back as `yelp:…`, a different key.
fn url_specific_url(value: &str) -> Option<String> {
    if value.is_empty() || value.contains("://") {
        return None;
    }
    let projected = format!("https://{value}");
    if normalize::specific_url(&projected).ok().as_deref() != Some(value) {
        return None;
    }
    if claimed_by_dedicated_kind(&projected) {
        return None;
    }
    Some(projected)
}

/// Would a kind with its own `url_match` claim this URL? Mirrors
/// `guess_id_from_url`'s loop: a matcher hit only counts if the raw value it
/// returns also normalizes, since that function falls through on a normalize
/// error.
fn claimed_by_dedicated_kind(url: &str) -> bool {
    KINDS.iter().any(|spec| {
        spec.url_match
            .and_then(|m| m(url))
            .is_some_and(|raw| (spec.normalize)(&raw).is_ok())
    })
}

/// Project a stored `kind:value` key to its canonical public URL.
///
/// `None` = unknown kind, or a kind with no URL form (`placekey`, `domain`,
/// `phone`). This is the entry point for anything holding a flat key rather
/// than a typed id — a candidate's `anchor`, for instance.
pub fn url_for_key(key: &str) -> Option<String> {
    let (tag, value) = key.split_once(':')?;
    let spec = spec_for_tag(tag)?;
    (spec.to_url?)(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmdb_tv_and_movie_do_not_share_a_key() {
        // Regression: `normalize::tmdb` reduces any TMDb URL to a bare number, so a
        // host-only matcher fused series 246 and film 246 into `tmdb:246` — a false
        // merge reachable just by round-tripping a stored TV identifier.
        assert!(match_tmdb("https://www.themoviedb.org/movie/246").is_some());
        assert!(
            match_tmdb("https://www.themoviedb.org/tv/246").is_none(),
            "a /tv/ URL must fall through to the generic `url` kind, not claim the \
             movie namespace"
        );
        // A person page is likewise not a movie.
        assert!(match_tmdb("https://www.themoviedb.org/person/246").is_none());
    }

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

    // --- URL projections (`to_url`) ---------------------------------------

    /// `(tag, normalized value, canonical URL)` for every kind that projects.
    ///
    /// One row per projecting kind, and `round_trip_table_covers_every_projecting_kind`
    /// fails if a new kind is added without a row — so the round-trip property
    /// below can never quietly stop covering something.
    const PROJECTIONS: &[(&str, &str, &str)] = &[
        (
            "yelp",
            "souvla-hayes-valley-san-francisco",
            "https://www.yelp.com/biz/souvla-hayes-valley-san-francisco",
        ),
        (
            "google_place_id",
            "ChIJN1t_tDeuEmsRUsoyG83frY4",
            "https://www.google.com/maps/place/?q=place_id:ChIJN1t_tDeuEmsRUsoyG83frY4",
        ),
        ("wikidata", "Q83495", "https://www.wikidata.org/wiki/Q83495"),
        ("imdb", "tt0133093", "https://www.imdb.com/title/tt0133093/"),
        ("tmdb", "603", "https://www.themoviedb.org/movie/603"),
        (
            "url",
            "guide.michelin.com/us/en/california/san-francisco/restaurant/a16",
            "https://guide.michelin.com/us/en/california/san-francisco/restaurant/a16",
        ),
    ];

    #[test]
    fn url_projections_have_the_expected_shape() {
        for (tag, value, expected) in PROJECTIONS {
            let spec = spec_for_tag(tag).unwrap();
            let to_url = spec
                .to_url
                .unwrap_or_else(|| panic!("{tag} should project to a URL"));
            assert_eq!(to_url(value).as_deref(), Some(*expected), "kind {tag}");
        }
    }

    /// **The property that matters.** Feed each projected URL back through the
    /// registry's own URL classifier and you must land on the *same*
    /// `kind:value` you started from.
    ///
    /// If this ever breaks, the failure is silent and expensive: a user shown a
    /// candidate list picks one, the caller echoes the candidate's URL back, it
    /// classifies as a *different* key, and a duplicate entity is minted instead
    /// of binding to the entity the user chose. That is exactly what happened
    /// before `match_google_place_id` existed — a Maps URL came back as `url:`.
    #[cfg(feature = "harvest")]
    #[test]
    fn url_projections_round_trip() {
        for (tag, value, url) in PROJECTIONS {
            let id = crate::resolve::guess_id_from_url(url)
                .unwrap_or_else(|| panic!("{url} should classify as some kind"));
            assert_eq!(
                id.key(),
                format!("{tag}:{value}"),
                "round trip broke for {tag}: {url}"
            );
        }
    }

    #[test]
    fn round_trip_table_covers_every_projecting_kind() {
        let covered: Vec<&str> = PROJECTIONS.iter().map(|(tag, _, _)| *tag).collect();
        for spec in KINDS {
            assert_eq!(
                spec.to_url.is_some(),
                covered.contains(&spec.tag),
                "kind {} projects={} but is {}in the round-trip table — add a row \
                 (or a deliberate `to_url: None` with a comment saying why)",
                spec.tag,
                spec.to_url.is_some(),
                if covered.contains(&spec.tag) {
                    ""
                } else {
                    "not "
                }
            );
        }
    }

    #[test]
    fn kinds_without_a_url_form_project_nothing() {
        // placekey: an opaque grid token, no page exists.
        // domain:   Affiliation grain — see the long comment on the registry entry.
        // phone:    weak corroborator, not an http(s) resource.
        for tag in ["placekey", "domain", "phone"] {
            assert!(
                spec_for_tag(tag).unwrap().to_url.is_none(),
                "{tag} must not project to a URL"
            );
        }
        assert_eq!(url_for_key("domain:souvla.com"), None);
        assert_eq!(url_for_key("placekey:223-227@5vg-7gq-tvz"), None);
        assert_eq!(url_for_key("phone:+15106533394"), None);
    }

    #[test]
    fn maps_projection_satisfies_the_consumer_merge_eligibility_rule() {
        // The consumer only clusters on an http(s) URL that carries a path OR a
        // query. The Maps projection holds on BOTH counts independently, so it
        // survives either clause being tightened later.
        let projected = url_for_key("google_place_id:ChIJN1t_tDeuEmsRUsoyG83frY4").unwrap();
        assert!(projected.contains("://"), "{projected}");
        let parsed = url::Url::parse(&projected).unwrap();
        assert!(matches!(parsed.scheme(), "http" | "https"));
        // Non-root path...
        assert_eq!(parsed.path(), "/maps/place/");
        assert!(!parsed.path().trim_matches('/').is_empty(), "{projected}");
        // ...AND a non-empty query.
        assert_eq!(
            parsed.query(),
            Some("q=place_id:ChIJN1t_tDeuEmsRUsoyG83frY4")
        );
    }

    #[test]
    fn maps_url_classifies_as_a_place_id_not_a_generic_url() {
        // The whole point of `match_google_place_id`: before it existed a Maps
        // URL fell through to the `url:` fallback, a different key.
        assert_eq!(
            match_google_place_id(
                "https://www.google.com/maps/place/?q=place_id:ChIJN1t_tDeuEmsRUsoyG83frY4"
            )
            .as_deref(),
            Some("ChIJN1t_tDeuEmsRUsoyG83frY4")
        );
        // Percent-encoded colon and a non-.com Google TLD both work.
        assert_eq!(
            match_google_place_id("https://google.co.uk/maps/place/?q=place_id%3AChIJabc")
                .as_deref(),
            Some("ChIJabc")
        );
        // Extra params either side do not matter.
        assert_eq!(
            match_google_place_id("https://maps.google.com/?hl=en&q=place_id:ChIJabc&z=17")
                .as_deref(),
            Some("ChIJabc")
        );
    }

    #[test]
    fn maps_matcher_is_narrow_by_design() {
        // The `?cid=` form carries a DIFFERENT identifier that cannot be turned
        // into a Place ID without calling Google. Never guess — let it fall
        // through to the generic `url` kind.
        assert_eq!(
            match_google_place_id("https://maps.google.com/?cid=12345"),
            None
        );
        // A Maps URL with no place_id at all.
        assert_eq!(
            match_google_place_id("https://www.google.com/maps/search/?api=1&query=souvla"),
            None
        );
        // Host is parsed, not substring-matched: a crafted path cannot borrow
        // Google's identity.
        assert_eq!(
            match_google_place_id("https://evil.example/google.com/?q=place_id:ChIJabc"),
            None
        );
        // Empty id.
        assert_eq!(
            match_google_place_id("https://www.google.com/maps/place/?q=place_id:"),
            None
        );
        // Non-http schemes and non-URLs.
        assert_eq!(match_google_place_id("q=place_id:ChIJabc"), None);
        assert_eq!(
            match_google_place_id("javascript:alert('?q=place_id:x')"),
            None
        );
    }

    #[test]
    fn projections_refuse_values_a_url_would_rewrite() {
        // A value carrying anything the URL parser would percent-encode cannot
        // be spliced in faithfully, so it is omitted rather than mangled — an
        // omitted anchor costs nothing, a rewritten one mints a duplicate.
        assert_eq!(url_for_key("yelp:blue bottle"), None);
        assert_eq!(url_for_key("google_place_id:ChIJ?x=1"), None);
        assert_eq!(url_for_key("google_place_id:ChIJ&x=1"), None);
        assert_eq!(url_for_key("yelp:"), None);
    }

    #[test]
    fn generic_url_projection_declines_when_another_kind_would_claim_it() {
        // `guess_id_from_url` tries the dedicated matchers first, so projecting
        // `url:yelp.com/biz/x` would come back as `yelp:x` — a different key.
        // Decline instead of breaking the round trip.
        assert_eq!(url_for_key("url:yelp.com/biz/blue-bottle-coffee"), None);
        assert_eq!(url_for_key("url:imdb.com/title/tt0133093"), None);
        // But a URL on an imdb.com page that has no derivable IMDb id is NOT
        // claimed (the matcher hits, the normalizer rejects), mirroring
        // `guess_id_from_url`'s fall-through — so it still projects.
        assert_eq!(
            url_for_key("url:imdb.com/list/ls000000001").as_deref(),
            Some("https://imdb.com/list/ls000000001")
        );
    }

    #[test]
    fn generic_url_projection_refuses_a_value_that_does_not_renormalize() {
        // A path-less value would fail `specific_url`'s "THE guard" on the way
        // back in, so it never projects.
        assert_eq!(url_for_key("url:souvla.com"), None);
        assert_eq!(url_for_key("url:https://example.com/x"), None);
        assert_eq!(url_for_key("url:"), None);
        // A query-only value round-trips (this is the Maps `?cid=` shape).
        assert_eq!(
            url_for_key("url:maps.google.com?cid=12345").as_deref(),
            Some("https://maps.google.com?cid=12345")
        );
    }

    #[test]
    fn url_for_key_rejects_unknown_and_malformed_keys() {
        assert_eq!(url_for_key("not_a_kind:x"), None);
        assert_eq!(url_for_key("no-colon"), None);
    }

    #[test]
    fn grains_are_assigned() {
        // Identity kinds name one thing; a shared domain is only affiliation;
        // phone is a weak corroborator outside the union-find.
        assert_eq!(
            spec_for_tag("google_place_id").unwrap().grain,
            Grain::Identity
        );
        assert_eq!(spec_for_tag("domain").unwrap().grain, Grain::Affiliation);
        assert_eq!(spec_for_tag("phone").unwrap().grain, Grain::Weak);
    }
}
