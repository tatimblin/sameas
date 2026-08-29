//! TMDb `/search/multi` — resolve a **title** to candidate films / TV series.
//!
//! This is the movie/tvSeries entry point for a name query, the counterpart to
//! [`super::places::PlaceTextSearchResolver`] for places. Two differences drive
//! the design:
//!
//! * **Free.** TMDb search costs nothing, so an ambiguous title is answered from
//!   one request — no per-candidate fan-out (which is what Google Places needs).
//! * **Self-describing.** Every result carries its title, its year, and whether
//!   it is a film or a series, which is exactly what tells `Avatar` (2009 film)
//!   from `Avatar: The Way of Water` (2022, same franchise) from
//!   `Avatar: The Last Airbender` (TV, unrelated franchise).
//!
//! **Why TV candidates are keyed as `url:` and not `tmdb:`.** TMDb numbers films
//! and series in *separate* namespaces — movie 246 and TV 246 are different
//! things — but the `tmdb` kind is movie-scoped (its crosswalk is
//! `/movie/{id}/external_ids`, see [`super::tmdb::TmdbResolver`]). Minting
//! `tmdb:246` for a series would therefore collide with a film and crosswalk to
//! that film's ids: a false merge, the one class of error this project treats as
//! its primary invariant. Until the registry grows a `tmdb_tv` kind, a series is
//! keyed by its canonical TMDb URL through the generic `url` kind — path-bearing,
//! `Grain::Identity`, and never speculatively crosswalked.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use super::{push_id, HubCandidate};
use crate::model::{EntityRecord, ExternalId};
use crate::resolve::Resolver;
use crate::transport::HttpTransport;

const BASE: &str = "https://api.themoviedb.org/3";

pub struct TmdbSearchResolver {
    query: String,
    api_key: String,
    transport: Arc<dyn HttpTransport>,
}

impl TmdbSearchResolver {
    pub fn new(
        query: impl Into<String>,
        api_key: String,
        transport: Arc<dyn HttpTransport>,
    ) -> TmdbSearchResolver {
        TmdbSearchResolver {
            query: query.into(),
            api_key,
            transport,
        }
    }

    pub(crate) fn url(&self) -> String {
        let q: String =
            url::form_urlencoded::byte_serialize(self.query.trim().as_bytes()).collect();
        // `api_key` is stripped from the fixture request signature (SECRET_PARAMS),
        // so offline fixtures match with or without a key.
        format!(
            "{BASE}/search/multi?query={q}&include_adult=false&api_key={}",
            self.api_key
        )
    }

    /// Run the search and return every candidate, in the hub's own rank order.
    pub async fn candidates(&self) -> Result<Vec<HubCandidate>> {
        let value = self.transport.get_json(&self.url()).await?;
        Ok(Self::parse(&value))
    }

    /// Parse a `/search/multi` response into candidates, **preserving TMDb's
    /// rank order** (its relevance ordering is better than anything we could
    /// recompute from a title string).
    ///
    /// `person` results are dropped: a name query typed by a reviewer names a
    /// work, and a person is not a work. Anything with no usable id is dropped
    /// too — a candidate we cannot key is not choosable.
    pub fn parse(value: &Value) -> Vec<HubCandidate> {
        let results = match value.get("results").and_then(|r| r.as_array()) {
            Some(r) => r,
            None => return Vec::new(),
        };
        results.iter().filter_map(Self::parse_one).collect()
    }

    fn parse_one(item: &Value) -> Option<HubCandidate> {
        let media_type = item
            .get("media_type")
            .and_then(|m| m.as_str())
            .unwrap_or("");
        let id = item.get("id").and_then(|i| {
            i.as_u64()
                .map(|n| n.to_string())
                .or_else(|| i.as_str().map(str::to_string))
        })?;
        let title = |field: &str, fallback: &str| -> Option<String> {
            item.get(field)
                .and_then(|t| t.as_str())
                .or_else(|| item.get(fallback).and_then(|t| t.as_str()))
                .map(|s| s.to_string())
        };
        let (id, name, date_field, kind_word) = match media_type {
            "movie" => (
                ExternalId::new("tmdb", &id).ok()?,
                title("title", "name"),
                "release_date",
                "film",
            ),
            // See the module header: a series id is NOT a `tmdb:` key.
            "tv" => (
                ExternalId::new("url", &format!("https://www.themoviedb.org/tv/{id}")).ok()?,
                title("name", "title"),
                "first_air_date",
                "TV series",
            ),
            _ => return None,
        };
        let year = item
            .get(date_field)
            .and_then(|d| d.as_str())
            .map(|d| d.chars().take(4).collect::<String>())
            .filter(|y| y.len() == 4 && y.chars().all(|c| c.is_ascii_digit()));
        let detail = match year {
            Some(y) => format!("{y} {kind_word}"),
            None => kind_word.to_string(),
        };
        Some(HubCandidate::new(id, name, Some(detail)))
    }
}

