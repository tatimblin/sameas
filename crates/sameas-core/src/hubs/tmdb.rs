//! TMDb Find-by-ID adapter — crosswalk `imdb_id ↔ tmdb_id ↔ wikidata_id`.
//!
//! * imdb → tmdb: `GET /3/find/{imdb}?external_source=imdb_id` → `movie_results[0].id`.
//! * tmdb → ids:  `GET /3/movie/{tmdb}/external_ids` → `imdb_id`, `wikidata_id`.

use std::sync::Arc;

use anyhow::{bail, Result};
use async_trait::async_trait;
use serde_json::Value;

use super::push_id;
use crate::model::{EntityRecord, ExternalId};
use crate::resolve::Resolver;
use crate::transport::HttpTransport;

const BASE: &str = "https://api.themoviedb.org/3";

pub struct TmdbResolver {
    input: ExternalId,
    api_key: String,
    transport: Arc<dyn HttpTransport>,
}

impl TmdbResolver {
    pub fn new(input: ExternalId, api_key: String, transport: Arc<dyn HttpTransport>) -> Self {
        TmdbResolver {
            input,
            api_key,
            transport,
        }
    }

    fn find_url(&self, imdb: &str) -> String {
        format!("{BASE}/find/{imdb}?external_source=imdb_id&api_key={}", self.api_key)
    }

    fn external_ids_url(&self, tmdb: &str) -> String {
        format!("{BASE}/movie/{tmdb}/external_ids?api_key={}", self.api_key)
    }

    /// Extract the TMDb movie id from a `/find` response (`movie_results[0].id`).
    pub fn parse_find(value: &Value) -> Option<String> {
        let id = value
            .get("movie_results")?
            .as_array()?
            .first()?
            .get("id")?;
        // TMDb ids are JSON numbers.
        id.as_u64().map(|n| n.to_string()).or_else(|| id.as_str().map(|s| s.to_string()))
    }

    /// Parse a `/movie/{id}/external_ids` response into a record (imdb + wikidata).
    /// `tmdb_id` is threaded in because that endpoint does not echo it.
    pub fn parse_external_ids(value: &Value, tmdb_id: &str) -> Result<EntityRecord> {
        let mut record = EntityRecord::default();
        push_id(&mut record, "tmdb", tmdb_id);
        if let Some(imdb) = value.get("imdb_id").and_then(|v| v.as_str()) {
            push_id(&mut record, "imdb", imdb);
        }
        if let Some(qid) = value.get("wikidata_id").and_then(|v| v.as_str()) {
            push_id(&mut record, "wikidata", qid);
        }
        Ok(record)
    }
}

#[async_trait(?Send)]
impl Resolver for TmdbResolver {
    async fn harvest(&self) -> Result<EntityRecord> {
        let mut record = EntityRecord::default();
        // Echo the input id up front.
        push_id(&mut record, self.input.kind_tag(), self.input.value());

        // Resolve to a TMDb movie id.
        let tmdb_id = match self.input.kind_tag() {
            "imdb" => {
                let v = self
                    .transport
                    .get_json(&self.find_url(self.input.value()))
                    .await?;
                // Capture the title + type for display (light metadata, like the
                // name Place Details returns for a business).
                if let Some(first) = v
                    .get("movie_results")
                    .and_then(|a| a.as_array())
                    .and_then(|a| a.first())
                {
                    if record.name.is_none() {
                        if let Some(title) = first.get("title").and_then(|t| t.as_str()) {
                            record.name = Some(title.to_string());
                        }
                    }
                    record.entity_type.get_or_insert_with(|| "Movie".to_string());
                }
                match Self::parse_find(&v) {
                    Some(id) => id,
                    None => return Ok(record), // no TMDb match — return the echo only
                }
            }
            "tmdb" => self.input.value().to_string(),
            other => bail!("tmdb: unsupported input kind {other:?}"),
        };

        // Crosswalk out to imdb + wikidata via external_ids.
        let ext = self
            .transport
            .get_json(&self.external_ids_url(&tmdb_id))
            .await?;
        let ext_rec = Self::parse_external_ids(&ext, &tmdb_id)?;
        for id in ext_rec.same_as {
            if !record.same_as.iter().any(|e| e == &id) {
                record.same_as.push(id);
            }
        }
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_find_reads_movie_id() {
        let v = json!({ "movie_results": [{ "id": 603, "title": "The Matrix" }] });
        assert_eq!(TmdbResolver::parse_find(&v).as_deref(), Some("603"));
        assert_eq!(TmdbResolver::parse_find(&json!({"movie_results": []})), None);
    }

    #[test]
    fn parse_external_ids_crosslinks() {
        let v = json!({ "imdb_id": "tt0133093", "wikidata_id": "Q83495" });
        let rec = TmdbResolver::parse_external_ids(&v, "603").unwrap();
        let keys: Vec<String> = rec.same_as.iter().map(|i| i.key()).collect();
        assert!(keys.contains(&"tmdb:603".to_string()));
        assert!(keys.contains(&"imdb:tt0133093".to_string()));
        assert!(keys.contains(&"wikidata:Q83495".to_string()));
    }

    #[tokio::test]
    async fn harvest_chains_find_then_external_ids() {
        let input = ExternalId::imdb("tt0133093").unwrap();
        let probe = TmdbResolver::new(
            input,
            "KEY".into(),
            Arc::new(crate::transport::FixtureTransport::from_pairs(vec![])),
        );
        let find = probe.find_url("tt0133093");
        let ext = probe.external_ids_url("603");
        let transport = crate::transport::FixtureTransport::from_pairs(vec![
            ("GET", &find, json!({ "movie_results": [{ "id": 603, "title": "The Matrix" }] })),
            ("GET", &ext, json!({ "imdb_id": "tt0133093", "wikidata_id": "Q83495" })),
        ]);
        let r = TmdbResolver::new(
            ExternalId::imdb("tt0133093").unwrap(),
            "KEY".into(),
            Arc::new(transport),
        );
        let rec = r.harvest().await.unwrap();
        let keys: Vec<String> = rec.same_as.iter().map(|i| i.key()).collect();
        assert!(keys.contains(&"tmdb:603".to_string()));
        assert!(keys.contains(&"wikidata:Q83495".to_string()));
        assert_eq!(keys.iter().filter(|k| *k == "imdb:tt0133093").count(), 1);
        // Title + type harvested for display.
        assert_eq!(rec.name.as_deref(), Some("The Matrix"));
        assert_eq!(rec.entity_type.as_deref(), Some("Movie"));
    }
}
