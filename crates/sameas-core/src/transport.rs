//! HTTP transport abstraction for hub adapters.
//!
//! All M2 hubs speak JSON over HTTP: Wikidata SPARQL (`GET`), TMDb (`GET`),
//! Google Places (`GET`), and Placekey (`POST` + `apikey` header). This module
//! centralizes the request/parse so adapters only build URLs and read fields.
//!
//! Two implementations mirror M1's `DomainResolver::from_fixture` / `from_live`
//! split:
//! * [`FixtureTransport`] — offline, deterministic; serves canned JSON keyed by
//!   a **secret-stripped** request signature. Used by tests and the demo so CI
//!   stays offline.
//! * `ReqwestTransport` — real HTTP, behind the `live-fetch` feature.

use anyhow::{anyhow, Result};
use serde_json::Value;
use std::collections::HashMap;

/// Query-parameter names that carry API secrets. They are stripped from the
/// fixture request signature so canned fixtures never embed a key and still
/// match regardless of which (or whether a) key is supplied.
pub const SECRET_PARAMS: &[&str] = &["key", "api_key", "apikey"];

/// Anything that can fetch JSON from a hub. Adapters capture one of these at
/// construction (`Arc<dyn HttpTransport>`), since `Resolver::harvest(&self)`
/// takes no transport parameter.
pub trait HttpTransport {
    /// HTTP GET returning parsed JSON.
    fn get_json(&self, url: &str) -> Result<Value>;

    /// HTTP GET with request headers (e.g. `X-Goog-Api-Key` + `X-Goog-FieldMask`
    /// for Google Places API New). Defaults to a plain GET — offline fixtures
    /// match on the URL and ignore headers, so they need no override.
    fn get_json_with_headers(&self, url: &str, _headers: &[(&str, &str)]) -> Result<Value> {
        self.get_json(url)
    }

    /// HTTP POST with headers and a JSON body, returning parsed JSON.
    fn post_json(&self, url: &str, headers: &[(&str, &str)], body: &Value) -> Result<Value>;
}

/// A canonical, secret-free signature of a request, used as the fixture key.
/// Format: `METHOD host+path?sorted-non-secret-query`. The body is deliberately
/// ignored (Placekey's body varies by name/address; the endpoint alone is a
/// sufficient fixture discriminator for our deterministic tests).
pub fn request_sig(method: &str, url: &str) -> String {
    // Split off the query string.
    let (base, query) = match url.split_once('?') {
        Some((b, q)) => (b, q),
        None => (url, ""),
    };
    // Strip scheme so http/https don't matter.
    let base = base.split_once("://").map(|(_, rest)| rest).unwrap_or(base);
    let base = base.trim_end_matches('/');

    let mut params: Vec<(String, String)> = query
        .split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| match kv.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => (kv.to_string(), String::new()),
        })
        .filter(|(k, _)| !SECRET_PARAMS.contains(&k.as_str()))
        .collect();
    params.sort();
    let query = params
        .into_iter()
        .map(|(k, v)| if v.is_empty() { k } else { format!("{k}={v}") })
        .collect::<Vec<_>>()
        .join("&");

    if query.is_empty() {
        format!("{method} {base}")
    } else {
        format!("{method} {base}?{query}")
    }
}

/// A coarser signature: `METHOD host+path`, ignoring the query string entirely.
/// Used for directory-loaded fixtures, where one response per endpoint path is
/// enough (and hand-authoring full encoded query strings — e.g. SPARQL — would
/// be brittle).
pub fn path_sig(method: &str, url: &str) -> String {
    let base = url.split('?').next().unwrap_or(url);
    let base = base.split_once("://").map(|(_, r)| r).unwrap_or(base);
    format!("{method} {}", base.trim_end_matches('/'))
}

/// Offline transport that serves canned JSON.
///
/// Two tiers: exact [`request_sig`] matches (used by `from_pairs`, precise for
/// unit tests) fall back to [`path_sig`] matches (used by `from_dir`, one
/// response per endpoint path — convenient for the demo).
pub struct FixtureTransport {
    exact: HashMap<String, Value>,
    by_path: HashMap<String, Value>,
}

