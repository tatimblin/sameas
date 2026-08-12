//! `sameas` CLI — a thin front-end over `sameas-core`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use clap::{ArgGroup, Args, Parser, Subcommand};

use sameas_core::{
    commit_record, complete_place_query, load_entity, resolve_and_complete, CompletionCtx,
    DirectRecordResolver, DomainResolver, EntityRecord, ExternalId, FixtureTransport, Graph,
    PlaceQuery, Resolver, ResolveOutput, Status,
};

#[derive(Parser)]
#[command(
    name = "sameas",
    about = "Resolve a partial identifier into a canonical entity + completed sameAs set.",
    version
)]
struct Cli {
    /// SQLite crosswalk-graph path.
    #[arg(long, global = true, default_value = "./sameas.db")]
    db: String,

    /// Emit JSON instead of the human table.
    #[arg(long, global = true)]
    json: bool,

    /// Force the human table (default). Present for symmetry with --json.
    #[arg(long, global = true)]
    pretty: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
// `Resolve` carries the most flags, so it's the largest variant. The CLI parses
// exactly one command per run, so the size difference is irrelevant here.
#[allow(clippy::large_enum_variant)]
enum Command {
    /// Resolve one identifier (or a record) to a canonical entity.
    Resolve(ResolveArgs),
    /// Show an entity and all its members by canonical id.
    Entity {
        /// Canonical id (e.g. cx_1a2b3c4d).
        id: String,
    },
    /// Load seed record(s) into the graph (file or directory of *.json).
    Ingest {
        /// A record file or a directory of record files.
        path: PathBuf,
    },
}

#[derive(Args)]
#[command(group(
    ArgGroup::new("source")
        .required(true)
        .multiple(false)
        .args(["domain", "phone", "place_id", "imdb", "id", "input", "name"])
))]
struct ResolveArgs {
    #[arg(long)]
    domain: Option<String>,
    #[arg(long)]
    phone: Option<String>,
    #[arg(long = "place-id")]
    place_id: Option<String>,
    #[arg(long)]
    imdb: Option<String>,

    /// Generic identifier as `KIND:VALUE` (e.g. `yelp:blue-bottle-coffee`).
    /// Works for ANY registered kind — no per-kind CLI code required.
    #[arg(long)]
    id: Option<String>,

    #[arg(long)]
    input: Option<PathBuf>,

    /// Resolve a place by name (+ address/city). Reverse-resolves to a Placekey
    /// anchor and a Google place_id; requires --complete.
    #[arg(long)]
    name: Option<String>,
    /// Street address for --name (a full address yields a precise Placekey).
    #[arg(long)]
    address: Option<String>,
    /// City for --name (a name+city query is coarse → low confidence).
    #[arg(long)]
    city: Option<String>,
    /// Region/state for --name.
    #[arg(long)]
    region: Option<String>,
    /// ISO country code for --name (e.g. `US`).
    #[arg(long)]
    country: Option<String>,

    /// Optional: harvest extra sameAs from a domain's page HTML fixture (offline
    /// enrichment). Without it, --domain is a plain graph key lookup.
    #[arg(long)]
    fixture: Option<PathBuf>,

    /// Optional: fetch the domain's page over HTTP to harvest extra sameAs
    /// (opt-in; requires building with `--features live-fetch`). Never implicit.
    #[arg(long)]
    fetch: bool,

    /// Bootstrap missing edges from external hubs (Wikidata, TMDb, Google
    /// Places, Placekey). Off by default — pure local-graph resolution.
    #[arg(long)]
    complete: bool,

    /// Serve hub responses from a directory of canned JSON fixtures (offline,
    /// deterministic). Without it, --complete goes live (needs --features
    /// live-fetch + API-key env vars).
    #[arg(long = "hub-fixtures")]
    hub_fixtures: Option<PathBuf>,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let graph = Graph::open(&cli.db).with_context(|| format!("opening db {}", cli.db))?;

