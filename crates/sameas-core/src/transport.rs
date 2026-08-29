//! HTTP transport abstraction for hub adapters.
//!
//! All M2 hubs speak JSON over HTTP: Wikidata SPARQL (`GET`), TMDb (`GET`),
//! Google Places (`GET`), and Placekey (`POST` + `apikey` header). This module
//! centralizes the request/parse so adapters only build URLs and read fields.
//!
//! Three implementations, one per environment:
//! * [`FixtureTransport`] — offline, deterministic; serves canned JSON keyed by
//!   a **secret-stripped** request signature. Used by tests and the demo so CI
//!   stays offline.
//! * `ReqwestTransport` — real HTTP for native builds (CLI), behind the
//!   `live-fetch` feature.
//! * `FetchTransport` — real HTTP inside a Cloudflare Worker (`worker::Fetch`),
//!   behind the `worker-fetch` feature. `reqwest` cannot run on
//!   `wasm32-unknown-unknown` in workerd, which is why this exists at all.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;

/// Query-parameter names that carry API secrets. They are stripped from the
/// fixture request signature so canned fixtures never embed a key and still
/// match regardless of which (or whether a) key is supplied.
pub const SECRET_PARAMS: &[&str] = &["key", "api_key", "apikey"];

/// Anything that can fetch JSON from a hub. Adapters capture one of these at
/// construction (`Arc<dyn HttpTransport>`), since `Resolver::harvest(&self)`
/// takes no transport parameter.
///
/// **Why `?Send`.** Same rationale as [`crate::store::GraphStore`]: the Worker
/// implementation (`FetchTransport`) is built on `worker::Fetch`, whose futures
/// hold `JsValue`s and are therefore `!Send`. Workers are single-threaded, so a
/// `Send` bound would buy nothing and cost `SendWrapper` gymnastics at every call;
/// the CLI drives this with a current-thread runtime.
#[async_trait(?Send)]
pub trait HttpTransport {
    /// HTTP GET returning parsed JSON.
    async fn get_json(&self, url: &str) -> Result<Value>;

    /// HTTP GET with request headers (e.g. `X-Goog-Api-Key` + `X-Goog-FieldMask`
    /// for Google Places API New). Defaults to a plain GET — offline fixtures
    /// match on the URL and ignore headers, so they need no override.
    async fn get_json_with_headers(&self, url: &str, _headers: &[(&str, &str)]) -> Result<Value> {
        self.get_json(url).await
    }

    /// HTTP POST with headers and a JSON body, returning parsed JSON.
    async fn post_json(&self, url: &str, headers: &[(&str, &str)], body: &Value) -> Result<Value>;
}

