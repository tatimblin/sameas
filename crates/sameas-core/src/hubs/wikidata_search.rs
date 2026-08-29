//! Wikidata `wbsearchentities` — resolve a **name** to candidate items.
//!
//! The type-agnostic fallback of the name-search router: whatever a caller asks
//! about — a park, a band, a book, a type sameas has never heard of — Wikidata
//! has an item and a one-line description for it. Free, and self-describing:
//! `label` + `description` ("2009 film by James Cameron") is exactly the
//! disambiguator a human needs to pick, so no per-candidate fan-out is required.
//!
//! Distinct from [`super::wikidata::WikidataResolver`], which is the *forward*
//! adapter: given an id, SPARQL out to the crosslinks. This one turns a string
//! into candidate QIDs and stops there — the crosswalk happens later, once one
//! candidate has been chosen and committed.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use super::{push_id, HubCandidate};
use crate::model::{EntityRecord, ExternalId};
use crate::resolve::Resolver;
use crate::transport::HttpTransport;

const ENDPOINT: &str = "https://www.wikidata.org/w/api.php";
/// Ask for more than the candidate cap so the *stored* cardinality reflects the
/// hub's real answer rather than our display budget.
const LIMIT: usize = 20;

pub struct WikidataSearchResolver {
    query: String,
    transport: Arc<dyn HttpTransport>,
}

impl WikidataSearchResolver {
    pub fn new(
        query: impl Into<String>,
        transport: Arc<dyn HttpTransport>,
    ) -> WikidataSearchResolver {
        WikidataSearchResolver {
            query: query.into(),
            transport,
        }
    }

    pub(crate) fn url(&self) -> String {
        let q: String =
            url::form_urlencoded::byte_serialize(self.query.trim().as_bytes()).collect();
        format!(
            "{ENDPOINT}?action=wbsearchentities&search={q}&language=en&uselang=en&type=item&limit={LIMIT}&format=json"
        )
    }

    /// Run the search and return every candidate, in Wikidata's rank order.
    pub async fn candidates(&self) -> Result<Vec<HubCandidate>> {
        let value = self.transport.get_json(&self.url()).await?;
        Ok(Self::parse(&value))
    }

    /// Parse a `wbsearchentities` response, preserving hub rank order. An item
    /// whose QID does not normalize is dropped (a candidate we cannot key is not
    /// choosable); the `description` becomes the disambiguator.
    pub fn parse(value: &Value) -> Vec<HubCandidate> {
        let items = match value.get("search").and_then(|s| s.as_array()) {
            Some(s) => s,
            None => return Vec::new(),
        };
        items
            .iter()
            .filter_map(|item| {
                let qid = item.get("id").and_then(|i| i.as_str())?;
                let id = ExternalId::new("wikidata", qid).ok()?;
                let name = item
                    .get("label")
                    .and_then(|l| l.as_str())
                    .map(|s| s.to_string());
                let detail = item
                    .get("description")
                    .and_then(|d| d.as_str())
                    .filter(|d| !d.trim().is_empty())
                    .map(|s| s.to_string());
                Some(HubCandidate::new(id, name, detail))
            })
            .collect()
    }
}

#[async_trait(?Send)]
impl Resolver for WikidataSearchResolver {
    /// The top-ranked hit as a record — see [`super::tmdb_search`] for why a
    /// search adapter implements `Resolver` at all.
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

    pub(crate) fn avatar_search() -> Value {
        json!({ "searchinfo": { "search": "Avatar" }, "search": [
            { "id": "Q24871", "label": "Avatar",
              "description": "2009 film by James Cameron" },
            { "id": "Q104123", "label": "Avatar: The Last Airbender",
              "description": "American animated television series" },
            { "id": "Q99", "label": "Avatar", "description": "" },
            { "label": "no id here", "description": "dropped" }
        ]})
    }

    #[test]
    fn parses_labels_and_descriptions_in_rank_order() {
        let c = WikidataSearchResolver::parse(&avatar_search());
        assert_eq!(c.len(), 3, "the id-less row must be dropped");
        assert_eq!(c[0].id.key(), "wikidata:Q24871");
        assert_eq!(
            c[0].label().as_deref(),
            Some("Avatar (2009 film by James Cameron)")
        );
        assert_eq!(c[1].id.key(), "wikidata:Q104123");
        // An empty description leaves the bare label — still choosable, if barely.
        assert_eq!(c[2].label().as_deref(), Some("Avatar"));
    }

    #[test]
    fn an_empty_or_shapeless_response_is_no_candidates() {
        assert!(WikidataSearchResolver::parse(&json!({})).is_empty());
        assert!(WikidataSearchResolver::parse(&json!({ "search": [] })).is_empty());
    }

    #[tokio::test]
    async fn candidates_reads_the_fixture_url() {
        let probe =
            WikidataSearchResolver::new("Avatar", Arc::new(FixtureTransport::from_pairs(vec![])));
        let url = probe.url();
        let transport = FixtureTransport::from_pairs(vec![("GET", &url, avatar_search())]);
        let c = WikidataSearchResolver::new("Avatar", Arc::new(transport))
            .candidates()
            .await
            .unwrap();
        assert_eq!(c.len(), 3);
    }

    #[tokio::test]
    async fn harvest_returns_the_top_hit_only() {
        let probe =
            WikidataSearchResolver::new("Avatar", Arc::new(FixtureTransport::from_pairs(vec![])));
        let url = probe.url();
        let transport = FixtureTransport::from_pairs(vec![("GET", &url, avatar_search())]);
        let rec = WikidataSearchResolver::new("Avatar", Arc::new(transport))
            .harvest()
            .await
            .unwrap();
        let keys: Vec<String> = rec.same_as.iter().map(|i| i.key()).collect();
        assert_eq!(keys, vec!["wikidata:Q24871".to_string()]);
        assert_eq!(rec.name.as_deref(), Some("Avatar"));
    }
}
