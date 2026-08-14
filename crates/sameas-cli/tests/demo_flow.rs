//! End-to-end integration test: exercises the compiled `sameas` binary through
//! the demo flow and asserts stable canonical identity across phone / place_id
//! / domain resolves, plus completion.

use std::path::PathBuf;
use std::process::Command;

fn workspace_root() -> PathBuf {
    // crates/sameas-cli -> workspace root
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .unwrap()
}

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_sameas")
}

struct Harness {
    db: PathBuf,
    _dir: tempfile::TempDir,
}

impl Harness {
    fn new() -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("test.db");
        Harness { db, _dir: dir }
    }

    fn run(&self, args: &[&str]) -> String {
        let output = Command::new(bin())
            .arg("--db")
            .arg(&self.db)
            .args(args)
            .output()
            .expect("failed to run sameas");
        assert!(
            output.status.success(),
            "command {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    /// Run without asserting success; return (success, stdout, stderr).
    fn run_raw(&self, args: &[&str]) -> (bool, String, String) {
        let output = Command::new(bin())
            .arg("--db")
            .arg(&self.db)
            .args(args)
            .output()
            .expect("failed to run sameas");
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    /// Create a subdir with the given `(filename, content)` files; returns its path.
    fn make_dir(&self, dirname: &str, files: &[(&str, &str)]) -> String {
        let dir = self._dir.path().join(dirname);
        std::fs::create_dir_all(&dir).unwrap();
        for (name, content) in files {
            std::fs::write(dir.join(name), content).unwrap();
        }
        dir.to_string_lossy().into_owned()
    }

    fn canonical_id(&self, args: &[&str]) -> String {
        self.value(args)["canonical_id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// Run with `--json` and parse the output.
    fn value(&self, args: &[&str]) -> serde_json::Value {
        let mut full = vec!["--json"];
        full.extend_from_slice(args);
        let out = self.run(&full);
        serde_json::from_str(&out).unwrap()
    }

    /// Write a file under the throwaway dir; returns its path.
    fn write_file(&self, name: &str, content: &str) -> String {
        let p = self._dir.path().join(name);
        std::fs::write(&p, content).unwrap();
        p.to_string_lossy().into_owned()
    }

    /// Create a hub-fixtures subdir with the given `(filename, json)` files;
    /// returns the dir path for `--hub-fixtures`.
    fn hub_dir(&self, dirname: &str, files: &[(&str, &str)]) -> String {
        let dir = self._dir.path().join(dirname);
        std::fs::create_dir_all(&dir).unwrap();
        for (name, content) in files {
            std::fs::write(dir.join(name), content).unwrap();
        }
        dir.to_string_lossy().into_owned()
    }
}

fn seed(name: &str) -> String {
    workspace_root()
        .join("examples/seed")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

fn fixture(name: &str) -> String {
    workspace_root()
        .join("examples/fixtures")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

fn hub_fixtures() -> String {
    workspace_root()
        .join("examples/fixtures/hubs")
        .to_string_lossy()
        .into_owned()
}

fn same_as_of(value: &serde_json::Value) -> Vec<String> {
    value["sameAs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

#[test]
fn resolve_is_stable_and_completes() {
    let h = Harness::new();

    // Seed the restaurant.
    h.run(&["ingest", &seed("blue-bottle.json")]);

    // The same entity is reached from any strong key (place_id, domain).
    let place_id = h.canonical_id(&["resolve", "--place-id", "EXAMPLE_blue_bottle_oakland"]);
    let domain_id = h.canonical_id(&[
        "resolve",
        "--domain",
        "bluebottlecoffee.com",
        "--fixture",
        &fixture("blue-bottle.html"),
    ]);
    assert_eq!(place_id, domain_id, "place_id and domain must resolve equal");

    // Completion: the cluster carries all four identifiers (phone included —
    // it was seeded as a corroborator).
    let value = h.value(&["entity", &place_id]);
    let same_as = same_as_of(&value);
    assert!(same_as.contains(&"domain:bluebottlecoffee.com".to_string()));
    assert!(same_as.contains(&"google_place_id:EXAMPLE_blue_bottle_oakland".to_string()));
    assert!(same_as.contains(&"phone:+15106533394".to_string()));
    assert!(same_as.contains(&"wikidata:Q4926426".to_string()));
    assert_eq!(value["anchor"].as_str().unwrap(), "wikidata:Q4926426");

    // M3: phone alone is a corroborator, not an identity. Resolving by phone
    // refuses (no strong key) and returns the entity as a candidate to confirm.
    let phone = h.value(&["resolve", "--phone", "+1-510-653-3394"]);
    assert_eq!(phone["status"].as_str().unwrap(), "unresolved");
    assert!(phone["canonical_id"].is_null());
    let cands = phone["candidates"].as_array().unwrap();
    assert!(
        cands.iter().any(|c| c["canonical_id"].as_str() == Some(place_id.as_str())),
        "phone should surface the seeded entity as a candidate: {cands:?}"
    );
}

#[test]
fn resolve_by_generic_id_flag_hits_same_entity() {
    let h = Harness::new();
    h.run(&["ingest", &seed("blue-bottle.json")]);

    // The generic --id flag resolves a kind (yelp) that has no named CLI flag.
    let yelp_id = h.canonical_id(&["resolve", "--id", "yelp:blue-bottle-coffee-san-francisco"]);
    let place_id = h.canonical_id(&["resolve", "--place-id", "EXAMPLE_blue_bottle_oakland"]);
    assert_eq!(
        yelp_id, place_id,
        "--id yelp:... must resolve to the same entity as --place-id"
    );

    // The completed cluster carries the yelp id.
    let out = h.run(&["--json", "entity", &yelp_id]);
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    let same_as: Vec<String> = value["sameAs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(same_as.contains(&"yelp:blue-bottle-coffee-san-francisco".to_string()));
}

#[test]
fn movie_completes_from_imdb() {
    let h = Harness::new();
    h.run(&["ingest", &seed("the-matrix.json")]);

    let out = h.run(&["--json", "resolve", "--imdb", "tt0133093"]);
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["status"].as_str().unwrap(), "hit");
    assert_eq!(value["anchor"].as_str().unwrap(), "wikidata:Q83495");
    let same_as: Vec<String> = value["sameAs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(same_as.contains(&"imdb:tt0133093".to_string()));
    assert!(same_as.contains(&"tmdb:603".to_string()));
    assert!(same_as.contains(&"wikidata:Q83495".to_string()));
}

// --- M2: hub bootstrapping (offline via examples/fixtures/hubs) -----------

#[test]
fn imdb_completes_from_hubs_with_empty_graph() {
    // Exit criterion 1: an IMDb id resolves to a QID and completes to
    // website + TMDb with NO prior graph state.
    let h = Harness::new();
    let fx = hub_fixtures();
    let out = h.run(&[
        "--json", "resolve", "--imdb", "tt0133093", "--complete", "--hub-fixtures", &fx,
    ]);
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["anchor"].as_str().unwrap(), "wikidata:Q83495");
    // Completed via a strong-key hub crosswalk (imdb → wikidata/tmdb).
    assert_eq!(value["confidence_reason"].as_str().unwrap(), "hub_crosswalk");
    assert!(value["confidence"].as_f64().unwrap() >= 0.85);
    let same_as = same_as_of(&value);
    assert!(same_as.contains(&"wikidata:Q83495".to_string()), "{same_as:?}");
    assert!(same_as.contains(&"tmdb:603".to_string()), "{same_as:?}");
    assert!(same_as.contains(&"domain:warnerbros.com".to_string()), "{same_as:?}");
}

#[test]
fn place_id_completes_website_and_phone_from_hubs() {
    // Exit criterion 2: a place_id completes to website + phone.
    let h = Harness::new();
    let fx = hub_fixtures();
    let out = h.run(&[
        "--json", "resolve", "--place-id", "EXAMPLE_blue_bottle_oakland", "--complete",
        "--hub-fixtures", &fx,
    ]);
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    let same_as = same_as_of(&value);
    assert!(same_as.contains(&"domain:bluebottlecoffee.com".to_string()), "{same_as:?}");
    assert!(same_as.contains(&"phone:+15106533394".to_string()), "{same_as:?}");
    // The website's provenance is the hub.
    assert_eq!(
        value["provenance"]["domain:bluebottlecoffee.com"].as_str().unwrap(),
        "google_places"
    );
}

#[test]
fn name_city_unique_match_resolves_via_place_id() {
    // Name + city (no street) can't produce a Placekey (min inputs), so it
    // resolves via the Google place_id. The fixture returns a SINGLE candidate →
    // a confident unique match (not the coarse floor).
    let h = Harness::new();
    let fx = hub_fixtures();
    let out = h.run(&[
        "--json", "resolve", "--name", "Blue Bottle Coffee", "--city", "Oakland", "--region",
        "CA", "--country", "US", "--complete", "--hub-fixtures", &fx,
    ]);
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["anchor"].as_str().unwrap(), "google_place_id:EXAMPLE_blue_bottle_oakland");
    assert_eq!(value["confidence_reason"].as_str().unwrap(), "place_unique_match");
    assert!(value["confidence"].as_f64().unwrap() >= 0.75);
    let same_as = same_as_of(&value);
    assert!(same_as.contains(&"google_place_id:EXAMPLE_blue_bottle_oakland".to_string()), "{same_as:?}");
    assert!(same_as.contains(&"domain:bluebottlecoffee.com".to_string()), "{same_as:?}");
    assert!(same_as.contains(&"phone:+15106533394".to_string()), "{same_as:?}");
    // No street → no Placekey.
    assert!(!same_as.iter().any(|k| k.starts_with("placekey:")), "{same_as:?}");
}

#[test]
fn full_address_resolves_to_placekey_anchor() {
    // A full street address lets Placekey run → Placekey (rank 1) is the anchor,
    // and place_id still completes to website + phone.
    let h = Harness::new();
    let fx = hub_fixtures();
    let out = h.run(&[
        "--json", "resolve", "--name", "Blue Bottle Coffee", "--address", "300 Webster St",
        "--city", "Oakland", "--region", "CA", "--country", "US", "--complete",
        "--hub-fixtures", &fx,
    ]);
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["anchor"].as_str().unwrap(), "placekey:227-223@5vg-7gq-tvz");
    assert_eq!(value["confidence_reason"].as_str().unwrap(), "placekey_address");
    let same_as = same_as_of(&value);
    assert!(same_as.contains(&"placekey:227-223@5vg-7gq-tvz".to_string()), "{same_as:?}");
    assert!(same_as.contains(&"domain:bluebottlecoffee.com".to_string()), "{same_as:?}");
    assert!(same_as.contains(&"phone:+15106533394".to_string()), "{same_as:?}");
}

#[test]
fn name_local_only_miss_is_unresolved() {
    // `--name` without `--complete` is a local-only lookup (no network). On an
    // empty graph it misses → unresolved, with a hint to re-run with --complete.
    let h = Harness::new();
    let v = h.value(&["resolve", "--name", "Blue Bottle Coffee", "--city", "Oakland"]);
    assert_eq!(v["status"].as_str().unwrap(), "unresolved");
    assert!(v["canonical_id"].is_null());
    assert_eq!(v["confidence_reason"].as_str().unwrap(), "needs_stronger_identifier");
    // M9: `matched_via` is strictly identifier-kind tags — empty on a miss. The
    // human guidance lives in the dedicated `hint` field, not polluting matched_via.
    assert!(v["matched_via"].as_array().unwrap().is_empty(), "matched_via={:?}", v["matched_via"]);
    assert!(
        v["hint"].as_str().unwrap_or_default().contains("--complete"),
        "expected a --complete hint, got {:?}",
        v["hint"]
    );
}

#[test]
fn name_query_is_cached_second_lookup_is_local() {
    // First resolve reaches the hub (fixtures) and records the name+qualifiers.
    // The second lookup runs local-only (NO --complete, no network) and still
    // resolves to the same entity — proving the name index cached it.
    let h = Harness::new();
    let fx = hub_fixtures();
    let first = h.value(&[
        "resolve", "--name", "Blue Bottle Coffee", "--city", "Oakland", "--region", "CA",
        "--country", "US", "--complete", "--hub-fixtures", &fx,
    ]);
    let id = first["canonical_id"].as_str().expect("first resolve should succeed");

    // No --complete, no --hub-fixtures → zero network. Must hit the local index.
    let second = h.value(&[
        "resolve", "--name", "Blue Bottle Coffee", "--city", "Oakland", "--region", "CA",
        "--country", "US",
    ]);
    assert_eq!(second["canonical_id"].as_str(), Some(id));
    assert_eq!(second["confidence_reason"].as_str().unwrap(), "local_name_match");
}

// --- M3: entity-grain, refuse, ambiguity ----------------------------------

#[test]
fn chain_locations_sharing_domain_stay_distinct() {
    // Two locations of one chain share a domain (Affiliation) but have distinct
    // place_ids (Identity) → they must NOT collapse into one entity.
    let h = Harness::new();
    let a = h.write_file(
        "kibatsu_sf.json",
        r#"{ "type": "LocalBusiness", "name": "Kibatsu SF",
             "sameAs": [ {"domain": "kibatsu.com"}, {"google_place_id": "KIBATSU_SF"} ] }"#,
    );
    let b = h.write_file(
        "kibatsu_oak.json",
        r#"{ "type": "LocalBusiness", "name": "Kibatsu Oakland",
             "sameAs": [ {"domain": "kibatsu.com"}, {"google_place_id": "KIBATSU_OAK"} ] }"#,
    );
    h.run(&["ingest", &a]);
    h.run(&["ingest", &b]);

    let sf = h.canonical_id(&["resolve", "--place-id", "KIBATSU_SF"]);
    let oak = h.canonical_id(&["resolve", "--place-id", "KIBATSU_OAK"]);
    assert_ne!(sf, oak, "two chain locations must stay distinct");

    // The shared domain resolves (single-valued) to the first owner, as a hit.
    let dom = h.value(&["resolve", "--domain", "kibatsu.com"]);
    assert_eq!(dom["status"].as_str().unwrap(), "hit");
    assert_eq!(dom["canonical_id"].as_str().unwrap(), sf);
}

#[test]
fn distinct_movies_sharing_a_domain_stay_distinct() {
    // Type-agnostic proof: two movies sharing a studio domain (Affiliation) but
    // with distinct IMDb ids (Identity) stay distinct — the rule isn't geo.
    let h = Harness::new();
    let a = h.write_file(
        "film_a.json",
        r#"{ "type": "Movie", "name": "Film A",
             "sameAs": [ {"domain": "studioexample.com"}, {"imdb": "tt1111111"} ] }"#,
    );
    let b = h.write_file(
        "film_b.json",
        r#"{ "type": "Movie", "name": "Film B",
             "sameAs": [ {"domain": "studioexample.com"}, {"imdb": "tt2222222"} ] }"#,
    );
    h.run(&["ingest", &a]);
    h.run(&["ingest", &b]);

    let fa = h.canonical_id(&["resolve", "--imdb", "tt1111111"]);
    let fb = h.canonical_id(&["resolve", "--imdb", "tt2222222"]);
    assert_ne!(fa, fb, "two films sharing a studio domain must stay distinct");
}

#[test]
fn name_city_with_no_hub_match_refuses() {
    // No public identifier resolvable → refuse (needs a stronger identifier).
    let h = Harness::new();
    let fx = h.hub_dir(
        "hubs_miss",
        &[
            (
                "findplace_zero.json",
                r#"{ "method": "POST",
                     "url": "https://places.googleapis.com/v1/places:searchText",
                     "response": {} }"#,
            ),
            (
                "placekey_miss.json",
                r#"{ "method": "POST",
                     "url": "https://api.placekey.io/v1/placekey",
                     "response": {} }"#,
            ),
        ],
    );
    let v = h.value(&[
        "resolve", "--name", "Nowhere Cafe", "--city", "Springfield", "--complete",
        "--hub-fixtures", &fx,
    ]);
    assert_eq!(v["status"].as_str().unwrap(), "unresolved");
    assert!(v["canonical_id"].is_null());
    assert_eq!(
        v["confidence_reason"].as_str().unwrap(),
        "needs_stronger_identifier"
    );
}