    match &cli.command {
        Command::Resolve(args) => {
            let out = do_resolve(&graph, args)?;
            print_output(&out, cli.json, "resolve");
        }
        Command::Entity { id } => {
            let out = load_entity(&graph, id)?;
            print_output(&out, cli.json, "entity");
        }
        Command::Ingest { path } => {
            let outs = do_ingest(&graph, path)?;
            for out in &outs {
                print_output(out, cli.json, "ingest");
            }
        }
    }
    Ok(())
}

fn do_resolve(graph: &Graph, args: &ResolveArgs) -> Result<ResolveOutput> {
    // Name/address is a reverse-resolution path: it needs the hubs.
    if let Some(name) = &args.name {
        if !args.complete {
            bail!("--name requires --complete (name/address is resolved via hub reverse-resolvers)");
        }
        let query = PlaceQuery {
            name: Some(name.clone()),
            street: args.address.clone(),
            city: args.city.clone(),
            region: args.region.clone(),
            country: args.country.clone(),
            entity_type: None,
        };
        let ctx = build_completion_ctx(args)?;
        return complete_place_query(graph, &query, &ctx);
    }

    // Build an EntityRecord from whichever typed source was given.
    let record = build_input_record(graph, args)?;

    if args.complete {
        let ctx = build_completion_ctx(args)?;
        return resolve_and_complete(graph, &record, &ctx);
    }
    commit_record(graph, &record)
}

/// Build the input record for a non-name source. `--domain` without page
/// harvesting is just a single domain key; everything else is a one-id record
/// (or a harvested/loaded record).
fn build_input_record(_graph: &Graph, args: &ResolveArgs) -> Result<EntityRecord> {
    if let Some(domain) = &args.domain {
        if args.fixture.is_some() || args.fetch {
            let resolver = build_domain_resolver(domain, args.fixture.as_deref(), args.fetch)?;
            return resolver.harvest();
        }
        return Ok(one_id(ExternalId::domain(domain)?));
    }
    if let Some(phone) = &args.phone {
        return Ok(one_id(ExternalId::phone(phone)?));
    }
    if let Some(place_id) = &args.place_id {
        return Ok(one_id(ExternalId::google_place_id(place_id)?));
    }
    if let Some(imdb) = &args.imdb {
        return Ok(one_id(ExternalId::imdb(imdb)?));
    }
    if let Some(id) = &args.id {
        // Generic path: KIND:VALUE, dispatched through the registry. No
        // per-kind CLI code — new kinds work here for free.
        let (tag, value) = id
            .split_once(':')
            .ok_or_else(|| anyhow!("--id must be KIND:VALUE, e.g. yelp:blue-bottle; got {id:?}"))?;
        return Ok(one_id(ExternalId::new(tag, value)?));
    }
    if let Some(input) = &args.input {
        return EntityRecord::from_path(input);
    }
    bail!("no resolve input provided");
}

fn one_id(id: ExternalId) -> EntityRecord {
    EntityRecord {
        same_as: vec![id],
        ..Default::default()
    }
}

/// Build the completion context: offline fixtures (`--hub-fixtures`) or live.
fn build_completion_ctx(args: &ResolveArgs) -> Result<CompletionCtx> {
    if let Some(dir) = &args.hub_fixtures {
        let transport = FixtureTransport::from_dir(dir)?;
        return Ok(CompletionCtx::new(Arc::new(transport)));
    }
    build_live_completion_ctx()
}

#[cfg(feature = "live-fetch")]
fn build_live_completion_ctx() -> Result<CompletionCtx> {
    let transport = sameas_core::transport::ReqwestTransport::new()?;
    let mut ctx = CompletionCtx::new(Arc::new(transport));
    ctx.tmdb_key = std::env::var("TMDB_API_KEY").unwrap_or_default();
    ctx.google_key = std::env::var("GOOGLE_PLACES_API_KEY").unwrap_or_default();
    ctx.placekey_key = std::env::var("PLACEKEY_API_KEY").unwrap_or_default();
    Ok(ctx)
}

#[cfg(not(feature = "live-fetch"))]
fn build_live_completion_ctx() -> Result<CompletionCtx> {
    bail!("--complete without --hub-fixtures requires building with --features live-fetch");
}