impl FixtureTransport {
    /// Build from an explicit list of `(method, url, json)` triples — handy in
    /// tests. The url is reduced to its signature, so the api key (if any) in
    /// the url does not matter.
    pub fn from_pairs(pairs: Vec<(&str, &str, Value)>) -> Self {
        let exact = pairs
            .into_iter()
            .map(|(method, url, v)| (request_sig(method, url), v))
            .collect();
        FixtureTransport {
            exact,
            by_path: HashMap::new(),
        }
    }

    /// Load `*.json` fixture files from a directory. Each file is
    /// `{ "method": "GET"|"POST", "url": "<endpoint url>", "response": <json> }`
    /// and is matched at the endpoint-path level (query ignored).
    pub fn from_dir(dir: &std::path::Path) -> Result<Self> {
        let mut by_path = HashMap::new();
        let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
            .map_err(|e| anyhow!("reading fixtures dir {}: {e}", dir.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json"))
            .collect();
        files.sort();
        for path in files {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| anyhow!("reading fixture {}: {e}", path.display()))?;
            let v: Value = serde_json::from_str(&text)
                .map_err(|e| anyhow!("parsing fixture {}: {e}", path.display()))?;
            let method = v.get("method").and_then(|m| m.as_str()).unwrap_or("GET");
            let url = v
                .get("url")
                .and_then(|u| u.as_str())
                .ok_or_else(|| anyhow!("fixture {} missing \"url\"", path.display()))?;
            let response = v
                .get("response")
                .cloned()
                .ok_or_else(|| anyhow!("fixture {} missing \"response\"", path.display()))?;
            by_path.insert(path_sig(method, url), response);
        }
        Ok(FixtureTransport {
            exact: HashMap::new(),
            by_path,
        })
    }

    fn lookup(&self, method: &str, url: &str) -> Result<Value> {
        if let Some(v) = self.exact.get(&request_sig(method, url)) {
            return Ok(v.clone());
        }
        if let Some(v) = self.by_path.get(&path_sig(method, url)) {
            return Ok(v.clone());
        }
        Err(anyhow!(
            "no fixture for request {:?}",
            request_sig(method, url)
        ))
    }
}

impl HttpTransport for FixtureTransport {
    fn get_json(&self, url: &str) -> Result<Value> {
        self.lookup("GET", url)
    }

    fn post_json(&self, url: &str, _headers: &[(&str, &str)], _body: &Value) -> Result<Value> {
        self.lookup("POST", url)
    }
}

/// Real HTTP transport (opt-in). A single reused blocking client carries a
/// descriptive `User-Agent` (mandatory for Wikidata SPARQL — 403 without),
/// sensible timeouts, gzip, and a JSON `Accept`. Calls retry transient failures
/// (timeouts, 429, 5xx) and map non-2xx to a clear, class-named error.
#[cfg(feature = "live-fetch")]
pub struct ReqwestTransport {
    client: reqwest::blocking::Client,
    max_attempts: u32,
}

#[cfg(feature = "live-fetch")]
impl ReqwestTransport {
    pub fn new() -> Result<Self> {
        use reqwest::header::{HeaderMap, HeaderValue, ACCEPT};
        use std::time::Duration;

        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));