/// Live smoke test against the REAL Google Places (New) v1 API. Ignored by
/// default (non-deterministic + costs money). Run intentionally with:
///   `GOOGLE_PLACES_API_KEY=… cargo test -p sameas-cli --features live-fetch -- --ignored`
/// Skips (no failure) when the key env var is absent.
#[test]
#[ignore = "hits the live Google Places API; needs GOOGLE_PLACES_API_KEY + --features live-fetch"]
fn live_place_details_smoke() {
    if std::env::var("GOOGLE_PLACES_API_KEY").is_err() {
        eprintln!("skipping live_place_details_smoke: GOOGLE_PLACES_API_KEY not set");
        return;
    }
    let h = Harness::new();
    // ChIJN1t_tDeuEmsRUsoyG83frY4 is a real, stable Google place id (Google Sydney).
    // Live completion = --complete with NO --hub-fixtures.
    let value = h.value(&[
        "resolve", "--place-id", "ChIJN1t_tDeuEmsRUsoyG83frY4", "--complete",
    ]);
    assert!(value["canonical_id"].as_str().is_some(), "expected a resolved id: {value}");
    let same_as = same_as_of(&value);
    // A real Place Details response should yield at least a website domain.
    assert!(
        same_as.iter().any(|k| k.starts_with("domain:")),
        "expected a website from live Place Details: {same_as:?}"
    );
}

