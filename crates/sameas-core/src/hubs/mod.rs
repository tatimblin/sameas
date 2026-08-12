//! Hub adapters — bootstrap completion by reaching external identity hubs.
//!
//! Each adapter implements the [`crate::resolve::Resolver`] trait: it captures
//! an input identifier plus an [`HttpTransport`] and, on `harvest()`, calls the
//! hub and returns an [`EntityRecord`] of the identifiers it learned. The JSON →
//! record step is a pure `parse()` function per adapter, so it is unit-tested on
//! canned JSON with no transport.
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
pub mod wikidata;

pub use placekey::PlacekeyResolver;
pub use places::{PlaceDetailsResolver, PlaceTextSearchResolver, TextSearchInput};
pub use tmdb::TmdbResolver;
pub use wikidata::WikidataResolver;

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
