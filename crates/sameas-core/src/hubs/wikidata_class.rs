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
//! **It removes only what Wikidata positively contradicts.** The question asked of
//! each candidate is not "is this an organization?" but "does Wikidata say this is
//! something else?" — and only a *yes* removes it. An item with no `P31` at all, or
//! one whose subclass chain never reaches a root we know, is **kept**: absence of
//! evidence is not evidence, and Wikidata's coverage is uneven enough that treating
//! silence as a verdict would drop real organizations whose sibling happened to be
//! better described. That distinction is why the query asks for two facts, not one
//! (`?item wdt:P31 ?any` for *typed at all*, the OPTIONAL closure for *org*).
//!
//! It never ranks, scores, or breaks a tie among the survivors. Whatever is left
//! goes back to the ordinary rule — one resolves, several refuse with candidates —
//! and a survivor that reached one BECAUSE of this gate is labelled
//! `type_gate_unique_match` rather than passed off as a lone hub answer. It is also
//! **fail-open** at every step (a hub error, a shapeless response, or a filter that
//! would empty the list leaves the original list alone).
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

/// What one class query learned about a candidate set.
///
/// Two sets rather than one, because "not known to be an organization" and "known
/// to be something else" are different answers and only the second may remove a
/// candidate. An item missing from `typed` had no `P31` at all.
#[derive(Debug, Default)]
pub struct ClassFacts {
    /// Items carrying at least one `P31` statement.
    pub typed: HashSet<String>,
    /// Items whose `P31/P279*` chain reaches one of [`ORG_CLASS_ROOTS`].
    pub org: HashSet<String>,
}

impl ClassFacts {
    /// The only question the gate is allowed to act on: does Wikidata positively
    /// say this is something other than an organization? An untyped item answers
    /// `false` — it is kept.
    pub fn is_typed_non_org(&self, qid: &str) -> bool {
        self.typed.contains(qid) && !self.org.contains(qid)
    }
}

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
        // `?item wdt:P31 ?any` is REQUIRED, so an item with no instance-of
        // statement never appears in the results at all — that absence is the
        // "untyped, keep it" signal. The org test is OPTIONAL on top, so a typed
        // item that is not an organization comes back with `?org` unbound. One
        // round trip, both facts.
        format!(
            "SELECT ?item ?org WHERE {{ \
               VALUES ?item {{ {items} }} \
               ?item wdt:P31 ?any. \
               OPTIONAL {{ \
                 VALUES ?class {{ {classes} }} \
                 ?item wdt:P31/wdt:P279* ?class. \
                 BIND(true AS ?org) \
               }} \
             }}"
        )
    }

    pub(crate) fn url(&self) -> String {
        let encoded: String =
            url::form_urlencoded::byte_serialize(self.query().as_bytes()).collect();
        format!("{ENDPOINT}?format=json&query={encoded}")
    }

    /// What the hub knows about the class of each input QID.
    pub async fn classify(&self) -> Result<ClassFacts> {
        if self.qids.is_empty() {
            return Ok(ClassFacts::default());
        }
        let value = self.transport.get_json(&self.url()).await?;
        Ok(Self::parse(&value))
    }

    /// Fold the bindings into [`ClassFacts`].
    ///
    /// An item appears once per `P31` value, and again per class root its chain
    /// reaches, so the same QID arrives many times with `?org` bound on some rows
    /// and not others. Aggregation is therefore a union, not a per-row verdict:
    /// `?org` bound on ANY row means organization. Reading one row in isolation
    /// would type a company as non-org whenever its first `P31` happened to sort
    /// first.
    pub fn parse(value: &Value) -> ClassFacts {
        let mut facts = ClassFacts::default();
        let bindings = match value
            .get("results")
            .and_then(|r| r.get("bindings"))
            .and_then(|b| b.as_array())
        {
            Some(b) => b,
            None => return facts,
        };
        for b in bindings {
            let qid = match binding_value(b, "item")
                .and_then(|item| ExternalId::new("wikidata", item).ok())
            {
                Some(id) => id.value().to_string(),
                None => continue,
            };
            if binding_value(b, "org").is_some() {
                facts.org.insert(qid.clone());
            }
            facts.typed.insert(qid);
        }
        facts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::FixtureTransport;
    use serde_json::json;

    fn row(qid: &str, org: bool) -> Value {
        let mut o = json!({ "item": { "value": format!("http://www.wikidata.org/entity/{qid}") } });
        if org {
            o["org"] = json!({ "value": "true" });
        }
        o
    }

    #[test]
    fn org_is_a_union_over_rows_never_a_per_row_verdict() {
        // One item arrives once per P31 value and once per class root reached, so
        // the SAME qid shows up with `?org` bound on some rows and not others.
        // Reading a row in isolation would type a company as non-org whenever its
        // other P31 sorted first.
        let v = json!({ "results": { "bindings": [
            row("Q17431399", false),
            row("Q17431399", true),
            row("Q7877036", false)
        ]}});
        let facts = WikidataClassResolver::parse(&v);
        assert!(!facts.is_typed_non_org("Q17431399"), "one org row is enough");
        assert!(facts.is_typed_non_org("Q7877036"), "typed, and never an org");
    }

    #[test]
    fn an_item_with_no_p31_is_not_typed_and_so_is_never_removed() {
        // The absence rule. An untyped item never appears in the results at all,
        // and silence must not read as "not an organization".
        let facts = WikidataClassResolver::parse(&json!({ "results": { "bindings": [
            row("Q17431399", true)
        ]}}));
        assert!(!facts.is_typed_non_org("Q2475886"));
        assert!(!facts.typed.contains("Q2475886"));
    }

    #[test]
    fn a_shapeless_response_removes_nothing() {
        // Fail-open depends on this: nothing is typed, so nothing is removable.
        let facts = WikidataClassResolver::parse(&json!({}));
        assert!(facts.typed.is_empty() && facts.org.is_empty());
        assert!(!facts.is_typed_non_org("Q1"));
    }

    #[test]
    fn the_query_asks_for_typed_at_all_and_org_separately() {
        let q = WikidataClassResolver::new(
            vec!["Q1".into()],
            Arc::new(FixtureTransport::from_pairs(vec![])),
        )
        .query();
        // The required triple is the "typed at all" probe; the closure is optional
        // on top. Collapsing them back into one would restore absence-removal.
        assert!(q.contains("?item wdt:P31 ?any."), "q={q}");
        assert!(q.contains("OPTIONAL") && q.contains("BIND(true AS ?org)"), "q={q}");
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
        let facts =
            WikidataClassResolver::new(vec![], Arc::new(FixtureTransport::from_pairs(vec![])))
                .classify()
                .await
                .unwrap();
        assert!(facts.typed.is_empty());
    }
}