#[test]
fn name_city_ambiguous_returns_candidates() {
    // Text search returns >1 candidate → refuse, surface all candidates.
    let h = Harness::new();
    let fx = h.hub_dir(
        "hubs_ambiguous",
        &[
            (
                "findplace_two.json",
                r#"{ "method": "POST",
                     "url": "https://places.googleapis.com/v1/places:searchText",
                     "response": { "places": [ {"id": "CAND_A"}, {"id": "CAND_B"} ] } }"#,
            ),
            (
                "placekey_miss.json",
                r#"{ "method": "POST",
                     "url": "https://api.placekey.io/v1/placekey",
                     "response": {} }"#,
            ),
        ],
    );
    let v = h.value(&[
        "resolve", "--name", "Joe's Pizza", "--city", "New York", "--complete",
        "--hub-fixtures", &fx,
    ]);
    assert_eq!(v["status"].as_str().unwrap(), "unresolved");
    assert_eq!(v["confidence_reason"].as_str().unwrap(), "ambiguous_among_n");
    assert_eq!(v["candidates"].as_array().unwrap().len(), 2);
}

// --- coarse-cache mis-bind fix: specificity-monotonic name index ----------

#[test]
fn name_street_establishes_then_coarse_city_query_does_not_wrong_bind() {
    // THE FIX (T2): name+street+city pins ONE location (unique text-search
    // result). A later name+city (no street) query, whose hub text-search now
    // returns MULTIPLE Blue Bottles, must return `ambiguous_among_n` — it must
    // NOT be confidently served the single specific location learned first.
    let h = Harness::new();

    // Fixtures dir #1: the specific query resolves to exactly ONE place.
    let one = h.hub_dir(
        "hubs_one",
        &[
            (
                "findplace_one.json",
                r#"{ "method": "POST",
                     "url": "https://places.googleapis.com/v1/places:searchText",
                     "response": { "places": [ {"id": "BLUE_FERRY"} ] } }"#,
            ),
            (
                "placekey_hit.json",
                r#"{ "method": "POST",
                     "url": "https://api.placekey.io/v1/placekey",
                     "response": { "placekey": "222-227@5vg-7gr-abc" } }"#,
            ),
            (
                "details_ferry.json",
                r#"{ "method": "GET",
                     "url": "https://places.googleapis.com/v1/places/BLUE_FERRY",
                     "response": { "displayName": {"text": "Blue Bottle Coffee"},
                                   "websiteUri": "https://bluebottlecoffee.com/" } }"#,
            ),
        ],
    );
    let established = h.value(&[
        "resolve", "--name", "Blue Bottle Coffee", "--address", "1 Ferry Building", "--city",
        "San Francisco", "--complete", "--hub-fixtures", &one,
    ]);
    let e_id = established["canonical_id"].as_str().expect("establish E").to_string();

    // Fixtures dir #2: the coarse (name+city, no street) query returns MULTIPLE.
    let many = h.hub_dir(
        "hubs_many",
        &[
            (
                "findplace_many.json",
                r#"{ "method": "POST",
                     "url": "https://places.googleapis.com/v1/places:searchText",
                     "response": { "places": [ {"id": "BLUE_FERRY"}, {"id": "BLUE_MISSION"} ] } }"#,
            ),
            (
                "placekey_miss.json",
                r#"{ "method": "POST",
                     "url": "https://api.placekey.io/v1/placekey",
                     "response": {} }"#,
            ),
        ],
    );
    let coarse = h.value(&[
        "resolve", "--name", "Blue Bottle Coffee", "--city", "San Francisco", "--complete",
        "--hub-fixtures", &many,
    ]);
    assert_eq!(coarse["status"].as_str().unwrap(), "unresolved");
    assert_eq!(coarse["confidence_reason"].as_str().unwrap(), "ambiguous_among_n");
    assert!(coarse["canonical_id"].is_null(), "must NOT confidently bind to E: {coarse}");
    assert_ne!(coarse["canonical_id"].as_str(), Some(e_id.as_str()));

    // And a name+city query WITHOUT --complete (pure local) does not confidently
    // return E either — it is ambiguous from memory now (recorded above).
    let local = h.value(&["resolve", "--name", "Blue Bottle Coffee", "--city", "San Francisco"]);
    assert!(local["canonical_id"].is_null(), "local must not wrong-bind: {local}");
    assert_eq!(local["confidence_reason"].as_str().unwrap(), "ambiguous_among_n");
}

