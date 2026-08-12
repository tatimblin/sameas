//! Google Places adapters (legacy web-service API — query-param key, single GET).
//!
//! * [`PlaceDetailsResolver`]: `place_id → website, phone` (M2 exit criterion 2).
//! * [`PlaceTextSearchResolver`] (reverse): `name/address` or `phone → place_id`.

use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use serde_json::Value;

use super::push_id;
use crate::model::{EntityRecord, ExternalId};
use crate::resolve::Resolver;
use crate::transport::HttpTransport;

const BASE: &str = "https://maps.googleapis.com";

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
        format!(
            "{BASE}/maps/api/place/details/json?place_id={}&fields=website,international_phone_number,name&key={}",
            self.place_id.value(),
            self.api_key
        )
    }

    /// Parse a Place Details response into a record. Reads `result.website`,
    /// `result.international_phone_number`, and `result.name`. Errors if the
    /// response `status` is present and not `OK`.
    pub fn parse(value: &Value) -> Result<EntityRecord> {
        if let Some(status) = value.get("status").and_then(|s| s.as_str()) {
            if status != "OK" {
                bail!("google place details status {status:?}");
            }
        }
        let result = value
            .get("result")
            .ok_or_else(|| anyhow!("google place details: missing result"))?;

        let mut record = EntityRecord::default();
        if let Some(name) = result.get("name").and_then(|v| v.as_str()) {
            record.name = Some(name.to_string());
        }
        if let Some(site) = result.get("website").and_then(|v| v.as_str()) {
            push_id(&mut record, "domain", site);
        }
        if let Some(phone) = result
            .get("international_phone_number")
            .and_then(|v| v.as_str())
        {
            push_id(&mut record, "phone", phone);
        }
        Ok(record)
    }
}

impl Resolver for PlaceDetailsResolver {
    fn harvest(&self) -> Result<EntityRecord> {
        let value = self.transport.get_json(&self.url())?;
        let mut record = Self::parse(&value)?;
        push_id(&mut record, "google_place_id", self.place_id.value());
        Ok(record)
    }
}

// ---------------------------------------------------------------------------
// Text Search (reverse): name/address or phone → place_id
// ---------------------------------------------------------------------------

/// What kind of text query is being sent to Find-Place-From-Text.
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
        let (inputtype, raw) = match &self.input {
            TextSearchInput::Text(t) => ("textquery", t.as_str()),
            TextSearchInput::Phone(p) => ("phonenumber", p.as_str()),
        };
        let encoded: String = url::form_urlencoded::byte_serialize(raw.as_bytes()).collect();
        format!(
            "{BASE}/maps/api/place/findplacefromtext/json?input={encoded}&inputtype={inputtype}&fields=place_id&key={}",
            self.api_key
        )
    }

    /// Parse a Find-Place response, returning the best candidate's `place_id`.
    pub fn parse(value: &Value) -> Result<Option<String>> {
        if let Some(status) = value.get("status").and_then(|s| s.as_str()) {
            // ZERO_RESULTS is a normal "no match", not an error.
            if status != "OK" && status != "ZERO_RESULTS" {
                bail!("google find-place status {status:?}");
            }
        }
        Ok(value
            .get("candidates")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|c| c.get("place_id"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()))
    }
}

impl Resolver for PlaceTextSearchResolver {
    fn harvest(&self) -> Result<EntityRecord> {
        let value = self.transport.get_json(&self.url())?;
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
    use serde_json::json;

    #[test]
    fn place_details_parses_website_and_phone() {
        let v = json!({
            "status": "OK",
            "result": {
                "name": "Blue Bottle Coffee",
                "website": "https://bluebottlecoffee.com/",
                "international_phone_number": "+1 510-653-3394"
            }
        });
        let rec = PlaceDetailsResolver::parse(&v).unwrap();
        assert_eq!(rec.name.as_deref(), Some("Blue Bottle Coffee"));
        let keys: Vec<String> = rec.same_as.iter().map(|i| i.key()).collect();
        assert!(keys.contains(&"domain:bluebottlecoffee.com".to_string()));
        assert!(keys.contains(&"phone:+15106533394".to_string()));
    }

    #[test]
    fn place_details_rejects_bad_status() {
        let v = json!({ "status": "NOT_FOUND", "result": {} });
        assert!(PlaceDetailsResolver::parse(&v).is_err());
    }

    #[test]
    fn text_search_reads_best_candidate() {
        let v = json!({ "status": "OK", "candidates": [{ "place_id": "ChIJabc" }, { "place_id": "ChIJxyz" }] });
        assert_eq!(
            PlaceTextSearchResolver::parse(&v).unwrap().as_deref(),
            Some("ChIJabc")
        );
        let zero = json!({ "status": "ZERO_RESULTS", "candidates": [] });
        assert_eq!(PlaceTextSearchResolver::parse(&zero).unwrap(), None);
    }
}