#[cfg(feature = "live-fetch")]
fn build_domain_resolver(
    domain: &str,
    fixture: Option<&Path>,
    fetch: bool,
) -> Result<DomainResolver> {
    match (fixture, fetch) {
        (Some(path), _) => DomainResolver::from_fixture(domain, path),
        (None, true) => DomainResolver::from_live(domain),
        (None, false) => bail!("no fixture or --fetch given for --domain harvest"),
    }
}

#[cfg(not(feature = "live-fetch"))]
fn build_domain_resolver(
    domain: &str,
    fixture: Option<&Path>,
    fetch: bool,
) -> Result<DomainResolver> {
    if let Some(path) = fixture {
        return DomainResolver::from_fixture(domain, path);
    }
    if fetch {
        bail!("--fetch requires building with --features live-fetch");
    }
    bail!("no fixture or --fetch given for --domain harvest");
}

fn do_ingest(graph: &Graph, path: &Path) -> Result<Vec<ResolveOutput>> {
    let mut files: Vec<PathBuf> = Vec::new();
    if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("json") {
                files.push(p);
            }
        }
        files.sort();
    } else {
        files.push(path.to_path_buf());
    }

    let mut outs = Vec::new();
    for file in files {
        let record = EntityRecord::from_path(&file)
            .with_context(|| format!("ingesting {}", file.display()))?;
        let resolver = DirectRecordResolver::new(record);
        let record = resolver.harvest()?;
        outs.push(commit_record(graph, &record)?);
    }
    Ok(outs)
}

// -------------------------------------------------------------------------
// Output rendering
// -------------------------------------------------------------------------

fn print_output(out: &ResolveOutput, json: bool, action: &str) {
    if json {
        println!("{}", to_json(out, action));
    } else {
        print_pretty(out, action);
    }
}

fn to_json(out: &ResolveOutput, action: &str) -> String {
    let same_as: Vec<String> = out.same_as.iter().map(|i| i.key()).collect();
    let provenance: serde_json::Map<String, serde_json::Value> = out
        .provenance
        .iter()
        .map(|(key, source)| {
            (
                key.clone(),
                serde_json::Value::String(source.clone().unwrap_or_else(|| "unknown".into())),
            )
        })
        .collect();
    let value = serde_json::json!({
        "action": action,
        "canonical_id": out.canonical_id,
        "anchor": out.anchor,
        "type": out.entity_type,
        "name": out.name,
        "status": out.status.as_str(),
        "confidence": out.confidence,
        "matched_via": out.matched_via,
        "sameAs": same_as,
        "provenance": provenance,
        "completion_count": same_as.len(),
        "harvested": out.harvested,
        "new_edges": out.new_edges,
    });
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_string())
}

fn print_pretty(out: &ResolveOutput, action: &str) {
    let status_word = match out.status {
        Status::New => "new entity",
        Status::Hit => "hit",
    };
    let matched = if out.matched_via.is_empty() {
        "-".to_string()
    } else {
        out.matched_via.join(", ")
    };

    println!("  {status_word:<12} {}", out.canonical_id);
    println!("  {:<12} {}", "anchor:", out.anchor);
    if let Some(t) = &out.entity_type {
        println!("  {:<12} {}", "type:", t);
    }
    if let Some(n) = &out.name {
        println!("  {:<12} {}", "name:", n);
    }
    println!("  {:<12} {:.2}", "confidence:", out.confidence);
    println!("  {:<12} {}", "matched_via:", matched);
    println!(
        "  {:<12} {} identifiers",
        "completion:",
        out.same_as.len()
    );
    if action != "entity" {
        println!(
            "  {:<12} harvested {}, {} new edge(s)",
            "graph:", out.harvested, out.new_edges
        );
    }
    println!("  sameAs:");
    for id in &out.same_as {
        let key = id.key();
        let source = out
            .provenance
            .iter()
            .find(|(k, _)| k == &key)
            .and_then(|(_, s)| s.as_deref());
        match source {
            Some(src) => println!("      - {key}  [{src}]"),
            None => println!("      - {key}"),
        }
    }
    println!();
}
