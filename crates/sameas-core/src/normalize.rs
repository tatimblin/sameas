//! Lightweight, deterministic normalizers.
//!
//! Each function turns a messy raw input (a URL, a formatted phone number, a
//! wiki link) into the canonical value stored in the crosswalk graph.

use anyhow::{anyhow, bail, Result};

/// A **specific** web page → a stable `host/path[?query]` identity key.
///
/// The generic fallback for a URL at a host no dedicated kind recognizes. Since `sameAs`
/// is a URL by definition in schema.org and the set of sources is open-ended (Michelin,
/// OpenTable, TripAdvisor, whatever comes next), a per-host allowlist can never keep up —
/// so the *default* for a URL has to be safe.
///
/// **Rejects a path-less URL**, and that rejection is the whole safety property. A bare
/// `https://guide.michelin.com/` names a directory, not a restaurant; typing it as an
/// identity key would merge every business listed there. Path-less URLs fall through to
/// the caller's `domain` fallback, where `Grain::Affiliation` already handles a shared
/// host correctly. Only a URL with a discriminating path or query identifies one thing.
///
/// Normalization keeps identity stable across trivial spelling differences without
/// merging distinct pages:
/// * host lowercased, `www.` and a default port dropped — case and `www` are not identity
/// * fragment dropped — `#reviews` is a position on one page
/// * **path case and the query preserved** — RFC 3986 makes paths case-sensitive, and
///   `?cid=1` vs `?cid=2` are different places
/// * one trailing slash trimmed, so `/restaurant/a16` and `/restaurant/a16/` agree
///
/// Examples:
/// * `https://guide.michelin.com/us/en/.../a16` → `guide.michelin.com/us/en/.../a16`
/// * `https://joes-diner.weebly.com/menu` → `joes-diner.weebly.com/menu` (the **full**
///   host, so two tenants of one site builder never collide)
/// * `https://guide.michelin.com/` → error (path-less)
pub fn specific_url(raw: &str) -> Result<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("empty url");
    }
    // Parsed, not string-munged: a substring check is how a crafted URL smuggles in
    // another host's identity.
    let url = url::Url::parse(raw).map_err(|e| anyhow!("invalid url {raw:?}: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("not an http(s) url: {raw:?}");
    }

    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("url {raw:?} has no host"))?
        .trim()
        .to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    // Same reasoning as `registrable_domain`: an IP literal is not a name, and treating
    // one as identity is a merge hazard.
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if bare.parse::<std::net::IpAddr>().is_ok() {
        bail!("IP literals are not identity keys: {host:?}");
    }
    if host.is_empty() || !host.contains('.') {
        bail!("url {raw:?} has no resolvable host");
    }

    let path = url.path().trim_end_matches('/');
    let query = url.query().unwrap_or("");
    // THE guard. Nothing discriminating → this URL names a site, not a thing.
    if path.is_empty() && query.is_empty() {
        bail!(
            "url {raw:?} has no path or query, so it names a site rather than a specific \
             thing; use `domain` for a bare host"
        );
    }

    let mut key = format!("{host}{path}");
    if !query.is_empty() {
        key.push('?');
        key.push_str(query);
    }
    Ok(key)
}

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

    // --- specific_url: the generic URL identity key ---

    #[test]
    fn specific_url_rejects_a_path_less_url() {
        // THE safety property. A bare directory host names a site, not a restaurant;
        // accepting it as an Identity key would merge everything listed there. These fall
        // through to `domain`, where Affiliation grain handles a shared host correctly.
        assert!(specific_url("https://guide.michelin.com/").is_err());
        assert!(specific_url("https://guide.michelin.com").is_err());
        assert!(specific_url("https://www.yelp.com/").is_err());
        // A path of only slashes is still no path.
        assert!(specific_url("https://example.com///").is_err());
    }

    #[test]
    fn specific_url_keeps_a_directory_listing_per_thing() {
        // The case the whole kind exists for: two restaurants in one directory get
        // different keys, so they never merge — while two reviewers citing the SAME
        // listing get the same key, so they do.
        let a16 =
            specific_url("https://guide.michelin.com/us/en/california/san-francisco/restaurant/a16")
                .unwrap();
        let flour = specific_url(
            "https://guide.michelin.com/us/en/california/san-francisco/restaurant/flour-water",
        )
        .unwrap();
        assert_ne!(a16, flour);
        assert_eq!(
            a16,
            "guide.michelin.com/us/en/california/san-francisco/restaurant/a16"
        );
    }

    #[test]
    fn specific_url_keeps_the_full_host_so_site_builders_stay_distinct() {
        // `registrable_domain` would reduce both to `weebly.com` and merge two unrelated
        // businesses. The full host is what prevents that.
        let joe = specific_url("https://joes-diner.weebly.com/menu").unwrap();
        let maria = specific_url("https://maria-tacos.weebly.com/menu").unwrap();
        assert_ne!(joe, maria);
        assert!(joe.starts_with("joes-diner.weebly.com"));
    }

    #[test]
    fn specific_url_normalizes_only_what_is_not_identity() {
        // Host case and `www.` are not identity; a fragment is a position on one page; a
        // trailing slash is a spelling difference. All collapse to one key.
        let variants = [
            "https://Guide.Michelin.com/us/en/restaurant/a16",
            "https://www.guide.michelin.com/us/en/restaurant/a16",
            "https://guide.michelin.com/us/en/restaurant/a16/",
            "https://guide.michelin.com/us/en/restaurant/a16#reviews",
            "https://guide.michelin.com:443/us/en/restaurant/a16",
        ];
        let keys: std::collections::HashSet<String> =
            variants.iter().map(|v| specific_url(v).unwrap()).collect();
        assert_eq!(keys.len(), 1, "{keys:?}");
    }

    #[test]
    fn specific_url_preserves_path_case_and_query() {
        // RFC 3986 makes paths case-sensitive, and `?cid=1` vs `?cid=2` are different
        // places. Lowercasing or dropping either would merge distinct things.
        let a = specific_url("https://maps.google.com/?cid=111").unwrap();
        let b = specific_url("https://maps.google.com/?cid=222").unwrap();
        assert_ne!(a, b);
        assert!(a.ends_with("?cid=111"));

        let upper = specific_url("https://example.com/Path/To/Thing").unwrap();
        assert!(upper.ends_with("/Path/To/Thing"), "{upper}");
    }

    #[test]
    fn specific_url_accepts_a_query_only_url() {
        // A query is discriminating even with no path — this is how Google Maps `?cid=`
        // links identify a place.
        assert!(specific_url("https://maps.google.com/?cid=12345").is_ok());
    }

    #[test]
    fn specific_url_rejects_non_http_and_ip_hosts() {
        // A crafted or non-web URL must not become an identity key. An IP literal is not a
        // name — the same merge hazard `registrable_domain` rejects it for.
        assert!(specific_url("ftp://example.com/file").is_err());
        assert!(specific_url("javascript:alert(1)").is_err());
        assert!(specific_url("not a url").is_err());
        assert!(specific_url("").is_err());
        assert!(specific_url("https://1.2.3.4/page").is_err());
        assert!(specific_url("https://[::1]/page").is_err());
        // No dot in the host: not resolvable.
        assert!(specific_url("https://localhost/page").is_err());
    }

    #[test]
    fn specific_url_is_parsed_not_substring_matched() {
        // A hostile URL naming another host in its path or query must key on ITS OWN host,
        // never the one it mentions.
        let key = specific_url("https://evil.com/?ref=guide.michelin.com/restaurant/a16").unwrap();
        assert!(key.starts_with("evil.com"), "{key}");
        let key2 = specific_url("https://evil.com/guide.michelin.com/restaurant/a16").unwrap();
        assert!(key2.starts_with("evil.com/"), "{key2}");
    }

    // --- The normalizer half of the URL-projection round trip ---------------
    //
    // `KindSpec::to_url` projects a stored value back to a canonical public URL
    // (see `kind.rs`). `kind::tests::url_projections_round_trip` asserts the
    // whole loop through the registry's classifier; these assert the piece that
    // lives here — that each normalizer reads its own projection back out
    // unchanged. Kept next to the normalizers so a change to one of them fails
    // here, where the reason is obvious, rather than only in `kind.rs`.

    #[test]
    fn normalizers_read_back_their_own_url_projections() {
        assert_eq!(
            yelp("https://www.yelp.com/biz/souvla-hayes-valley-san-francisco").unwrap(),
            "souvla-hayes-valley-san-francisco"
        );
        assert_eq!(
            place_id("ChIJN1t_tDeuEmsRUsoyG83frY4").unwrap(),
            "ChIJN1t_tDeuEmsRUsoyG83frY4"
        );
        assert_eq!(
            qid("https://www.wikidata.org/wiki/Q83495").unwrap(),
            "Q83495"
        );
        assert_eq!(
            imdb("https://www.imdb.com/title/tt0133093/").unwrap(),
            "tt0133093"
        );
        assert_eq!(tmdb("https://www.themoviedb.org/movie/603").unwrap(), "603");
        let michelin = "guide.michelin.com/us/en/california/san-francisco/restaurant/a16";
        assert_eq!(
            specific_url(&format!("https://{michelin}")).unwrap(),
            michelin
        );
    }

    #[test]
    fn maps_place_id_projection_is_merge_eligible() {
        // The downstream consumer only clusters records on an http(s) URL that
        // carries a path OR a query; a bare origin is dropped. The Maps
        // projection satisfies BOTH clauses independently, so it survives either
        // one being tightened later.
        let projected = "https://www.google.com/maps/place/?q=place_id:ChIJN1t_tDeuEmsRUsoyG83frY4";
        let parsed = url::Url::parse(projected).unwrap();
        assert!(matches!(parsed.scheme(), "http" | "https"));
        assert!(!parsed.path().trim_matches('/').is_empty(), "needs a path");
        assert!(!parsed.query().unwrap_or("").is_empty(), "needs a query");

        // And `specific_url` — the strictest gate in this file, which rejects a
        // path-less URL outright — accepts it too.
        let key = specific_url(projected).unwrap();
        assert_eq!(
            key,
            "google.com/maps/place?q=place_id:ChIJN1t_tDeuEmsRUsoyG83frY4"
        );
    }

    #[test]
    fn a_bare_brand_origin_is_still_refused() {
        // The negative control for the projection table: `domain` deliberately
        // has no `to_url`, and this is why. `https://souvla.com` names the
        // chain, not a location, and it is not a usable clustering anchor —
        // exactly the record that can never collapse with anything.
        assert!(specific_url("https://souvla.com").is_err());
        assert_eq!(
            registrable_domain("https://souvla.com").unwrap(),
            "souvla.com"
        );
        // The escape hatch: a location page projects and clusters fine.
        assert_eq!(
            specific_url("https://souvla.com/hayes-valley").unwrap(),
            "souvla.com/hayes-valley"
        );
    }
}
