//! Google Places API (New) v1 adapters (header auth + field mask).
//!
//! * [`PlaceDetailsResolver`]: `place_id → website, phone` (M2 exit criterion 2).
//! * [`PlaceTextSearchResolver`] (reverse): `name/address` or `phone → place_id`.
//!
//! The New API drops the legacy in-body `status` string: success is HTTP 2xx and
//! the fields are camelCase; failures are HTTP 4xx/5xx (mapped to a clear error by
//! the transport). Auth is the `X-Goog-Api-Key` header and every request carries a
//! required `X-Goog-FieldMask`.

use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};

use super::push_id;
use crate::model::{EntityRecord, ExternalId};
use crate::resolve::Resolver;
use crate::transport::HttpTransport;

const BASE: &str = "https://places.googleapis.com";

/// Fields for Place Details. `websiteUri`/phone are the Enterprise SKU — required
/// for our use. Never use the `*` wildcard mask in production.
const DETAILS_MASK: &str = "id,displayName,websiteUri,internationalPhoneNumber,nationalPhoneNumber";
/// Text Search fields use the `places.` prefix (results nest under `places[]`).
const TEXT_SEARCH_MASK: &str = "places.id,places.displayName";

// ---------------------------------------------------------------------------
// Place Details: place_id → website, phone
// ---------------------------------------------------------------------------

pub struct PlaceDetailsResolver {
    place_id: ExternalId,
    api_key: String,
    transport: Arc<dyn HttpTransport>,
}

impl PlaceDetailsResolver {
    pub fn new(place_id: ExternalId, api_key: String, transport: Arc<dyn HttpTransport>) -> Self {
        PlaceDetailsResolver {
            place_id,
            api_key,
            transport,
        }
    }

    pub(crate) fn url(&self) -> String {
        format!("{BASE}/v1/places/{}", self.place_id.value())
    }

    /// Parse a Place Details (New) response — a bare place object (no `result`
    /// wrapper, no `status`). Reads `websiteUri`, `internationalPhoneNumber`
    /// (falling back to `nationalPhoneNumber`), and `displayName.text`.
    pub fn parse(value: &Value) -> Result<EntityRecord> {
        let mut record = EntityRecord::default();
        if let Some(name) = value
            .get("displayName")
            .and_then(|d| d.get("text"))
            .and_then(|t| t.as_str())
        {
            record.name = Some(name.to_string());
        }
        if let Some(site) = value.get("websiteUri").and_then(|v| v.as_str()) {
            push_id(&mut record, "domain", site);
        }
        if let Some(phone) = value
            .get("internationalPhoneNumber")
            .and_then(|v| v.as_str())
            .or_else(|| value.get("nationalPhoneNumber").and_then(|v| v.as_str()))
        {
            push_id(&mut record, "phone", phone);
        }
        Ok(record)
    }
}

impl Resolver for PlaceDetailsResolver {
    fn harvest(&self) -> Result<EntityRecord> {
        let headers = [
            ("X-Goog-Api-Key", self.api_key.as_str()),
            ("X-Goog-FieldMask", DETAILS_MASK),
        ];
        let value = self.transport.get_json_with_headers(&self.url(), &headers)?;
        let mut record = Self::parse(&value)?;
        push_id(&mut record, "google_place_id", self.place_id.value());
        Ok(record)
    }
}

// ---------------------------------------------------------------------------
// Text Search (reverse): name/address or phone → place_id
// ---------------------------------------------------------------------------

/// What kind of text query is being sent to Text Search. Both become `textQuery`
/// in the New API; the distinction is documentary (phone is low priority).
pub enum TextSearchInput {
    /// Free-text query, e.g. `"Blue Bottle Coffee, Oakland CA"`.
    Text(String),
    /// A phone number in E.164 form.
    Phone(String),
}

pub struct PlaceTextSearchResolver {
    input: TextSearchInput,
    api_key: String,
    transport: Arc<dyn HttpTransport>,
}

impl PlaceTextSearchResolver {
    pub fn new(input: TextSearchInput, api_key: String, transport: Arc<dyn HttpTransport>) -> Self {
        PlaceTextSearchResolver {
            input,
            api_key,
            transport,
        }
    }

