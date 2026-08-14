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
    let (host, from_url) = if raw.contains("://") {
        let url = url::Url::parse(raw).map_err(|e| anyhow!("invalid url {raw:?}: {e}"))?;
        (
            url.host_str()
                .ok_or_else(|| anyhow!("url {raw:?} has no host"))?
                .to_string(),
            true,
        )
    } else {
        // Might still carry a path (e.g. "example.com/foo") or scheme-less "//".
        (
            raw.trim_start_matches("//")
                .split('/')
                .next()
                .unwrap_or(raw)
                .to_string(),
            false,
        )
    };

    let host = host.trim().to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);

    // Reject IP literals (v4/v6): they are not registrable domains, and feeding
    // them to the public-suffix logic collapses them to a misleading two-label
    // "domain" (`1.2.3.4` and `99.88.3.4` would both become `domain:3.4`), a
    // merge hazard. IPv6 hosts from URLs arrive bracketed (`[::1]`).
    let ip_candidate = host.trim_start_matches('[').trim_end_matches(']');
    if ip_candidate.parse::<std::net::IpAddr>().is_ok() {
        bail!("IP literals are not registrable domains: {host:?}");
    }

    // For bare (non-URL) hosts, reject clearly-malformed inputs the URL parser
    // would otherwise have caught: embedded whitespace or empty labels (`..`).
    if !from_url && (host.chars().any(|c| c.is_whitespace()) || host.contains("..")) {
        bail!("malformed host {host:?}");
    }

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
    // A leading `00` is the ITU international call prefix; rewrite it to `+` so
    // an international number (e.g. `00441234567890`) is not misparsed against
    // the US default region (which would yield `+100441234567890`).
    let candidate = match raw.strip_prefix("00") {
        Some(rest) => format!("+{rest}"),
        None => raw.to_string(),
    };
    let region = if candidate.starts_with('+') {
        None
    } else {
        Some(phonenumber::country::US)
    };
    let number = phonenumber::parse(region, &candidate)
        .map_err(|e| anyhow!("invalid phone {raw:?}: {e}"))?;
    // The parser is permissive; require an actually-valid number so junk like
    // `123` or 18-digit garbage is rejected rather than emitted as `phone:+…`.
    if !number.is_valid() {
        bail!("invalid phone {raw:?}");
    }
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
    // `tt` followed by digits. The prefix match is case-insensitive (so
    // `TT0133093` is accepted), but the canonical form always uses lowercase
    // `tt`. Leading zeros are significant for IMDb ids and are preserved.
    let digits = &candidate[2..];
    if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
        Ok(format!("tt{digits}"))
    } else {
        bail!("invalid IMDb id {raw:?}")
    }
}

/// Wikidata → bare `Q…` QID. Accepts a raw QID or a wikidata.org/wiki URL.
pub fn qid(raw: &str) -> Result<String> {
    let raw = raw.trim();
    let candidate = extract_token(raw, "Q")
        .ok_or_else(|| anyhow!("could not find a Wikidata QID (Q…) in {raw:?}"))?;
    // `Q` followed by digits. The prefix match is case-insensitive (so
    // `q83495` is accepted), but the canonical form always uses uppercase `Q`.
    let digits = &candidate[1..];
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        bail!("invalid Wikidata QID {raw:?}");
    }
    // `Q0` (or any all-zero id) is not a valid QID.
    if digits.chars().all(|c| c == '0') {
        bail!("invalid Wikidata QID {raw:?} (id must be non-zero)");
    }
    Ok(format!("Q{digits}"))
}

/// TMDb id → bare numeric string. Accepts a raw number or a themoviedb.org URL.
pub fn tmdb(raw: &str) -> Result<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("empty tmdb id");
    }
    let lower = raw.to_ascii_lowercase();
    // The id is the FIRST numeric segment immediately after `/movie/` or `/tv/`
    // in a URL. Anchoring here avoids grabbing a trailing year from a slug like
    // `/movie/335984-blade-runner-2049` (which would wrongly yield `2049`).
    let id_str = if let Some(idx) = lower.find("/movie/") {
        raw[idx + "/movie/".len()..]
            .split(['/', '-', '?', '#', '=', ' '])
            .next()
            .unwrap_or("")
    } else if let Some(idx) = lower.find("/tv/") {
        raw[idx + "/tv/".len()..]
            .split(['/', '-', '?', '#', '=', ' '])
            .next()
            .unwrap_or("")
    } else if raw.contains("://") || lower.contains("themoviedb.org") {
        // URL-shaped but not a `/movie/` or `/tv/` page — no derivable id.
        bail!("could not find a TMDb id in {raw:?}");
    } else {
        // Bare token, e.g. `603`.
        raw
    };
    if id_str.is_empty() || !id_str.chars().all(|c| c.is_ascii_digit()) {
        bail!("could not find a TMDb numeric id in {raw:?}");
    }
    // Canonicalize leading zeros (`007` → `7`) and reject the non-id `0`.
    let canonical = id_str.trim_start_matches('0');
    if canonical.is_empty() {
        bail!("invalid TMDb id {raw:?} (id must be non-zero)");
    }
    Ok(canonical.to_string())
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

