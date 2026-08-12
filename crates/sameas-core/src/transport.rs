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

/// Real HTTP transport (opt-in). A single blocking client carries a default
/// `User-Agent` — mandatory for the Wikidata SPARQL endpoint (403 without).
#[cfg(feature = "live-fetch")]
pub struct ReqwestTransport {
    client: reqwest::blocking::Client,
}

#[cfg(feature = "live-fetch")]
impl ReqwestTransport {
    pub fn new() -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .user_agent(concat!("sameas/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| anyhow!("building http client: {e}"))?;
        Ok(ReqwestTransport { client })
    }
}

#[cfg(feature = "live-fetch")]
impl HttpTransport for ReqwestTransport {
    fn get_json(&self, url: &str) -> Result<Value> {
        self.client
            .get(url)
            .send()
            .and_then(|r| r.error_for_status())
            .and_then(|r| r.json::<Value>())
            .map_err(|e| anyhow!("GET {url}: {e}"))
    }

    fn post_json(&self, url: &str, headers: &[(&str, &str)], body: &Value) -> Result<Value> {
        let mut req = self.client.post(url).json(body);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        req.send()
            .and_then(|r| r.error_for_status())
            .and_then(|r| r.json::<Value>())
            .map_err(|e| anyhow!("POST {url}: {e}"))
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