#[test]
fn cardinality_memory_answers_repeat_coarse_query_with_zero_hub_calls() {
    // T3: a name+city --complete query where the hub returns 3 → ambiguous AND
    // recorded. A later IDENTICAL query (WITHOUT --complete, so NO hub fixtures at
    // all → any hub call would error) is answered from local memory: ambiguous,
    // harvested 0, no hub call.
    let h = Harness::new();
    let three = h.hub_dir(
        "hubs_three",
        &[
            (
                "findplace_three.json",
                r#"{ "method": "POST",
                     "url": "https://places.googleapis.com/v1/places:searchText",
                     "response": { "places": [ {"id": "A"}, {"id": "B"}, {"id": "C"} ] } }"#,
            ),
            (
                "placekey_miss.json",
                r#"{ "method": "POST",
                     "url": "https://api.placekey.io/v1/placekey",
                     "response": {} }"#,
            ),
        ],
    );
    let first = h.value(&[
        "resolve", "--name", "Joe's Pizza", "--city", "New York", "--complete",
        "--hub-fixtures", &three,
    ]);
    assert_eq!(first["confidence_reason"].as_str().unwrap(), "ambiguous_among_n");
    assert_eq!(first["candidates"].as_array().unwrap().len(), 3);

    // Identical repeat, NO --complete and NO --hub-fixtures → zero network.
    let repeat = h.value(&["resolve", "--name", "Joe's Pizza", "--city", "New York"]);
    assert_eq!(repeat["confidence_reason"].as_str().unwrap(), "ambiguous_among_n");
    assert_eq!(repeat["candidates"].as_array().unwrap().len(), 3);
    assert_eq!(repeat["harvested"].as_u64().unwrap(), 0);
    assert_eq!(repeat["new_edges"].as_u64().unwrap(), 0);

    // A repeat WITH --complete but the SAME (would-error) fixtures also short-
    // circuits from memory before any hub call.
    let repeat_complete = h.value(&[
        "resolve", "--name", "Joe's Pizza", "--city", "New York", "--complete",
        "--hub-fixtures", &three,
    ]);
    assert_eq!(repeat_complete["confidence_reason"].as_str().unwrap(), "ambiguous_among_n");
}

