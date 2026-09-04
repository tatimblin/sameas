//! Hub adapters — bootstrap completion by reaching external identity hubs.
//!
//! Each adapter implements the [`crate::resolve::Resolver`] trait: it captures
//! an input identifier plus an [`HttpTransport`] and, on `harvest()`, calls the
//! hub and returns an [`EntityRecord`] of the identifiers it learned. The JSON →
//! record step is a pure `parse()` function per adapter, so it is unit-tested on
//! canned JSON with no transport.
//!
//! **Name-search adapters** ([`PlaceTextSearchResolver`], [`TmdbSearchResolver`],
//! [`WikidataSearchResolver`]) are the reverse shape: the query is a *string*, so
//! there is no input id to echo, and the interesting output is a LIST. They expose
//! `candidates()`/`search()` returning [`HubCandidate`]s (pure `parse()` underneath,
//! same as the forward adapters) and additionally implement `Resolver` over the
//! top-ranked hit so they compose with the completion BFS.
//!
//! **Invariant:** every adapter echoes its query id into the harvested record
//! (like `DomainResolver` does with the domain). Otherwise a harvested record
//! whose strong ids aren't yet in the graph would mint a *new* entity instead of
//! joining the input's cluster.

use crate::model::{EntityRecord, ExternalId};
use serde_json::Value;

pub mod placekey;
pub mod places;
pub mod tmdb;
pub mod tmdb_search;
pub mod wikidata;
pub mod wikidata_class;
pub mod wikidata_search;
pub mod wikidata_website;

pub use placekey::PlacekeyResolver;
pub use places::{PlaceCandidate, PlaceDetailsResolver, PlaceTextSearchResolver, TextSearchInput};
pub use tmdb::TmdbResolver;
pub use tmdb_search::TmdbSearchResolver;
pub use wikidata::WikidataResolver;
pub use wikidata_class::{ClassFacts, WikidataClassResolver};
pub use wikidata_search::WikidataSearchResolver;
pub use wikidata_website::WikidataWebsiteResolver;

/// Push a `tag:raw` identifier into a record, normalizing and de-duplicating.
/// Best-effort: a value that fails to normalize is silently skipped (hub data
/// is noisy; we harvest what we can).
pub(crate) fn push_id(record: &mut EntityRecord, tag: &str, raw: &str) {
    if raw.trim().is_empty() {
        return;
    }
    if let Ok(id) = ExternalId::new(tag, raw) {
        if !record.same_as.iter().any(|existing| existing == &id) {
            record.same_as.push(id);
        }
    }
}

/// Read `obj.<field>.value` as a string — the shape of a SPARQL binding cell.
pub(crate) fn binding_value<'a>(obj: &'a Value, field: &str) -> Option<&'a str> {
    obj.get(field)?.get("value")?.as_str()
}

/// One choosable option returned by a **name search** hub (`Souvla` → the three
/// SF locations; `Avatar` → the 2009 film, its sequel, and the cartoon).
///
/// A name search is the only path where sameas hands a *list* back to the caller
/// instead of resolving, so each entry has to be choosable by a human: an `id`
/// to echo back (the retry key), a `name`, and a `detail` that tells same-named
/// things apart — an address for a place, a year for a film. A candidate list
/// whose entries differ only by an opaque id is not an answer to "which one?".
pub struct HubCandidate {
    /// The identity key this candidate would bind to (`kind:value`).
    pub id: ExternalId,
    /// Display name / title, as the hub spells it.
    pub name: Option<String>,
    /// The disambiguator: `"517 Hayes St, San Francisco"`, `"2009 film"`,
    /// `"2005 TV series"`, a Wikidata description.
    pub detail: Option<String>,
}

impl HubCandidate {
    pub fn new(id: ExternalId, name: Option<String>, detail: Option<String>) -> HubCandidate {
        HubCandidate { id, name, detail }
    }

    /// The human-facing label: `"Avatar (2009 film)"`. `None` only when the hub
    /// gave us neither a name nor a detail (then the caller falls back to the
    /// bare key, or buys a description with an extra hub call).
    pub fn label(&self) -> Option<String> {
        match (self.name.as_deref(), self.detail.as_deref()) {
            (Some(n), Some(d)) => Some(format!("{n} ({d})")),
            (Some(n), None) => Some(n.to_string()),
            (None, Some(d)) => Some(d.to_string()),
            (None, None) => None,
        }
    }

    /// Normalized `name + detail` text, used to narrow a candidate list by the
    /// query's qualifier tokens (`--qualifier 2009` over a self-describing hub).
    pub fn haystack(&self) -> String {
        let mut s = String::new();
        if let Some(n) = &self.name {
            s.push_str(n);
            s.push(' ');
        }
        if let Some(d) = &self.detail {
            s.push_str(d);
        }
        crate::normalize::name_key(&s)
    }

    /// Does this candidate's own self-description contain every qualifier token?
    pub fn matches_all(&self, tokens: &[String]) -> bool {
        let hay = self.haystack();
        tokens.iter().all(|t| hay.contains(t.as_str()))
    }
}