        let client = reqwest::blocking::Client::builder()
            // Descriptive UA per Wikimedia's User-Agent policy.
            .user_agent(concat!(
                "sameas/",
                env!("CARGO_PKG_VERSION"),
                " (https://example.com/sameas) reqwest"
            ))
            .default_headers(headers)
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(5))
            .gzip(true)
            .build()
            .map_err(|e| anyhow!("building http client: {e}"))?;
        Ok(ReqwestTransport {
            client,
            max_attempts: 3,
        })
    }

    /// Send with bounded retry on transient failures (transport timeout/connect,
    /// HTTP 429, 5xx), honoring `Retry-After` when present, else exponential
    /// backoff. Never retries other 4xx.
    fn send_with_retry(
        &self,
        build: impl Fn() -> reqwest::blocking::RequestBuilder,
    ) -> reqwest::Result<reqwest::blocking::Response> {
        use std::time::Duration;
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            match build().send() {
                Ok(resp) => {
                    let status = resp.status();
                    let transient =
                        status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
                    if transient && attempt < self.max_attempts {
                        let wait = resp
                            .headers()
                            .get(reqwest::header::RETRY_AFTER)
                            .and_then(|v| v.to_str().ok())
                            .and_then(|s| s.parse::<u64>().ok())
                            .map(Duration::from_secs)
                            .unwrap_or_else(|| Duration::from_millis(200 * 2u64.pow(attempt)));
                        std::thread::sleep(wait);
                        continue;
                    }
                    return Ok(resp);
                }
                Err(e) if attempt < self.max_attempts && (e.is_timeout() || e.is_connect()) => {
                    std::thread::sleep(Duration::from_millis(200 * 2u64.pow(attempt)));
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Read a response as JSON, mapping a non-2xx to an `anyhow` error that names
    /// the class (auth / not-found / rate-limited / other) with a body snippet —
    /// so callers can tell "bad key" from "no data".
    fn read_json(resp: reqwest::blocking::Response, what: &str) -> Result<Value> {
        let status = resp.status();
        if status.is_success() {
            return resp
                .json::<Value>()
                .map_err(|e| anyhow!("{what}: decoding response body: {e}"));
        }
        let body = resp.text().unwrap_or_default();
        let snippet: String = body.chars().take(300).collect();
        match status.as_u16() {
            401 | 403 => Err(anyhow!(
                "{what}: authentication/authorization denied (HTTP {status}): {snippet}"
            )),
            404 => Err(anyhow!("{what}: not found (HTTP 404): {snippet}")),
            429 => Err(anyhow!("{what}: rate limited (HTTP 429): {snippet}")),
            _ => Err(anyhow!("{what}: unexpected HTTP {status}: {snippet}")),
        }
    }
}

#[cfg(feature = "live-fetch")]
impl HttpTransport for ReqwestTransport {
    fn get_json(&self, url: &str) -> Result<Value> {
        self.get_json_with_headers(url, &[])
    }

    fn get_json_with_headers(&self, url: &str, headers: &[(&str, &str)]) -> Result<Value> {
        let resp = self
            .send_with_retry(|| {
                let mut req = self.client.get(url);
                for (k, v) in headers {
                    req = req.header(*k, *v);
                }
                req
            })
            .map_err(|e| anyhow!("GET {url}: {e}"))?;
        Self::read_json(resp, &format!("GET {url}"))
    }

    fn post_json(&self, url: &str, headers: &[(&str, &str)], body: &Value) -> Result<Value> {
        let resp = self
            .send_with_retry(|| {
                let mut req = self.client.post(url).json(body);
                for (k, v) in headers {
                    req = req.header(*k, *v);
                }
                req
            })
            .map_err(|e| anyhow!("POST {url}: {e}"))?;
        Self::read_json(resp, &format!("POST {url}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn signature_strips_secrets_and_sorts_params() {
        let a = request_sig(
            "GET",
            "https://maps.googleapis.com/maps/api/place/details/json?key=SECRET&place_id=ChIJ&fields=website",
        );
        let b = request_sig(
            "GET",
            "http://maps.googleapis.com/maps/api/place/details/json/?fields=website&place_id=ChIJ&api_key=OTHER",
        );
        // Same signature despite different secret, scheme, param order, trailing slash.
        assert_eq!(a, b);
        assert!(!a.contains("SECRET"));
        assert!(a.contains("place_id=ChIJ"));
    }

    #[test]
    fn fixture_transport_serves_by_signature() {
        let t = FixtureTransport::from_pairs(vec![(
            "GET",
            "https://query.wikidata.org/sparql?format=json&query=abc",
            json!({"ok": true}),
        )]);
        // Different key/order still matches the fixture.
        let got = t
            .get_json("https://query.wikidata.org/sparql?query=abc&format=json")
            .unwrap();
        assert_eq!(got, json!({"ok": true}));
        assert!(t.get_json("https://query.wikidata.org/sparql?query=zzz").is_err());
    }
}
