//! Lightweight, deterministic normalizers.
//!
//! Each function turns a messy raw input (a URL, a formatted phone number, a
//! wiki link) into the canonical value stored in the crosswalk graph.

use anyhow::{anyhow, bail, Result};

/// URL or bare host → registrable domain (eTLD+1), lowercased, `www.` stripped.
///
/// Uses the embedded Public Suffix List (`psl`), so it is offline and
/// deterministic. Examples:
/// * `https://www.bluebottlecoffee.com/menu` → `bluebottlecoffee.com`
/// * `shop.example.co.uk` → `example.co.uk`
pub fn registrable_domain(raw: &str) -> Result<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("empty domain");
    }

    // Extract a host from either a full URL or a bare hostname.
    let host = if raw.contains("://") {
        let url = url::Url::parse(raw).map_err(|e| anyhow!("invalid url {raw:?}: {e}"))?;
        url.host_str()
            .ok_or_else(|| anyhow!("url {raw:?} has no host"))?
            .to_string()
    } else {
        // Might still carry a path (e.g. "example.com/foo") or scheme-less "//".
        raw.trim_start_matches("//")
            .split('/')
            .next()
            .unwrap_or(raw)
            .to_string()
    };

    let host = host.trim().to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);

    let registrable = psl::domain_str(host)
        .ok_or_else(|| anyhow!("could not determine registrable domain for {host:?}"))?;
    Ok(registrable.to_string())
}

/// Phone → E.164. Defaults to the US region when the number is not written in
/// international (`+`) form. Phone is a corroborator only; normalization does
/// not imply it is safe to merge on.
pub fn phone_e164(raw: &str) -> Result<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("empty phone");
    }
    let region = if raw.starts_with('+') {
        None
    } else {
        Some(phonenumber::country::US)
    };
    let number =
        phonenumber::parse(region, raw).map_err(|e| anyhow!("invalid phone {raw:?}: {e}"))?;
    Ok(number
        .format()
        .mode(phonenumber::Mode::E164)
        .to_string())
}

/// Bare Google Place ID. We do not scrape or validate against Google; a place
/// id is an opaque token, so we just trim it and reject empties.
pub fn place_id(raw: &str) -> Result<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("empty place_id");
    }
    Ok(raw.to_string())
}

/// IMDb id → bare `tt…` form. Accepts a raw id or an imdb.com title URL.
pub fn imdb(raw: &str) -> Result<String> {
    let raw = raw.trim();
    let candidate = extract_token(raw, "tt")
        .ok_or_else(|| anyhow!("could not find an IMDb id (tt…) in {raw:?}"))?;
    // tt followed by digits.
    if candidate.len() > 2 && candidate[2..].chars().all(|c| c.is_ascii_digit()) {
        Ok(candidate)
    } else {
        bail!("invalid IMDb id {raw:?}")
    }
}

/// Wikidata → bare `Q…` QID. Accepts a raw QID or a wikidata.org/wiki URL.
pub fn qid(raw: &str) -> Result<String> {
    let raw = raw.trim();
    let candidate = extract_token(raw, "Q")
        .ok_or_else(|| anyhow!("could not find a Wikidata QID (Q…) in {raw:?}"))?;
    if candidate.len() > 1 && candidate[1..].chars().all(|c| c.is_ascii_digit()) {
        Ok(candidate)
    } else {
        bail!("invalid Wikidata QID {raw:?}")
    }
}

/// TMDb id → bare numeric string. Accepts a raw number or a themoviedb.org URL.
pub fn tmdb(raw: &str) -> Result<String> {
    let raw = raw.trim();
    // Take the last all-digit path/token segment.
    let token = raw
        .split(['/', '-', '?', '#', '=', ' '])
        .rfind(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
        .ok_or_else(|| anyhow!("could not find a TMDb numeric id in {raw:?}"))?;
    Ok(token.to_string())
}

/// Yelp business → canonical biz slug. Accepts a full Yelp biz URL like
/// `https://www.yelp.com/biz/blue-bottle-coffee-san-francisco?foo=1` (scheme,
/// host, query and trailing slash are stripped) or a bare slug. Lowercased.
pub fn yelp(raw: &str) -> Result<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("empty yelp id");
    }
    let lower = raw.to_ascii_lowercase();
    // If it looks like a Yelp biz URL, take the segment after `/biz/`.
    let slug = if let Some(idx) = lower.find("/biz/") {
        &lower[idx + "/biz/".len()..]
    } else if lower.contains("://") || lower.contains("yelp.com") {
        // URL-shaped but not a `/biz/` page — we can't derive a business slug.
        bail!("could not find a Yelp biz slug in {raw:?}");
    } else {
        // Treat as a bare slug.
        lower.as_str()
    };
    // Strip any trailing path, query, or fragment.
    let slug = slug
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(slug)
        .trim();
    if slug.is_empty() {
        bail!("could not find a Yelp biz slug in {raw:?}");
    }
    Ok(slug.to_string())
}

