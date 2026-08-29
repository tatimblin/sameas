//! Wikidata adapter — resolve an IMDb id / website / QID to a Wikidata item and
//! harvest its cross-identifiers via SPARQL:
//! P345 (IMDb), P856 (official website), P1329 (phone), P4947 (TMDb movie).
//!
//! One query returns the QID plus all four properties, so an IMDb id completes
//! to QID + website + TMDb in a single hop (M2 exit criterion 1).

use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use serde_json::Value;

use super::{binding_value, push_id};
use crate::model::{EntityRecord, ExternalId};
use crate::resolve::Resolver;
use crate::transport::HttpTransport;

const ENDPOINT: &str = "https://query.wikidata.org/sparql";

pub struct WikidataResolver {
    input: ExternalId,
    transport: Arc<dyn HttpTransport>,
}

impl WikidataResolver {
    pub fn new(input: ExternalId, transport: Arc<dyn HttpTransport>) -> Self {
        WikidataResolver { input, transport }
    }

    /// Build the SPARQL query for this input kind.
    fn query(&self) -> Result<String> {
        // Match ?item from the input; then OPTIONAL-harvest the four properties.
        let selector = match self.input.kind_tag() {
            // String literal match on the IMDb id (P345).
            "imdb" => format!("?item wdt:P345 {:?}.", self.input.value()),
            // Bind the item directly from its QID.
            "wikidata" => format!("VALUES ?item {{ wd:{} }}", self.input.value()),
            // Website is stored as a full URL; match by registrable-domain
            // substring (lossy — flagged).
            "domain" => format!(
                "?item wdt:P856 ?anysite. FILTER(CONTAINS(LCASE(STR(?anysite)), {:?}))",
                self.input.value()
            ),
            other => bail!("wikidata: unsupported input kind {other:?}"),
        };
        Ok(format!(
            "SELECT ?item ?imdb ?website ?phone ?tmdb WHERE {{ \
               {selector} \
               OPTIONAL {{ ?item wdt:P345 ?imdb. }} \
               OPTIONAL {{ ?item wdt:P856 ?website. }} \
               OPTIONAL {{ ?item wdt:P1329 ?phone. }} \
               OPTIONAL {{ ?item wdt:P4947 ?tmdb. }} \
             }}"
        ))
    }

    pub(crate) fn url(&self) -> Result<String> {
        let q = self.query()?;
        let encoded: String = url::form_urlencoded::byte_serialize(q.as_bytes()).collect();
        Ok(format!("{ENDPOINT}?format=json&query={encoded}"))
    }

    /// Parse a Wikidata SPARQL JSON result into a record. OPTIONAL joins can
    /// produce a cartesian product of bindings, so `push_id` de-duplicates.
    pub fn parse(value: &Value) -> Result<EntityRecord> {
        let bindings = value
            .get("results")
            .and_then(|r| r.get("bindings"))
            .and_then(|b| b.as_array())
            .ok_or_else(|| anyhow!("wikidata: missing results.bindings"))?;

        let mut record = EntityRecord::default();
        for b in bindings {
            if let Some(item) = binding_value(b, "item") {
                // e.g. http://www.wikidata.org/entity/Q83495 → Q83495
                push_id(&mut record, "wikidata", item);
            }
            if let Some(v) = binding_value(b, "imdb") {
                push_id(&mut record, "imdb", v);
            }
            if let Some(v) = binding_value(b, "website") {
                push_id(&mut record, "domain", v);
            }
            if let Some(v) = binding_value(b, "phone") {
                push_id(&mut record, "phone", v);
            }
            if let Some(v) = binding_value(b, "tmdb") {
                push_id(&mut record, "tmdb", v);
            }
        }
        Ok(record)
    }
}

#[async_trait(?Send)]
impl Resolver for WikidataResolver {
    async fn harvest(&self) -> Result<EntityRecord> {
        let value = self.transport.get_json(&self.url()?).await?;
        let mut record = Self::parse(&value)?;
        // Echo the input id so the harvested record joins the input's cluster.
        push_id(&mut record, self.input.kind_tag(), self.input.value());
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn matrix_sparql() -> Value {
        // Shape of a real Wikidata SPARQL JSON response (trimmed).
        json!({
          "results": { "bindings": [
            {
              "item":    { "type": "uri",     "value": "http://www.wikidata.org/entity/Q83495" },
              "imdb":    { "type": "literal",  "value": "tt0133093" },
              "website": { "type": "literal",  "value": "https://www.warnerbros.com/movies/matrix" },
              "tmdb":    { "type": "literal",  "value": "603" }
            }
          ]}
        })
    }

    #[test]
    fn parses_qid_and_crosslinks() {
        let rec = WikidataResolver::parse(&matrix_sparql()).unwrap();
        let keys: Vec<String> = rec.same_as.iter().map(|i| i.key()).collect();
        assert!(keys.contains(&"wikidata:Q83495".to_string()));
        assert!(keys.contains(&"imdb:tt0133093".to_string()));
        assert!(keys.contains(&"tmdb:603".to_string()));
        assert!(keys.contains(&"domain:warnerbros.com".to_string()));
    }

    #[tokio::test]
    async fn harvest_echoes_input_and_dedupes() {
        // Register the fixture under the exact URL the resolver builds (the
        // encoded SPARQL query is part of the request signature).
        let probe = WikidataResolver::new(
            ExternalId::imdb("tt0133093").unwrap(),
            Arc::new(crate::transport::FixtureTransport::from_pairs(vec![])),
        );
        let url = probe.url().unwrap();
        let transport =
            crate::transport::FixtureTransport::from_pairs(vec![("GET", &url, matrix_sparql())]);
        let r = WikidataResolver::new(ExternalId::imdb("tt0133093").unwrap(), Arc::new(transport));

        let rec = r.harvest().await.unwrap();
        let keys: Vec<String> = rec.same_as.iter().map(|i| i.key()).collect();
        // imdb echoed exactly once (no duplicate with the P345 binding).
        assert_eq!(keys.iter().filter(|k| *k == "imdb:tt0133093").count(), 1);
        assert!(keys.contains(&"wikidata:Q83495".to_string()));
    }
}