#[async_trait(?Send)]
impl Resolver for TmdbSearchResolver {
    /// The top-ranked hit as a record, so a search composes with the completion
    /// BFS the same way the other adapters do. The ambiguity question is answered
    /// by [`Self::candidates`]; this deliberately keeps only the best match.
    async fn harvest(&self) -> Result<EntityRecord> {
        let mut record = EntityRecord::default();
        if let Some(top) = self.candidates().await?.into_iter().next() {
            push_id(&mut record, top.id.kind_tag(), top.id.value());
            record.name = top.name;
        }
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::FixtureTransport;
    use serde_json::json;

    /// The two ambiguity shapes in one response: an unrelated franchise colliding
    /// on the name (the cartoon) and two works inside one franchise separated only
    /// by year — plus a person, which must be dropped.
    pub(crate) fn avatar_multi() -> Value {
        json!({ "results": [
            { "id": 19995, "media_type": "movie", "title": "Avatar",
              "release_date": "2009-12-15" },
            { "id": 76600, "media_type": "movie", "title": "Avatar: The Way of Water",
              "release_date": "2022-12-14" },
            { "id": 246, "media_type": "tv", "name": "Avatar: The Last Airbender",
              "first_air_date": "2005-02-21" },
            { "id": 8888, "media_type": "person", "name": "James Cameron" }
        ]})
    }

    #[test]
    fn parses_films_and_series_and_drops_people() {
        let c = TmdbSearchResolver::parse(&avatar_multi());
        assert_eq!(c.len(), 3, "the person result must be dropped");
        // Hub rank order preserved.
        assert_eq!(c[0].id.key(), "tmdb:19995");
        assert_eq!(c[1].id.key(), "tmdb:76600");
        // A series is keyed by URL, never by a movie-namespace `tmdb:` id.
        assert_eq!(c[2].id.key(), "url:themoviedb.org/tv/246");
    }

    #[test]
    fn labels_carry_the_year() {
        let c = TmdbSearchResolver::parse(&avatar_multi());
        assert_eq!(c[0].label().as_deref(), Some("Avatar (2009 film)"));
        assert_eq!(
            c[1].label().as_deref(),
            Some("Avatar: The Way of Water (2022 film)")
        );
        assert_eq!(
            c[2].label().as_deref(),
            Some("Avatar: The Last Airbender (2005 TV series)")
        );
    }

    #[test]
    fn a_missing_date_still_yields_a_usable_label() {
        let c = TmdbSearchResolver::parse(&json!({ "results": [
            { "id": 5, "media_type": "movie", "title": "Untitled" }
        ]}));
        assert_eq!(c[0].label().as_deref(), Some("Untitled (film)"));
    }

    #[test]
    fn an_empty_or_shapeless_response_is_no_candidates() {
        assert!(TmdbSearchResolver::parse(&json!({})).is_empty());
        assert!(TmdbSearchResolver::parse(&json!({ "results": [] })).is_empty());
    }

    #[tokio::test]
    async fn candidates_reads_the_fixture_url() {
        let probe = TmdbSearchResolver::new(
            "Avatar",
            String::new(),
            Arc::new(FixtureTransport::from_pairs(vec![])),
        );
        let url = probe.url();
        let transport = FixtureTransport::from_pairs(vec![("GET", &url, avatar_multi())]);
        let r = TmdbSearchResolver::new("Avatar", "KEY".into(), Arc::new(transport));
        // The api key is stripped from the fixture signature, so a keyed request
        // still matches the key-less fixture.
        let c = r.candidates().await.unwrap();
        assert_eq!(c.len(), 3);
    }

    #[tokio::test]
    async fn harvest_returns_the_top_hit_only() {
        let probe = TmdbSearchResolver::new(
            "Avatar",
            String::new(),
            Arc::new(FixtureTransport::from_pairs(vec![])),
        );
        let url = probe.url();
        let transport = FixtureTransport::from_pairs(vec![("GET", &url, avatar_multi())]);
        let rec = TmdbSearchResolver::new("Avatar", String::new(), Arc::new(transport))
            .harvest()
            .await
            .unwrap();
        let keys: Vec<String> = rec.same_as.iter().map(|i| i.key()).collect();
        assert_eq!(keys, vec!["tmdb:19995".to_string()]);
        assert_eq!(rec.name.as_deref(), Some("Avatar"));
    }
}