#[test]
fn name_city_unique_match_repeat_is_local_hit() {
    // T4: name+city --complete where the hub returns exactly ONE → hit; the
    // repeat name+city query resolves locally (zero external) to the same entity.
    let h = Harness::new();
    let fx = hub_fixtures();
    let first = h.value(&[
        "resolve", "--name", "Blue Bottle Coffee", "--city", "Oakland", "--region", "CA",
        "--country", "US", "--complete", "--hub-fixtures", &fx,
    ]);
    let id = first["canonical_id"].as_str().expect("unique hit").to_string();
    assert_eq!(first["confidence_reason"].as_str().unwrap(), "place_unique_match");

    let repeat = h.value(&[
        "resolve", "--name", "Blue Bottle Coffee", "--city", "Oakland", "--region", "CA",
        "--country", "US",
    ]);
    assert_eq!(repeat["canonical_id"].as_str(), Some(id.as_str()));
    assert_eq!(repeat["confidence_reason"].as_str().unwrap(), "local_name_match");
}

// --- M5: resilient directory ingest ---------------------------------------

#[test]
fn dir_ingest_continues_past_a_bad_file_and_reports_it() {
    // A directory with one valid record and one malformed one: the good record
    // must still be committed (batch not aborted), the failure reported, and the
    // process exits non-zero (never a silent half-done batch).
    let h = Harness::new();
    let dir = h.make_dir(
        "batch",
        &[
            (
                "good.json",
                r#"{ "type": "LocalBusiness", "name": "Good Cafe",
                     "sameAs": [ {"google_place_id": "GOOD_PLACE"} ] }"#,
            ),
            ("bad.json", r#"{ not valid json "#),
        ],
    );

    let (ok, stdout, stderr) = h.run_raw(&["ingest", &dir]);
    assert!(!ok, "exit should be non-zero when a file fails");
    assert!(
        stdout.contains("ingested 1, skipped/failed 1"),
        "summary missing, stdout={stdout}"
    );
    assert!(stderr.contains("bad.json"), "failure not listed, stderr={stderr}");

    // The good record was committed despite the bad file — resolving it hits.
    let v = h.value(&["resolve", "--place-id", "GOOD_PLACE"]);
    assert_eq!(v["status"].as_str().unwrap(), "hit");
    assert!(v["canonical_id"].as_str().is_some());
}

