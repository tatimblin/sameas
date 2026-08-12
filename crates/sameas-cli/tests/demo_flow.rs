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
        let mut full = vec!["--json"];
        full.extend_from_slice(args);
        let out = self.run(&full);
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        value["canonical_id"].as_str().unwrap().to_string()
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

#[test]
fn resolve_is_stable_and_completes() {
    let h = Harness::new();

    // Seed the restaurant.
    h.run(&["ingest", &seed("blue-bottle.json")]);

    // The same entity must be reached from phone, place_id, and domain.
    let phone_id = h.canonical_id(&["resolve", "--phone", "+1-510-653-3394"]);
    let place_id = h.canonical_id(&[
        "resolve",
        "--place-id",
        "ChIJN1t_tDeuEmsRUsoyG83frY4",
    ]);
    let domain_id = h.canonical_id(&[
        "resolve",
        "--domain",
        "bluebottlecoffee.com",
        "--fixture",
        &fixture("blue-bottle.html"),
    ]);

    assert_eq!(phone_id, place_id, "phone and place_id must resolve equal");
    assert_eq!(place_id, domain_id, "place_id and domain must resolve equal");

    // Completion: the cluster carries all four identifiers.
    let out = h.run(&["--json", "entity", &phone_id]);
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    let same_as: Vec<String> = value["sameAs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(same_as.contains(&"domain:bluebottlecoffee.com".to_string()));
    assert!(same_as.contains(&"google_place_id:ChIJN1t_tDeuEmsRUsoyG83frY4".to_string()));
    assert!(same_as.contains(&"phone:+15106533394".to_string()));
    assert!(same_as.contains(&"wikidata:Q4926426".to_string()));
    assert_eq!(value["anchor"].as_str().unwrap(), "wikidata:Q4926426");
}

#[test]
fn resolve_by_generic_id_flag_hits_same_entity() {
    let h = Harness::new();
    h.run(&["ingest", &seed("blue-bottle.json")]);

    // The generic --id flag resolves a kind (yelp) that has no named CLI flag.
    let yelp_id = h.canonical_id(&["resolve", "--id", "yelp:blue-bottle-coffee-san-francisco"]);
    let place_id = h.canonical_id(&["resolve", "--place-id", "ChIJN1t_tDeuEmsRUsoyG83frY4"]);
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