/// Placekey → canonical form. A Placekey is `What@Where` or `@Where`, where the
/// segments are dash-joined base-32-ish tokens (e.g. `223-227@5vg-7gq-tvz` or
/// `@5vg-7gq-tvz`). We validate the `@`-delimited shape and lowercase it; we do
/// not call the Placekey API here.
pub fn placekey(raw: &str) -> Result<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("empty placekey");
    }
    let lower = raw.to_ascii_lowercase();
    let (what, wher) = lower
        .split_once('@')
        .ok_or_else(|| anyhow!("placekey must be What@Where or @Where, got {raw:?}"))?;
    // Each part is a dash-joined run of non-empty alphanumeric segments. The
    // What part may be empty (leading `@`); the Where part must be present.
    let ok_part = |p: &str| {
        !p.is_empty()
            && p.split('-')
                .all(|seg| !seg.is_empty() && seg.chars().all(|c| c.is_ascii_alphanumeric()))
    };
    if !what.is_empty() && !ok_part(what) {
        bail!("invalid placekey What part in {raw:?}");
    }
    if !ok_part(wher) {
        bail!("invalid placekey Where part in {raw:?}");
    }
    Ok(lower)
}

/// Normalize a display name / title / qualifier into a match key for the local
/// name index: lowercased, trimmed, internal whitespace collapsed, and
/// surrounding (not internal) punctuation stripped per word. Deterministic and
/// dependency-free — this is **exact** normalization, NOT fuzzy matching (no
/// stemming, no stop-word removal, no alias folding). `"  Blue  Bottle Café! "`
/// → `"blue bottle café"`; `"San Francisco"` → `"san francisco"`.
pub fn name_key(raw: &str) -> String {
    raw.split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Find a `prefix…`-shaped token in a raw string or URL (case-sensitive on the
/// prefix). Splits on common URL/path separators.
fn extract_token(raw: &str, prefix: &str) -> Option<String> {
    raw.split(['/', '?', '#', '=', ' ', '\t'])
        .map(|s| s.trim())
        .find(|s| s.starts_with(prefix) && s.len() > prefix.len())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registrable_domain_extraction() {
        assert_eq!(
            registrable_domain("https://www.bluebottlecoffee.com/menu?x=1").unwrap(),
            "bluebottlecoffee.com"
        );
        assert_eq!(
            registrable_domain("BlueBottleCoffee.com").unwrap(),
            "bluebottlecoffee.com"
        );
        assert_eq!(
            registrable_domain("shop.example.co.uk").unwrap(),
            "example.co.uk"
        );
        assert!(registrable_domain("").is_err());
    }

    #[test]
    fn phone_to_e164() {
        assert_eq!(phone_e164("+1-510-653-3394").unwrap(), "+15106533394");
        assert_eq!(phone_e164("(510) 653-3394").unwrap(), "+15106533394");
        assert!(phone_e164("").is_err());
    }

    #[test]
    fn imdb_normalization() {
        assert_eq!(imdb("tt0133093").unwrap(), "tt0133093");
        assert_eq!(
            imdb("https://www.imdb.com/title/tt0133093/").unwrap(),
            "tt0133093"
        );
        assert!(imdb("not-an-id").is_err());
    }

    #[test]
    fn qid_normalization() {
        assert_eq!(qid("Q83495").unwrap(), "Q83495");
        assert_eq!(
            qid("https://www.wikidata.org/wiki/Q83495").unwrap(),
            "Q83495"
        );
        assert!(qid("Qxyz").is_err());
    }

    #[test]
    fn tmdb_normalization() {
        assert_eq!(tmdb("603").unwrap(), "603");
        assert_eq!(
            tmdb("https://www.themoviedb.org/movie/603-the-matrix").unwrap(),
            "603"
        );
    }

    #[test]
    fn yelp_normalization() {
        assert_eq!(
            yelp("https://www.yelp.com/biz/blue-bottle-coffee-san-francisco?foo=1").unwrap(),
            "blue-bottle-coffee-san-francisco"
        );
        assert_eq!(
            yelp("https://www.yelp.com/biz/blue-bottle-coffee-san-francisco/").unwrap(),
            "blue-bottle-coffee-san-francisco"
        );
        // Bare slug (case-normalized) is accepted too.
        assert_eq!(
            yelp("Blue-Bottle-Coffee-San-Francisco").unwrap(),
            "blue-bottle-coffee-san-francisco"
        );
        assert!(yelp("").is_err());
        // A Yelp URL that is not a /biz/ page has no derivable slug.
        assert!(yelp("https://www.yelp.com/search?find_desc=coffee").is_err());
    }

    #[test]
    fn name_key_normalization() {
        assert_eq!(name_key("  Blue  Bottle  Coffee "), "blue bottle coffee");
        assert_eq!(name_key("Basecamp restaurant"), "basecamp restaurant");
        assert_eq!(name_key("San Francisco"), "san francisco");
        // Surrounding punctuation stripped, internal kept.
        assert_eq!(name_key("Joe's Pizza!"), "joe's pizza");
        assert_eq!(name_key("(1999)"), "1999");
        assert_eq!(name_key("   "), "");
    }

    #[test]
    fn placekey_normalization() {
        assert_eq!(placekey("223-227@5vg-7gq-tvz").unwrap(), "223-227@5vg-7gq-tvz");
        assert_eq!(placekey("@5vg-7gq-tvz").unwrap(), "@5vg-7gq-tvz");
        // Case-normalized.
        assert_eq!(placekey("@5VG-7GQ-TVZ").unwrap(), "@5vg-7gq-tvz");
        // Missing '@', empty Where, and empty input are rejected.
        assert!(placekey("5vg-7gq-tvz").is_err());
        assert!(placekey("223-227@").is_err());
        assert!(placekey("").is_err());
    }
}