/// Find a `prefix…`-shaped token in a raw string or URL. The prefix match is
/// case-insensitive (so `TT…`/`q…` are accepted); the returned token keeps its
/// original characters, and callers canonicalize the prefix case. Splits on
/// common URL/path separators.
fn extract_token(raw: &str, prefix: &str) -> Option<String> {
    let lower_prefix = prefix.to_ascii_lowercase();
    raw.split(['/', '?', '#', '=', ' ', '\t'])
        .map(|s| s.trim())
        .find(|s| {
            s.len() > prefix.len() && s.to_ascii_lowercase().starts_with(&lower_prefix)
        })
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
    fn registrable_domain_rejects_ip_literals() {
        // H3: raw IPs must not collapse to a two-label "domain" (merge hazard).
        assert!(registrable_domain("1.2.3.4").is_err());
        assert!(registrable_domain("99.88.3.4").is_err());
        assert!(registrable_domain("192.168.1.1").is_err());
        assert!(registrable_domain("10.0.1.1").is_err());
        // Extracted from a URL host, too.
        assert!(registrable_domain("http://192.168.1.1/path").is_err());
        // IPv6, bare and bracketed (URL host form).
        assert!(registrable_domain("::1").is_err());
        assert!(registrable_domain("http://[2001:db8::1]/x").is_err());
    }

    #[test]
    fn registrable_domain_rejects_malformed_bare_hosts() {
        assert!(registrable_domain("foo..com").is_err());
        assert!(registrable_domain("foo bar.com").is_err());
    }

    #[test]
    fn phone_to_e164() {
        assert_eq!(phone_e164("+1-510-653-3394").unwrap(), "+15106533394");
        assert_eq!(phone_e164("(510) 653-3394").unwrap(), "+15106533394");
        assert!(phone_e164("").is_err());
    }

    #[test]
    fn phone_rejects_invalid() {
        // M8: too-short / junk numbers must be rejected, not emitted.
        assert!(phone_e164("123").is_err());
        assert!(phone_e164("123456789012345678").is_err());
    }

    #[test]
    fn phone_handles_00_intl_prefix() {
        // M8: leading `00` (ITU intl prefix) → `+`, not the US default region.
        assert_eq!(phone_e164("00441234567890").unwrap(), "+441234567890");
    }

    #[test]
    fn phone_keeps_valid_forms() {
        // Toll-free vanity + extension stripping still work.
        assert_eq!(phone_e164("1-800-FLOWERS").unwrap(), "+18003569377");
        assert_eq!(phone_e164("+1-510-653-3394 ext. 12").unwrap(), "+15106533394");
    }

    #[test]
    fn imdb_normalization() {
        assert_eq!(imdb("tt0133093").unwrap(), "tt0133093");
        assert_eq!(
            imdb("https://www.imdb.com/title/tt0133093/").unwrap(),
            "tt0133093"
        );
        assert!(imdb("not-an-id").is_err());
        // Case-insensitive prefix; canonical form is lowercase `tt`.
        assert_eq!(imdb("TT0133093").unwrap(), "tt0133093");
    }

    #[test]
    fn qid_normalization() {
        assert_eq!(qid("Q83495").unwrap(), "Q83495");
        assert_eq!(
            qid("https://www.wikidata.org/wiki/Q83495").unwrap(),
            "Q83495"
        );
        assert!(qid("Qxyz").is_err());
        // Case-insensitive prefix (the `wikidata:` key prefix is stripped by the
        // caller, so `q83495` is the value that reaches this normalizer);
        // canonical form is uppercase `Q`.
        assert_eq!(qid("q83495").unwrap(), "Q83495");
        // `Q0` is not a valid id.
        assert!(qid("Q0").is_err());
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
    fn tmdb_anchors_id_after_movie_or_tv() {
        // H4: the id is the first segment after /movie/ or /tv/, not a trailing
        // year in the slug or query string.
        assert_eq!(
            tmdb("https://www.themoviedb.org/movie/335984-blade-runner-2049").unwrap(),
            "335984"
        );
        assert_eq!(
            tmdb("https://www.themoviedb.org/movie/603-the-matrix-1999").unwrap(),
            "603"
        );
        assert_eq!(
            tmdb("https://www.themoviedb.org/movie/603?year=2024").unwrap(),
            "603"
        );
        assert_eq!(tmdb("https://www.themoviedb.org/tv/1399-game-of-thrones").unwrap(), "1399");
    }

    #[test]
    fn tmdb_leading_zeros_and_junk() {
        // Canonicalize leading zeros; reject the non-id `0` and non-/movie URLs.
        assert_eq!(tmdb("00603").unwrap(), "603");
        assert!(tmdb("0").is_err());
        assert!(tmdb("themoviedb.org/person/123").is_err());
        assert!(tmdb("").is_err());
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
