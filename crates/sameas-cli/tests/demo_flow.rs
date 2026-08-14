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
fn name_city_resolves_to_placekey_anchor_at_low_confidence() {
    let h = Harness::new();
    let fx = hub_fixtures();
    let out = h.run(&[
        "--json", "resolve", "--name", "Blue Bottle Coffee", "--city", "Oakland", "--region",
        "CA", "--country", "US", "--complete", "--hub-fixtures", &fx,
    ]);
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    // Placekey (rank 1) is the anchor; a name+city query is coarse → low confidence.
    assert_eq!(value["anchor"].as_str().unwrap(), "placekey:227-223@5vg-7gq-tvz");
    assert!((value["confidence"].as_f64().unwrap() - 0.40).abs() < 1e-3);
    let same_as = same_as_of(&value);
    assert!(same_as.contains(&"google_place_id:EXAMPLE_blue_bottle_oakland".to_string()), "{same_as:?}");
    assert!(same_as.contains(&"domain:bluebottlecoffee.com".to_string()), "{same_as:?}");
    assert!(same_as.contains(&"phone:+15106533394".to_string()), "{same_as:?}");
}

#[test]
fn name_without_complete_is_rejected() {
    let h = Harness::new();
    let output = std::process::Command::new(bin())
        .arg("--db")
        .arg(&h.db)
        .args(["resolve", "--name", "Blue Bottle Coffee", "--city", "Oakland"])
        .output()
        .expect("failed to run sameas");
    assert!(!output.status.success(), "--name without --complete should fail");
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
                r#"{ "method": "GET",
                     "url": "https://maps.googleapis.com/maps/api/place/findplacefromtext/json",
                     "response": { "status": "ZERO_RESULTS", "candidates": [] } }"#,
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

#[test]
fn name_city_ambiguous_returns_candidates() {
    // Text search returns >1 candidate → refuse, surface all candidates.
    let h = Harness::new();
    let fx = h.hub_dir(
        "hubs_ambiguous",
        &[
            (
                "findplace_two.json",
                r#"{ "method": "GET",
                     "url": "https://maps.googleapis.com/maps/api/place/findplacefromtext/json",
                     "response": { "status": "OK",
                       "candidates": [ {"place_id": "CAND_A"}, {"place_id": "CAND_B"} ] } }"#,
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
