//! Placekey adapter (reverse-resolver) — `name/address → Placekey`.
//!
//! Placekey is a free, open identity anchor for physical places, keyed on
//! name/address. It is an *anchor*, not a data hub: it returns a Placekey, not a
//! website or phone. The completion orchestrator pairs it with a Google place_id
//! lookup (see `complete::resolve_name`) so a name+address query still
//! completes to website + phone.
//!
//! Endpoint: `POST https://api.placekey.io/v1/placekey` with an `apikey` header
//! and a `{"query": {...}}` body.

use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};

use super::push_id;
use crate::complete::NameQuery;
use crate::model::EntityRecord;
use crate::resolve::Resolver;
use crate::transport::HttpTransport;

const ENDPOINT: &str = "https://api.placekey.io/v1/placekey";

pub struct PlacekeyResolver {
    query: NameQuery,
    api_key: String,
    transport: Arc<dyn HttpTransport>,
}

impl PlacekeyResolver {
    pub fn new(query: NameQuery, api_key: String, transport: Arc<dyn HttpTransport>) -> Self {
        PlacekeyResolver {
            query,
            api_key,
            transport,
        }
    }

    /// Build the Placekey request body from the query fields that are present.
    fn body(&self) -> Value {
        let mut q = serde_json::Map::new();
        let mut put = |k: &str, v: &Option<String>| {
            if let Some(s) = v {
                if !s.trim().is_empty() {
                    q.insert(k.to_string(), Value::String(s.clone()));
                }
            }
        };
        put("location_name", &self.query.name);
        put("street_address", &self.query.street);
        put("city", &self.query.city);
        put("region", &self.query.region);
        put("iso_country_code", &self.query.country);
        json!({ "query": Value::Object(q) })
    }

    /// Extract the `placekey` field from a Placekey API response.
    pub fn parse(value: &Value) -> Option<String> {
        value
            .get("placekey")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }
}

impl Resolver for PlacekeyResolver {
    fn harvest(&self) -> Result<EntityRecord> {
        let headers = [
            ("apikey", self.api_key.as_str()),
            ("Content-Type", "application/json"),
        ];
        let value = self.transport.post_json(ENDPOINT, &headers, &self.body())?;
        let mut record = EntityRecord::default();
        if let Some(pk) = Self::parse(&value) {
            push_id(&mut record, "placekey", &pk);
        }
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::FixtureTransport;

    #[test]
    fn parse_reads_placekey() {
        let v = json!({ "placekey": "227-223@5vg-7gq-tvz", "query_id": "0" });
        assert_eq!(
            PlacekeyResolver::parse(&v).as_deref(),
            Some("227-223@5vg-7gq-tvz")
        );
        assert_eq!(PlacekeyResolver::parse(&json!({})), None);
    }

    #[test]
    fn harvest_yields_placekey_id() {
        let transport = FixtureTransport::from_pairs(vec![(
            "POST",
            ENDPOINT,
            json!({ "placekey": "227-223@5vg-7gq-tvz" }),
        )]);
        let query = NameQuery {
            name: Some("Blue Bottle Coffee".into()),
            city: Some("Oakland".into()),
            region: Some("CA".into()),
            country: Some("US".into()),
            ..Default::default()
        };
        let r = PlacekeyResolver::new(query, String::new(), Arc::new(transport));
        let rec = r.harvest().unwrap();
        let keys: Vec<String> = rec.same_as.iter().map(|i| i.key()).collect();
        assert_eq!(keys, vec!["placekey:227-223@5vg-7gq-tvz".to_string()]);
    }
}
