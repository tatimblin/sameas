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
use async_trait::async_trait;
use serde_json::{json, Value};

use super::push_id;
use crate::model::{EntityRecord, ExternalId};
use crate::resolve::Resolver;
use crate::transport::HttpTransport;

const BASE: &str = "https://places.googleapis.com";

/// Fields for Place Details. `websiteUri`/phone are the Enterprise SKU — required
/// for our use. Never use the `*` wildcard mask in production.
const DETAILS_MASK: &str = "id,displayName,websiteUri,internationalPhoneNumber,nationalPhoneNumber";
/// Fields for [`PlaceDetailsResolver::describe`] — name + address only, so a
/// candidate can be *labelled* without buying the Enterprise fields the crosswalk
/// needs. Same endpoint, strictly cheaper mask.
const DESCRIBE_MASK: &str = "id,displayName,formattedAddress";
/// Text Search fields use the `places.` prefix (results nest under `places[]`).
///
/// `formattedAddress` rides along **in the same request and the same SKU tier as
/// `displayName`**, which was already in this mask. That one word is what makes
/// an ambiguous place list self-describing for free: without it every un-graphed
/// candidate needs its own Place Details call to become choosable.
const TEXT_SEARCH_MASK: &str = "places.id,places.displayName,places.formattedAddress";

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

    /// Read just the display name + formatted address from a Place Details
    /// response — what a human needs to tell two same-named locations apart.
    pub fn parse_description(value: &Value) -> (Option<String>, Option<String>) {
        let name = value
            .get("displayName")
            .and_then(|d| d.get("text"))
            .and_then(|t| t.as_str())
            .map(|s| s.to_string());
        let address = value
            .get("formattedAddress")
            .and_then(|a| a.as_str())
            .filter(|a| !a.trim().is_empty())
            .map(|s| s.to_string());
        (name, address)
    }

    /// Fetch `(display name, formatted address)` for one candidate.
    ///
    /// The **only** billable per-candidate call on the ambiguity path, so it is
    /// budgeted by the caller (see `PLACE_DETAILS_FANOUT_CAP` in
    /// `crate::complete`) and asks for the cheap mask, not the crosswalk one.
    pub async fn describe(&self) -> Result<(Option<String>, Option<String>)> {
        let headers = [
            ("X-Goog-Api-Key", self.api_key.as_str()),
            ("X-Goog-FieldMask", DESCRIBE_MASK),
        ];
        let value = self
            .transport
            .get_json_with_headers(&self.url(), &headers)
            .await?;
        Ok(Self::parse_description(&value))
    }
}

#[async_trait(?Send)]
impl Resolver for PlaceDetailsResolver {
    async fn harvest(&self) -> Result<EntityRecord> {
        let headers = [
            ("X-Goog-Api-Key", self.api_key.as_str()),
            ("X-Goog-FieldMask", DETAILS_MASK),
        ];
        let value = self
            .transport
            .get_json_with_headers(&self.url(), &headers)
            .await?;
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

/// One Text Search result: the id to bind to, plus whatever the field mask
/// returned to make it choosable.
#[derive(Clone, Debug)]
pub struct PlaceCandidate {
    pub place_id: String,
    pub name: Option<String>,
    pub address: Option<String>,
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
    ///
    /// `pub`, not `pub(crate)`: the ambiguity list is what a caller outside this
    /// crate (the Worker's name route) turns into "which one did you mean?".
    pub async fn candidates(&self) -> Result<Vec<String>> {
        Ok(self
            .search()
            .await?
            .into_iter()
            .map(|c| c.place_id)
            .collect())
    }

    /// Like [`Self::candidates`], but keeping the name + address the same
    /// response already carried, so an ambiguous list is choosable without a
    /// per-candidate Place Details call.
    pub async fn search(&self) -> Result<Vec<PlaceCandidate>> {
        let headers = [
            ("X-Goog-Api-Key", self.api_key.as_str()),
            ("X-Goog-FieldMask", TEXT_SEARCH_MASK),
        ];
        let value = self
            .transport
            .post_json(&self.url(), &headers, &self.body())
            .await?;
        Ok(Self::parse_candidates(&value))
    }

    /// Parse a Text Search (New) response, returning the best candidate's id.
    pub fn parse(value: &Value) -> Result<Option<String>> {
        Ok(Self::parse_all(value)?.into_iter().next())
    }

    /// Parse a Text Search (New) response, returning every `places[].id` in order.
    /// An absent/empty `places` array means no match (the New API has no
    /// `ZERO_RESULTS` status — it returns 200 with no `places`).
    pub fn parse_all(value: &Value) -> Result<Vec<String>> {
        Ok(Self::parse_candidates(value)
            .into_iter()
            .map(|c| c.place_id)
            .collect())
    }

    /// Parse a Text Search (New) response into candidates, **preserving Google's
    /// rank order**, carrying whatever display name / address the field mask
    /// returned.
    pub fn parse_candidates(value: &Value) -> Vec<PlaceCandidate> {
        value
            .get("places")
            .and_then(|p| p.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|p| {
                        let place_id = p.get("id").and_then(|v| v.as_str())?.to_string();
                        let (name, address) = PlaceDetailsResolver::parse_description(p);
                        Some(PlaceCandidate {
                            place_id,
                            name,
                            address,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[async_trait(?Send)]
impl Resolver for PlaceTextSearchResolver {
    async fn harvest(&self) -> Result<EntityRecord> {
        let headers = [
            ("X-Goog-Api-Key", self.api_key.as_str()),
            ("X-Goog-FieldMask", TEXT_SEARCH_MASK),
        ];
        let value = self
            .transport
            .post_json(&self.url(), &headers, &self.body())
            .await?;
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
    fn text_search_candidates_carry_name_and_address() {
        // The mask asks for formattedAddress, so an ambiguous list is choosable
        // from the SEARCH response alone — no per-candidate Details call.
        let v = json!({ "places": [
            { "id": "ChIJ_hayes", "displayName": { "text": "Souvla" },
              "formattedAddress": "517 Hayes St, San Francisco, CA 94102, USA" },
            { "id": "ChIJ_marina", "displayName": { "text": "Souvla" },
              "formattedAddress": "2272 Chestnut St, San Francisco, CA 94123, USA" }
        ]});
        let c = PlaceTextSearchResolver::parse_candidates(&v);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].place_id, "ChIJ_hayes");
        assert_eq!(c[0].name.as_deref(), Some("Souvla"));
        assert!(c[0].address.as_deref().unwrap().starts_with("517 Hayes St"));
        // Same name, different address — the address is the whole disambiguator.
        assert_eq!(c[1].name.as_deref(), Some("Souvla"));
        assert_ne!(c[0].address, c[1].address);
    }

    #[test]
    fn details_description_is_name_plus_address() {
        let v = json!({
            "id": "ChIJ_hayes",
            "displayName": { "text": "Souvla" },
            "formattedAddress": "517 Hayes St, San Francisco, CA 94102, USA"
        });
        let (name, address) = PlaceDetailsResolver::parse_description(&v);
        assert_eq!(name.as_deref(), Some("Souvla"));
        assert!(address.unwrap().contains("Hayes"));
        // Blank/missing fields read as absent, never as an empty label.
        let (n, a) = PlaceDetailsResolver::parse_description(&json!({ "formattedAddress": "  " }));
        assert!(n.is_none() && a.is_none());
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