    pub(crate) fn url(&self) -> String {
        format!("{BASE}/v1/places:searchText")
    }

    fn body(&self) -> Value {
        let query = match &self.input {
            TextSearchInput::Text(t) => t,
            TextSearchInput::Phone(p) => p,
        };
        json!({ "textQuery": query })
    }

    /// Run the search and return every candidate `place_id` (best-effort; used by
    /// the completion layer to detect ambiguity). POSTs with the api-key +
    /// field-mask headers.
    pub(crate) fn candidates(&self) -> Result<Vec<String>> {
        let headers = [
            ("X-Goog-Api-Key", self.api_key.as_str()),
            ("X-Goog-FieldMask", TEXT_SEARCH_MASK),
        ];
        let value = self
            .transport
            .post_json(&self.url(), &headers, &self.body())?;
        Self::parse_all(&value)
    }

    /// Parse a Text Search (New) response, returning the best candidate's id.
    pub fn parse(value: &Value) -> Result<Option<String>> {
        Ok(Self::parse_all(value)?.into_iter().next())
    }

    /// Parse a Text Search (New) response, returning every `places[].id` in order.
    /// An absent/empty `places` array means no match (the New API has no
    /// `ZERO_RESULTS` status — it returns 200 with no `places`).
    pub fn parse_all(value: &Value) -> Result<Vec<String>> {
        Ok(value
            .get("places")
            .and_then(|p| p.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|p| p.get("id").and_then(|v| v.as_str()))
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default())
    }
}

impl Resolver for PlaceTextSearchResolver {
    fn harvest(&self) -> Result<EntityRecord> {
        let headers = [
            ("X-Goog-Api-Key", self.api_key.as_str()),
            ("X-Goog-FieldMask", TEXT_SEARCH_MASK),
        ];
        let value = self
            .transport
            .post_json(&self.url(), &headers, &self.body())?;
        let mut record = EntityRecord::default();
        if let Some(place_id) = Self::parse(&value)? {
            push_id(&mut record, "google_place_id", &place_id);
        }
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn place_details_parses_v1_website_and_phone() {
        let v = json!({
            "id": "ChIJabc",
            "displayName": { "text": "Blue Bottle Coffee", "languageCode": "en" },
            "websiteUri": "https://bluebottlecoffee.com/",
            "internationalPhoneNumber": "+1 510-653-3394"
        });
        let rec = PlaceDetailsResolver::parse(&v).unwrap();
        assert_eq!(rec.name.as_deref(), Some("Blue Bottle Coffee"));
        let keys: Vec<String> = rec.same_as.iter().map(|i| i.key()).collect();
        assert!(keys.contains(&"domain:bluebottlecoffee.com".to_string()));
        assert!(keys.contains(&"phone:+15106533394".to_string()));
    }

    #[test]
    fn place_details_falls_back_to_national_phone() {
        let v = json!({
            "displayName": { "text": "X" },
            "nationalPhoneNumber": "(510) 653-3394"
        });
        let rec = PlaceDetailsResolver::parse(&v).unwrap();
        let keys: Vec<String> = rec.same_as.iter().map(|i| i.key()).collect();
        assert!(keys.contains(&"phone:+15106533394".to_string()));
    }

    #[test]
    fn place_details_tolerates_missing_fields() {
        // A place with no website/phone yields an empty record (not an error).
        let rec = PlaceDetailsResolver::parse(&json!({ "id": "ChIJabc" })).unwrap();
        assert!(rec.same_as.is_empty());
    }

    #[test]
    fn text_search_reads_place_ids() {
        let v = json!({ "places": [
            { "id": "ChIJ_a", "displayName": { "text": "A" } },
            { "id": "ChIJ_b", "displayName": { "text": "B" } }
        ]});
        assert_eq!(PlaceTextSearchResolver::parse(&v).unwrap().as_deref(), Some("ChIJ_a"));
        assert_eq!(
            PlaceTextSearchResolver::parse_all(&v).unwrap(),
            vec!["ChIJ_a".to_string(), "ChIJ_b".to_string()]
        );
        // No `places` → no match.
        assert!(PlaceTextSearchResolver::parse_all(&json!({})).unwrap().is_empty());
    }
}