#[test]
fn dir_ingest_skips_subdirectory_named_dot_json() {
    // A subdirectory literally named `x.json` must be skipped (not treated as a
    // record file, which used to error the whole batch).
    let h = Harness::new();
    let dir = h.make_dir(
        "batch2",
        &[(
            "good.json",
            r#"{ "type": "LocalBusiness", "name": "Cafe",
                 "sameAs": [ {"google_place_id": "P2"} ] }"#,
        )],
    );
    std::fs::create_dir_all(std::path::Path::new(&dir).join("nested.json")).unwrap();

    let (ok, stdout, _stderr) = h.run_raw(&["ingest", &dir]);
    assert!(ok, "a subdir named *.json must be skipped, not fail the batch");
    assert!(stdout.contains("ingested 1"), "stdout={stdout}");
}

#[test]
fn empty_dir_ingest_reports_zero_records() {
    // An empty directory (no *.json) prints explicit feedback, not silent success.
    let h = Harness::new();
    let dir = h.make_dir("empty", &[]);
    let (ok, stdout, _stderr) = h.run_raw(&["ingest", &dir]);
    assert!(ok, "empty dir is not an error");
    assert!(stdout.contains("0 records ingested"), "stdout={stdout}");
}

#[test]
fn single_bad_file_ingest_errors_loudly() {
    // A single file passed directly still errors loudly (exit non-zero), naming it.
    let h = Harness::new();
    let bad = h.write_file("solo_bad.json", r#"{ nope "#);
    let (ok, _stdout, stderr) = h.run_raw(&["ingest", &bad]);
    assert!(!ok, "a single malformed file must error");
    assert!(stderr.contains("solo_bad.json"), "stderr={stderr}");
}

// --- M10 + lows: input guards ---------------------------------------------

#[test]
fn geo_args_with_typed_source_are_rejected() {
    // Geo/qualifier args apply only to --name. Combining them with a typed source
    // (here --imdb) is rejected up-front rather than silently ignored.
    let h = Harness::new();
    let (ok, _stdout, stderr) = h.run_raw(&["resolve", "--imdb", "tt0133093", "--city", "Oakland"]);
    assert!(!ok, "geo args with a typed source must be rejected");
    assert!(
        stderr.contains("--name") || stderr.contains("name"),
        "error should reference --name, stderr={stderr}"
    );

    // The legitimate --name + geo path is unaffected (local miss, but accepted).
    let v = h.value(&["resolve", "--name", "Somewhere", "--city", "Oakland"]);
    assert_eq!(v["status"].as_str().unwrap(), "unresolved");
}

#[test]
fn empty_name_is_rejected() {
    // Parity with typed sources: a whitespace-only --name is rejected clearly.
    let h = Harness::new();
    let (ok, _stdout, stderr) = h.run_raw(&["resolve", "--name", "   "]);
    assert!(!ok, "empty --name must be rejected");
    assert!(stderr.contains("empty"), "stderr={stderr}");
}
