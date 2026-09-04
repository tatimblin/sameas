//! Wikidata P856 **reverse** lookup — resolve a *website* to candidate items.
//!
//! The org counterpart of [`super::wikidata_search`]. A name search asks "what is
//! called this?"; this asks "who publishes this site?" — and for an organization
//! that is the stronger question, because a registrable domain is nearly unique
//! where a name is not (`Mercury` is a bank, a planet, an element and a record
//! label; `mercury.com` is one company).
//!
//! **Why this is not a weakening of the entity-grain rule.** A `domain` is
//! [`Grain::Affiliation`](crate::kind::Grain): it may name a chain, so it can
//! never *by itself* stand in for one of that chain's locations. This adapter does
//! not resolve on the domain — it asks an authority to hand back an **Identity**
//! key (a QID) for it, and identity is what the caller then resolves on. The grain
//! rule is upheld one level up, in [`crate::complete::resolve_by_website`], which
//! refuses to run this at all for place-shaped types and for a domain already
//! owned by an identity-bearing cluster.
//!
//! Like every search adapter it returns a LIST and stops there: two items sharing
//! a website (a company and its foundation) is an ambiguity for the caller to
//! resolve, never a merge.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use super::{binding_value, HubCandidate};
use crate::model::ExternalId;
use crate::transport::HttpTransport;

const ENDPOINT: &str = "https://query.wikidata.org/sparql";
/// Enough to prove "more than one" without unbounded work; a domain that names
/// more than a handful of items is ambiguous long before the cap.
const LIMIT: usize = 20;

pub struct WikidataWebsiteResolver {
    domain: String,
    transport: Arc<dyn HttpTransport>,
}

impl WikidataWebsiteResolver {
    /// `domain` is an already-**normalized** registrable domain (`uber.com`), i.e.
    /// the value of a `domain`-kind [`ExternalId`], never a raw URL.
    pub fn new(domain: impl Into<String>, transport: Arc<dyn HttpTransport>) -> Self {
        WikidataWebsiteResolver {
            domain: domain.into(),
            transport,
        }
    }

    fn query(&self) -> String {
        // `SERVICE wikibase:label` buys the label and description in the same
        // round trip, which is what makes each candidate *choosable* by a human
        // (see `HubCandidate`). Without it an ambiguous answer would be a list of
        // opaque QIDs, which is not an answer to "which one?".
        format!(
            "SELECT ?item ?itemLabel ?itemDescription WHERE {{ \
               {values} \
               ?item wdt:P856 ?anysite. \
               SERVICE wikibase:label {{ bd:serviceParam wikibase:language \"en\". }} \
             }} LIMIT {LIMIT}",
            values = super::wikidata::website_values(&self.domain),
        )
    }

    pub(crate) fn url(&self) -> String {
        let encoded: String = url::form_urlencoded::byte_serialize(self.query().as_bytes()).collect();
        format!("{ENDPOINT}?format=json&query={encoded}")
    }

    /// Every item that publishes this site, in the hub's order.
    pub async fn candidates(&self) -> Result<Vec<HubCandidate>> {
        let value = self.transport.get_json(&self.url()).await?;
        Ok(Self::parse(&value))
    }

    /// Parse a SPARQL JSON result into candidates.
    ///
    /// De-duplicated by QID: an item with several matching P856 spellings (the
    /// bare and `www.` forms are both recorded on plenty of items) produces one
    /// binding per spelling, and counting those as separate candidates would
    /// report a unique answer as ambiguous — turning a clean resolve into a
    /// refusal.
    pub fn parse(value: &Value) -> Vec<HubCandidate> {
        let bindings = match value
            .get("results")
            .and_then(|r| r.get("bindings"))
            .and_then(|b| b.as_array())
        {
            Some(b) => b,
            None => return Vec::new(),
        };
        let mut out: Vec<HubCandidate> = Vec::new();
        for b in bindings {
            let item = match binding_value(b, "item") {
                Some(i) => i,
                None => continue,
            };
            // `http://www.wikidata.org/entity/Q780442` → `Q780442`, via the
            // registry's own normalizer — a value it rejects is not choosable.
            let id = match ExternalId::new("wikidata", item) {
                Ok(id) => id,
                Err(_) => continue,
            };
            if out.iter().any(|c| c.id == id) {
                continue;
            }
            // The label service echoes the QID as the label when an item has no
            // English label; that is not a disambiguator, so drop it and let the
            // description (or the bare key) speak.
            let name = binding_value(b, "itemLabel")
                .filter(|l| *l != id.value())
                .map(|s| s.to_string());
            let detail = binding_value(b, "itemDescription")
                .filter(|d| !d.trim().is_empty())
                .map(|s| s.to_string());
            out.push(HubCandidate::new(id, name, detail));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::FixtureTransport;
    use serde_json::json;

    fn one_item() -> Value {
        json!({ "results": { "bindings": [
            { "item": { "value": "http://www.wikidata.org/entity/Q780442" },
              "itemLabel": { "value": "Uber" },
              "itemDescription": { "value": "American transportation company" } },
            // The same item again via its `www.` spelling — must collapse.
            { "item": { "value": "http://www.wikidata.org/entity/Q780442" },
              "itemLabel": { "value": "Uber" },
              "itemDescription": { "value": "American transportation company" } }
        ]}})
    }

    #[test]
    fn duplicate_spellings_of_one_item_are_one_candidate() {
        let c = WikidataWebsiteResolver::parse(&one_item());
        assert_eq!(c.len(), 1, "one item, two P856 spellings");
        assert_eq!(c[0].id.key(), "wikidata:Q780442");
        assert_eq!(
            c[0].label().as_deref(),
            Some("Uber (American transportation company)")
        );
    }

    #[test]
    fn a_qid_label_is_dropped_rather_than_shown_as_a_name() {
        let v = json!({ "results": { "bindings": [
            { "item": { "value": "http://www.wikidata.org/entity/Q1" },
              "itemLabel": { "value": "Q1" },
              "itemDescription": { "value": "a thing" } }
        ]}});
        let c = WikidataWebsiteResolver::parse(&v);
        assert_eq!(c[0].label().as_deref(), Some("a thing"));
    }

    #[test]
    fn an_empty_or_shapeless_response_is_no_candidates() {
        assert!(WikidataWebsiteResolver::parse(&json!({})).is_empty());
        assert!(
            WikidataWebsiteResolver::parse(&json!({"results": {"bindings": []}})).is_empty()
        );
    }

    #[test]
    fn the_query_matches_exact_urls_never_a_substring() {
        // The regression this file exists for: a substring test on the website
        // would also match `notuber.com`, and two items in one answer commit as
        // one entity.
        let q = WikidataWebsiteResolver::new("uber.com", Arc::new(FixtureTransport::from_pairs(vec![])))
            .query();
        assert!(!q.contains("CONTAINS"), "q={q}");
        assert!(q.contains("<https://uber.com>"), "q={q}");
        assert!(q.contains("<https://www.uber.com/>"), "q={q}");
    }

    #[tokio::test]
    async fn candidates_reads_the_fixture_url() {
        let probe =
            WikidataWebsiteResolver::new("uber.com", Arc::new(FixtureTransport::from_pairs(vec![])));
        let url = probe.url();
        let transport = FixtureTransport::from_pairs(vec![("GET", &url, one_item())]);
        let c = WikidataWebsiteResolver::new("uber.com", Arc::new(transport))
            .candidates()
            .await
            .unwrap();
        assert_eq!(c.len(), 1);
    }
}