/// Rewrite every [`SECRET_PARAMS`] value in a URL's query string to `REDACTED`,
/// preserving everything else verbatim.
///
/// **Load-bearing, not cosmetic.** Hub error messages embed the request URL
/// (`GET {url}: authentication/authorization denied …`), and TMDb carries its key
/// as a `?api_key=` query parameter (see `hubs::tmdb_search::url`). Those messages
/// are now reported to the caller as `ResolveOutput::hub_error` and travel all the
/// way out to an MCP error envelope — i.e. to an agent, and to whoever is reading
/// its output. Without this, the first TMDb 401 we ever diagnosed would have
/// printed our TMDb key to a user.
///
/// Applied at the point the error string is *built* (both live transports) rather
/// than at the point it is displayed, so a future caller that logs a hub error
/// cannot reintroduce the leak by forgetting to redact.
///
/// Google Places and Placekey pass their keys as headers, and headers are never
/// interpolated into a message — this covers the one kind of key that is in a URL.
pub fn redact_url(url: &str) -> String {
    let Some((base, query)) = url.split_once('?') else {
        return url.to_string();
    };
    let redacted = query
        .split('&')
        .map(|kv| match kv.split_once('=') {
            Some((k, _)) if SECRET_PARAMS.contains(&k) => format!("{k}=REDACTED"),
            _ => kv.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&");
    format!("{base}?{redacted}")
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

#[async_trait(?Send)]
impl HttpTransport for FixtureTransport {
    async fn get_json(&self, url: &str) -> Result<Value> {
        self.lookup("GET", url)
    }

    async fn post_json(
        &self,
        url: &str,
        _headers: &[(&str, &str)],
        _body: &Value,
    ) -> Result<Value> {
        self.lookup("POST", url)
    }
}

/// Descriptive `User-Agent`, mandatory for Wikidata SPARQL (403 without it).
/// `$transport` names which of the two live transports sent the request, so a hub
/// operator (and our own logs) can tell a CLI call from a Worker call.
#[cfg(any(feature = "live-fetch", feature = "worker-fetch"))]
macro_rules! user_agent {
    ($transport:literal) => {
        concat!(
            "sameas/",
            env!("CARGO_PKG_VERSION"),
            " (https://example.com/sameas) ",
            $transport
        )
    };
}

/// Real HTTP transport for native builds (opt-in). A single reused async client
/// carries a descriptive `user_agent!`, sensible timeouts, gzip, and a JSON
/// `Accept`. Calls
/// retry transient failures (timeouts, 429, 5xx) and map non-2xx to a clear,
/// class-named error.
#[cfg(feature = "live-fetch")]
pub struct ReqwestTransport {
    client: reqwest::Client,
    max_attempts: u32,
}

#[cfg(feature = "live-fetch")]
impl ReqwestTransport {
    pub fn new() -> Result<Self> {
        use reqwest::header::{HeaderMap, HeaderValue, ACCEPT};
        use std::time::Duration;

        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));

        let client = reqwest::Client::builder()
            // Descriptive UA per Wikimedia's User-Agent policy.
            .user_agent(user_agent!("reqwest"))
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
    ///
    /// The backoff is `tokio::time::sleep`, not `std::thread::sleep`: blocking the
    /// thread inside an async runtime stalls every other task on it. That is what
    /// makes `tokio` a runtime (not dev-only) dependency of this crate — but only
    /// under `live-fetch`, so the Worker build never pulls it in.
    async fn send_with_retry(
        &self,
        build: impl Fn() -> reqwest::RequestBuilder,
    ) -> reqwest::Result<reqwest::Response> {
        use std::time::Duration;
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            match build().send().await {
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
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    return Ok(resp);
                }
                Err(e) if attempt < self.max_attempts && (e.is_timeout() || e.is_connect()) => {
                    tokio::time::sleep(Duration::from_millis(200 * 2u64.pow(attempt))).await;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Read a response as JSON, mapping a non-2xx to an `anyhow` error that names
    /// the class (auth / not-found / rate-limited / other) with a body snippet —
    /// so callers can tell "bad key" from "no data".
    async fn read_json(resp: reqwest::Response, what: &str) -> Result<Value> {
        let status = resp.status();
        if status.is_success() {
            return resp
                .json::<Value>()
                .await
                .map_err(|e| anyhow!("{what}: decoding response body: {e}"));
        }
        let body = resp.text().await.unwrap_or_default();
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
#[async_trait(?Send)]
impl HttpTransport for ReqwestTransport {
    async fn get_json(&self, url: &str) -> Result<Value> {
        self.get_json_with_headers(url, &[]).await
    }

    async fn get_json_with_headers(&self, url: &str, headers: &[(&str, &str)]) -> Result<Value> {
        let resp = self
            .send_with_retry(|| {
                let mut req = self.client.get(url);
                for (k, v) in headers {
                    req = req.header(*k, *v);
                }
                req
            })
            .await
            .map_err(|e| anyhow!("GET {}: {e}", redact_url(url)))?;
        Self::read_json(resp, &format!("GET {}", redact_url(url))).await
    }

    async fn post_json(&self, url: &str, headers: &[(&str, &str)], body: &Value) -> Result<Value> {
        let resp = self
            .send_with_retry(|| {
                let mut req = self.client.post(url).json(body);
                for (k, v) in headers {
                    req = req.header(*k, *v);
                }
                req
            })
            .await
            .map_err(|e| anyhow!("POST {}: {e}", redact_url(url)))?;
        Self::read_json(resp, &format!("POST {}", redact_url(url))).await
    }
}

// ---------------------------------------------------------------------------
// FetchTransport — the Cloudflare Worker transport
// ---------------------------------------------------------------------------

/// Real HTTP transport inside a Cloudflare Worker, behind the `worker-fetch`
/// feature.
///
/// `reqwest` is not an option here: even its wasm32 backend targets a browser's
/// `window.fetch`, and the blocking client this crate used before U1 cannot exist
/// in workerd at all. `worker::Fetch` is the platform primitive, and its futures
/// are `!Send` — which is exactly why [`HttpTransport`] is `#[async_trait(?Send)]`.
///
/// Retry policy is deliberately identical to [`ReqwestTransport`]'s (bounded at
/// `max_attempts`, transient = 429/5xx/send-error, `Retry-After` honored, else
/// exponential backoff) with `worker::Delay` standing in for `tokio::time::sleep`.
/// Keeping the two in step means a hub that flakes behaves the same on the CLI and
/// in the Worker — the alternative (no retry in the Worker) would have made
/// Worker-only failures unreproducible locally.
///
/// Caveat inherited from the shared policy: a subrequest still counts against the
/// Worker's per-request subrequest limit on every attempt, and `Delay` burns wall
/// clock inside the request. Three attempts with 400ms/800ms backoff is the
/// bounded worst case.
#[cfg(feature = "worker-fetch")]
pub struct FetchTransport {
    max_attempts: u32,
}

#[cfg(feature = "worker-fetch")]
impl Default for FetchTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "worker-fetch")]
impl FetchTransport {
    /// Three attempts, matching `ReqwestTransport::new`.
    pub fn new() -> Self {
        FetchTransport { max_attempts: 3 }
    }

    /// Override the attempt cap. `1` disables retry entirely.
    pub fn with_max_attempts(max_attempts: u32) -> Self {
        FetchTransport {
            max_attempts: max_attempts.max(1),
        }
    }

    /// Build one `worker::Request`. Rebuilt per attempt: a `Request` body is a
    /// one-shot stream, so a retry cannot reuse the previous instance.
    fn build(
        method: worker::Method,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Result<worker::Request> {
        use wasm_bindgen::JsValue;

        let mut h = worker::Headers::new();
        let safe = redact_url(url);
        let mut set = |k: &str, v: &str| -> Result<()> {
            h.set(k, v)
                .map_err(|e| anyhow!("building request for {safe}: header {k}: {e}"))
        };
        set("Accept", "application/json")?;
        set("User-Agent", user_agent!("workers-rs"))?;
        let serialized = match body {
            Some(v) => {
                set("Content-Type", "application/json")?;
                Some(
                    serde_json::to_string(v)
                        .map_err(|e| anyhow!("building request for {safe}: serializing body: {e}"))?,
                )
            }
            None => None,
        };
        // Caller headers last so an explicit override (e.g. Placekey's own
        // Content-Type) wins over the defaults above.
        for (k, v) in headers {
            set(k, v)?;
        }

        let mut init = worker::RequestInit::new();
        init.with_method(method).with_headers(h);
        if let Some(s) = serialized {
            init.with_body(Some(JsValue::from_str(&s)));
        }
        worker::Request::new_with_init(url, &init)
            .map_err(|e| anyhow!("building request for {safe}: {e}"))
    }

    async fn send(
        &self,
        method: worker::Method,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Result<Value> {
        use std::time::Duration;

        // Redacted: `what` is the prefix of every error this call can produce, and
        // those errors are surfaced to the caller as `ResolveOutput::hub_error`.
        let what = format!("{method} {}", redact_url(url));
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let req = Self::build(method.clone(), url, headers, body)?;
            match worker::Fetch::Request(req).send().await {
                Ok(mut resp) => {
                    let status = resp.status_code();
                    let transient = status == 429 || (500..600).contains(&status);
                    if transient && attempt < self.max_attempts {
                        let wait = resp
                            .headers()
                            .get("retry-after")
                            .ok()
                            .flatten()
                            .and_then(|s| s.parse::<u64>().ok())
                            .map(Duration::from_secs)
                            .unwrap_or_else(|| Duration::from_millis(200 * 2u64.pow(attempt)));
                        worker::Delay::from(wait).await;
                        continue;
                    }
                    return Self::read_json(&mut resp, &what).await;
                }
                // `worker::Fetch` does not classify errors (no `is_timeout` /
                // `is_connect` the way reqwest has), so every send failure is
                // treated as transient. A malformed URL fails in `build` above and
                // never reaches here.
                Err(_) if attempt < self.max_attempts => {
                    worker::Delay::from(Duration::from_millis(200 * 2u64.pow(attempt))).await;
                    continue;
                }
                Err(e) => return Err(anyhow!("{what}: {e}")),
            }
        }
    }

    /// Mirrors `ReqwestTransport::read_json`'s error classes so a hub failure
    /// reads identically whichever transport produced it.
    async fn read_json(resp: &mut worker::Response, what: &str) -> Result<Value> {
        let status = resp.status_code();
        let body = resp
            .text()
            .await
            .map_err(|e| anyhow!("{what}: reading response body: {e}"))?;
        if (200..300).contains(&status) {
            return serde_json::from_str(&body)
                .map_err(|e| anyhow!("{what}: decoding response body: {e}"));
        }
        let snippet: String = body.chars().take(300).collect();
        match status {
            401 | 403 => Err(anyhow!(
                "{what}: authentication/authorization denied (HTTP {status}): {snippet}"
            )),
            404 => Err(anyhow!("{what}: not found (HTTP 404): {snippet}")),
            429 => Err(anyhow!("{what}: rate limited (HTTP 429): {snippet}")),
            _ => Err(anyhow!("{what}: unexpected HTTP {status}: {snippet}")),
        }
    }
}

#[cfg(feature = "worker-fetch")]
#[async_trait(?Send)]
impl HttpTransport for FetchTransport {
    async fn get_json(&self, url: &str) -> Result<Value> {
        self.get_json_with_headers(url, &[]).await
    }

    async fn get_json_with_headers(&self, url: &str, headers: &[(&str, &str)]) -> Result<Value> {
        self.send(worker::Method::Get, url, headers, None).await
    }

    async fn post_json(&self, url: &str, headers: &[(&str, &str)], body: &Value) -> Result<Value> {
        self.send(worker::Method::Post, url, headers, Some(body))
            .await
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
    fn redact_url_keeps_the_diagnostic_and_drops_the_key() {
        // TMDb is the one hub with its key in the URL, so this is the string that
        // would otherwise reach an agent inside `hub_error`.
        let out = redact_url(
            "https://api.themoviedb.org/3/search/multi?query=Avatar&include_adult=false&api_key=sk-live-123",
        );
        assert!(!out.contains("sk-live-123"), "the key must not survive: {out}");
        assert!(out.contains("api_key=REDACTED"));
        // Everything that makes the message useful is preserved verbatim.
        assert!(out.contains("query=Avatar"));
        assert!(out.contains("include_adult=false"));
        assert!(out.starts_with("https://api.themoviedb.org/3/search/multi?"));
    }

    #[test]
    fn redact_url_is_total_on_urls_with_no_query() {
        // Google Places posts to a bare path and carries its key in a header.
        let url = "https://places.googleapis.com/v1/places:searchText";
        assert_eq!(redact_url(url), url);
        assert_eq!(redact_url(""), "");
    }

    #[test]
    fn redact_url_covers_every_secret_param_name() {
        for p in SECRET_PARAMS {
            let out = redact_url(&format!("https://h/x?{p}=SECRET"));
            assert!(!out.contains("SECRET"), "{p} leaked: {out}");
        }
    }

    #[tokio::test]
    async fn fixture_transport_serves_by_signature() {
        let t = FixtureTransport::from_pairs(vec![(
            "GET",
            "https://query.wikidata.org/sparql?format=json&query=abc",
            json!({"ok": true}),
        )]);
        // Different key/order still matches the fixture.
        let got = t
            .get_json("https://query.wikidata.org/sparql?query=abc&format=json")
            .await
            .unwrap();
        assert_eq!(got, json!({"ok": true}));
        assert!(t
            .get_json("https://query.wikidata.org/sparql?query=zzz")
            .await
            .is_err());
    }
}
