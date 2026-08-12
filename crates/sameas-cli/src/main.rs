//! `sameas` CLI — a thin front-end over `sameas-core`.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use clap::{ArgGroup, Args, Parser, Subcommand};

use sameas_core::{
    commit_record, load_entity, resolve_id, DirectRecordResolver, DomainResolver, EntityRecord,
    ExternalId, Graph, Resolver, ResolveOutput, Status,
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
        .args(["domain", "phone", "place_id", "imdb", "id", "input"])
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

    /// Optional: harvest extra sameAs from a domain's page HTML fixture (offline
    /// enrichment). Without it, --domain is a plain graph key lookup.
    #[arg(long)]
    fixture: Option<PathBuf>,

    /// Optional: fetch the domain's page over HTTP to harvest extra sameAs
    /// (opt-in; requires building with `--features live-fetch`). Never implicit.
    #[arg(long)]
    fetch: bool,
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
    if let Some(domain) = &args.domain {
        // Page-harvesting is strictly opt-in. By default a domain is just a key.
        if args.fixture.is_some() || args.fetch {
            let resolver = build_domain_resolver(domain, args.fixture.as_deref(), args.fetch)?;
            let record = resolver.harvest()?;
            return commit_record(graph, &record);
        }
        // Default: pure graph lookup / mint — no HTML, no network.
        return resolve_id(graph, ExternalId::domain(domain)?);
    }
    if let Some(phone) = &args.phone {
        return resolve_id(graph, ExternalId::phone(phone)?);
    }
    if let Some(place_id) = &args.place_id {
        return resolve_id(graph, ExternalId::google_place_id(place_id)?);
    }
    if let Some(imdb) = &args.imdb {
        return resolve_id(graph, ExternalId::imdb(imdb)?);
    }
    if let Some(id) = &args.id {
        // Generic path: KIND:VALUE, dispatched through the registry. No
        // per-kind CLI code — new kinds work here for free.
        let (tag, value) = id
            .split_once(':')
            .ok_or_else(|| anyhow!("--id must be KIND:VALUE, e.g. yelp:blue-bottle; got {id:?}"))?;
        return resolve_id(graph, ExternalId::new(tag, value)?);
    }
    if let Some(input) = &args.input {
        let record = EntityRecord::from_path(input)?;
        return commit_record(graph, &record);
    }
    bail!("no resolve input provided");
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
    let value = serde_json::json!({
        "action": action,
        "canonical_id": out.canonical_id,
        "anchor": out.anchor,
        "type": out.entity_type,
        "name": out.name,
        "status": out.status.as_str(),
        "matched_via": out.matched_via,
        "sameAs": same_as,
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
        println!("      - {}", id.key());
    }
    println!();
}
