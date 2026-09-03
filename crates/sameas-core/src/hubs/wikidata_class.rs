//! Wikidata P31 (instance of) type gate — which of these items are
//! **organization-shaped**?
//!
//! `wbsearchentities` ranks by search relevance, not by kind, so a name search
//! for an organization comes back mixed: `Uber` is a company, a Nomeansno album
//! and a German preposition; `Mercury` is a planet, an element, a record label
//! and a bank. Handing all of that back as `candidates` is not wrong, but it is a
//! worse answer than the hub can support, and it turns a resolvable query into a
//! refusal the caller cannot act on.
//!
//! **This is a filter, never a chooser.** It removes candidates that Wikidata
//! itself says are not organizations; it never ranks, scores, or breaks a tie
//! among the survivors. If the filter leaves more than one, the answer is still
//! `ambiguous_among_n` with candidates — refuse over guess is untouched. And it is
//! **fail-open** at every step (a hub error, a shapeless response, or a filter
//! that would empty the list leaves the original list alone), because a narrowing
//! that erases the right answer is worse than noise the caller can read.
//!
//! One SPARQL call for the whole candidate set (`VALUES ?item { … }`), on the free
//! hub, so an org name query costs two Wikidata calls and no money.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use super::binding_value;
use crate::model::ExternalId;
use crate::transport::HttpTransport;

const ENDPOINT: &str = "https://query.wikidata.org/sparql";

/// The class roots an organization-shaped item must reach through
/// `P31/P279*` (instance of, then any number of subclass-of hops).
///
/// Deliberately *roots*, not a list of concrete classes: real items are typed
/// `public company`, `startup`, `taxicab company`, `subsidiary`, `non-profit
/// organization` — hundreds of leaves, all of which reach one of these by
/// subclass-of. Enumerating leaves would silently drop whatever we forgot.
///
/// * `Q43229` organization — the root of the schema.org `Organization` hierarchy.
/// * `Q4830453` business — a subclass of organization today, listed anyway so a
///   future ontology edit that detaches it cannot quietly empty this gate.
/// * `Q431289` brand — NOT an organization in Wikidata's ontology, but it is
///   routinely how a consumer-facing name (the one an end user types) is modelled,
///   and a brand item carries the P856 the org path is after.
const ORG_CLASS_ROOTS: &[&str] = &["Q43229", "Q4830453", "Q431289"];

pub struct WikidataClassResolver {
    qids: Vec<String>,
    transport: Arc<dyn HttpTransport>,
}

impl WikidataClassResolver {
    /// `qids` are already-normalized QID values (`Q17431399`).
    pub fn new(qids: Vec<String>, transport: Arc<dyn HttpTransport>) -> Self {
        WikidataClassResolver { qids, transport }
    }

    fn query(&self) -> String {
        let items = self
            .qids
            .iter()
            .map(|q| format!("wd:{q}"))
            .collect::<Vec<_>>()
            .join(" ");
        let classes = ORG_CLASS_ROOTS
            .iter()
            .map(|c| format!("wd:{c}"))
            .collect::<Vec<_>>()
            .join(" ");
        format!(
            "SELECT ?item WHERE {{ \
               VALUES ?item {{ {items} }} \
               VALUES ?class {{ {classes} }} \
               ?item wdt:P31/wdt:P279* ?class. \
             }}"
        )
    }

    pub(crate) fn url(&self) -> String {
        let encoded: String =
            url::form_urlencoded::byte_serialize(self.query().as_bytes()).collect();
        format!("{ENDPOINT}?format=json&query={encoded}")
    }

    /// The subset of the input QIDs that are organization-shaped.
    pub async fn org_shaped(&self) -> Result<HashSet<String>> {
        if self.qids.is_empty() {
            return Ok(HashSet::new());
        }
        let value = self.transport.get_json(&self.url()).await?;
        Ok(Self::parse(&value))
    }

    /// Collect the `?item` bindings as normalized QID values. A subclass chain can
    /// reach several roots, so one item can appear several times — a set.
    pub fn parse(value: &Value) -> HashSet<String> {
        let bindings = match value
            .get("results")
            .and_then(|r| r.get("bindings"))
            .and_then(|b| b.as_array())
        {
            Some(b) => b,
            None => return HashSet::new(),
        };
        bindings
            .iter()
            .filter_map(|b| binding_value(b, "item"))
            .filter_map(|item| ExternalId::new("wikidata", item).ok())
            .map(|id| id.value().to_string())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::FixtureTransport;
    use serde_json::json;

    #[test]
    fn parses_and_dedupes_item_bindings() {
        let v = json!({ "results": { "bindings": [
            { "item": { "value": "http://www.wikidata.org/entity/Q17431399" } },
            // Reached through two class roots — one item, not two.
            { "item": { "value": "http://www.wikidata.org/entity/Q17431399" } },
            { "item": { "value": "http://www.wikidata.org/entity/Q95" } }
        ]}});
        let set = WikidataClassResolver::parse(&v);
        assert_eq!(set.len(), 2);
        assert!(set.contains("Q17431399"));
    }

    #[test]
    fn a_shapeless_response_is_an_empty_set_not_an_error() {
        // Fail-open depends on this: the caller reads "narrowed to nothing" and
        // keeps the original list.
        assert!(WikidataClassResolver::parse(&json!({})).is_empty());
    }

    #[test]
    fn the_query_walks_the_subclass_closure_from_the_roots() {
        let q = WikidataClassResolver::new(
            vec!["Q17431399".into(), "Q7877036".into()],
            Arc::new(FixtureTransport::from_pairs(vec![])),
        )
        .query();
        assert!(q.contains("wdt:P31/wdt:P279*"), "q={q}");
        assert!(q.contains("wd:Q43229") && q.contains("wd:Q4830453"), "q={q}");
        assert!(q.contains("wd:Q17431399") && q.contains("wd:Q7877036"), "q={q}");
    }

    #[tokio::test]
    async fn an_empty_input_makes_no_call_at_all() {
        // The transport has no fixtures: a call would error, so this asserts the
        // short-circuit rather than just the result.
        let set = WikidataClassResolver::new(vec![], Arc::new(FixtureTransport::from_pairs(vec![])))
            .org_shaped()
            .await
            .unwrap();
        assert!(set.is_empty());
    }
}
