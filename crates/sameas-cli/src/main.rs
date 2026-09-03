//! `sameas` CLI — a thin front-end over `sameas-core`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use clap::{ArgGroup, Args, Parser, Subcommand};

use sameas_core::{
    commit_record, link, load_entity, merge, name_not_found, resolve_and_complete, resolve_name,
    resolve_name_local, split, CompletionCtx, DirectRecordResolver, DomainResolver, EntityRecord,
    ExternalId, FixtureTransport, GraphStore, LinkOutcome, NameQuery, Resolver, ResolveOutput,
    SqliteStore, StatsReport, Status,
};
use sameas_core::confidence::reason_tag;

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
    /// Assert two identifiers are the same entity (create/attach/merge as needed).
    Link {
        /// First key as KIND:VALUE (e.g. google_place_id:ChIJ...).
        a: String,
        /// Second key as KIND:VALUE.
        b: String,
        /// Override the same-kind identity-conflict guard.
        #[arg(long)]
        force: bool,
    },
    /// Combine two or more entities into one, keeping the strongest anchor.
    Merge {
        /// Canonical ids to merge (2+).
        #[arg(required = true, num_args = 2..)]
        ids: Vec<String>,
        /// Override the same-kind identity-conflict guard.
        #[arg(long)]
        force: bool,
    },
    /// Detach one or more strong keys onto a fresh entity (undo a bad merge).
    Split {
        /// The key to detach as KIND:VALUE.
        key: String,
        /// Additional keys to detach with it (repeatable).
        #[arg(long = "with")]
        with: Vec<String>,
    },
    /// Report the resolution miss rate (exact / hub / miss breakdown).
    Stats,
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

    /// Resolve an entity by name (+ qualifiers). Reverse-resolves through the hub
    /// its --type routes to; requires --complete to reach out.
    #[arg(long)]
    name: Option<String>,
    /// Entity type for --name, as an NSID leaf (matched case-insensitively, and a
    /// full `info.cursive.creativeWork.movie` is accepted too). It picks the hub:
    /// `place`/`localBusiness`/`foodEstablishment`/`restaurant` → Google Places
    /// (the only BILLABLE hub); `movie`/`tvSeries` → TMDb; anything else, or no
    /// --type at all → Wikidata. Applies ONLY to --name.
    #[arg(long = "type")]
    entity_type: Option<String>,
    /// Street address for --name (a full address yields a precise Placekey).
    /// Applies ONLY to --name; combining it with a typed source is rejected.
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
    /// Generic disambiguating qualifier for --name; repeatable. Type-agnostic —
    /// a city, state, borough, year, etc. (e.g. `--qualifier Brooklyn`,
    /// `--qualifier 1999`). Used to match/cache by name locally.
    #[arg(long = "qualifier")]
    qualifiers: Vec<String>,

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

/// Current-thread runtime: the storage trait is `#[async_trait(?Send)]` (see
/// `sameas_core::store`), and the CLI is a single sequential resolve — there is no
/// concurrency to exploit, so a work-stealing pool would only add a `Send` bound we
/// deliberately don't want.
#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let graph = SqliteStore::open(&cli.db).with_context(|| format!("opening db {}", cli.db))?;

    match &cli.command {
        Command::Resolve(args) => {
            let out = do_resolve(&graph, args).await?;
            // Log the outcome for `sameas stats` (best-effort — a logging failure
            // must never fail the resolution). `entity` and `ingest` are excluded:
            // a direct id lookup and a seed load are not user-facing *queries*, so
            // counting them would skew the miss rate.
            record_outcome(&graph, &out, &describe_resolve(args)).await;
            print_output(&out, cli.json, "resolve");
        }
        Command::Entity { id } => {
            let out = load_entity(&graph, id).await?;
            print_output(&out, cli.json, "entity");
        }
        Command::Ingest { path } => {
            do_ingest(&graph, path, cli.json).await?;
        }
        Command::Link { a, b, force } => {
            let outcome = link(&graph, a, b, *force).await?;
            print_link(&outcome, cli.json);
        }
        Command::Merge { ids, force } => {
            let winner = merge(&graph, ids, *force).await?;
            print_correction("merge", &winner, cli.json);
        }
        Command::Split { key, with } => {
            let mut keys = vec![key.clone()];
            keys.extend(with.iter().cloned());
            let new_cid = split(&graph, &keys).await?;
            print_correction("split", &new_cid, cli.json);
        }
        Command::Stats => {
            let report = graph.stats().await?;
            print_stats(&report, cli.json);
        }
    }
    Ok(())
}

/// Log a resolution outcome to the stats table, best-effort. Silently ignores
/// errors: instrumentation must never break the user's resolution.
async fn record_outcome(graph: &dyn GraphStore, out: &ResolveOutput, input_desc: &str) {
    let _ = graph.record_resolution(
        out.status.as_str(),
        reason_tag(&out.confidence_reason),
        out.matched_via.first().map(|s| s.as_str()),
        out.confidence,
        Some(input_desc),
    ).await;
}

/// A short descriptor of what was resolved, stored with the outcome so the
/// future fuzzy-phase gate can slice actual missed inputs. IDs only — no
/// provider content.
fn describe_resolve(args: &ResolveArgs) -> String {
    if let Some(d) = &args.domain {
        format!("domain:{d}")
    } else if args.phone.is_some() {
        "phone".to_string()
    } else if let Some(p) = &args.place_id {
        format!("google_place_id:{p}")
    } else if let Some(i) = &args.imdb {
        format!("imdb:{i}")
    } else if let Some(id) = &args.id {
        id.clone()
    } else if args.name.is_some() {
        // IDs only, no provider content — the type is a routing key, not content.
        match &args.entity_type {
            Some(t) => format!("name:{t}"),
            None => "name".to_string(),
        }
    } else if args.input.is_some() {
        "record".to_string()
    } else {
        "unknown".to_string()
    }
}

async fn do_resolve(graph: &dyn GraphStore, args: &ResolveArgs) -> Result<ResolveOutput> {
    // Geo/qualifier facets apply ONLY to the --name reverse-resolution path.
    // Combined with a typed source they would be silently ignored, so reject.
    if args.name.is_none() {
        let mut offenders: Vec<&str> = Vec::new();
        if args.address.is_some() {
            offenders.push("--address");
        }
        if args.city.is_some() {
            offenders.push("--city");
        }
        if args.region.is_some() {
            offenders.push("--region");
        }
        if args.country.is_some() {
            offenders.push("--country");
        }
        if !args.qualifiers.is_empty() {
            offenders.push("--qualifier");
        }
        if args.entity_type.is_some() {
            offenders.push("--type");
        }
        if !offenders.is_empty() {
            bail!(
                "{} appl{} only to --name; combine with --name or drop {}",
                offenders.join(", "),
                if offenders.len() == 1 { "ies" } else { "y" },
                if offenders.len() == 1 { "it" } else { "them" }
            );
        }
    }

    // Name/address is a reverse-resolution path: it needs the hubs.
    if let Some(name) = &args.name {
        if name.trim().is_empty() {
            bail!("--name must not be empty");
        }
        let query = NameQuery {
            name: Some(name.clone()),
            qualifiers: args.qualifiers.clone(),
            entity_type: args.entity_type.clone(),
            street: args.address.clone(),
            city: args.city.clone(),
            region: args.region.clone(),
            country: args.country.clone(),
        };
        // With --complete, resolve graph-first then reach out to hubs on a miss.
        // Without it, do a local-only lookup (zero network): a hit is served from
        // the graph; a miss says "re-run with --complete".
        if args.complete {
            let ctx = build_completion_ctx(args)?;
            return resolve_name(graph, &query, &ctx).await;
        }
        return Ok(resolve_name_local(graph, &query).await?.unwrap_or_else(|| name_not_found(&query)));
    }

    // Build an EntityRecord from whichever typed source was given.
    let record = build_input_record(graph, args).await?;

    if args.complete {
        let ctx = build_completion_ctx(args)?;
        // An ORGANIZATION identified only by its homepage: a registrable domain is
        // Affiliation grain, so on its own it mints a brand-level entity and learns
        // nothing. `resolve_by_website` asks Wikidata who publishes the site and
        // resolves on the QID instead. Guarded to org-shaped types inside — a
        // restaurant's chain domain must never crosswalk. Kept here, and not inside
        // `resolve_and_complete`, for the same reason the other reverse-resolvers
        // are entry-point only: running it speculatively over every domain in a
        // cluster is how a movie's website acquires a studio's QID.
        if let (Some(domain), true) = (
            &args.domain,
            sameas_core::org_shaped(args.entity_type.as_deref()),
        ) {
            let query = NameQuery {
                name: args.name.clone(),
                entity_type: args.entity_type.clone(),
                ..Default::default()
            };
            let id = ExternalId::domain(domain)?;
            let mut hub_error = None;
            if let Some(out) =
                sameas_core::resolve_by_website(graph, &id, &query, &ctx, &mut hub_error).await?
            {
                return Ok(out);
            }
        }
        return resolve_and_complete(graph, &record, &ctx).await;
    }
    commit_record(graph, &record).await
}

/// Build the input record for a non-name source. `--domain` without page
/// harvesting is just a single domain key; everything else is a one-id record
/// (or a harvested/loaded record).
async fn build_input_record(_graph: &dyn GraphStore, args: &ResolveArgs) -> Result<EntityRecord> {
    if let Some(domain) = &args.domain {
        if args.fixture.is_some() || args.fetch {
            let resolver =
                build_domain_resolver(domain, args.fixture.as_deref(), args.fetch).await?;
            return resolver.harvest().await;
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
async fn build_domain_resolver(
    domain: &str,
    fixture: Option<&Path>,
    fetch: bool,
) -> Result<DomainResolver> {
    match (fixture, fetch) {
        (Some(path), _) => DomainResolver::from_fixture(domain, path),
        (None, true) => DomainResolver::from_live(domain).await,
        (None, false) => bail!("no fixture or --fetch given for --domain harvest"),
    }
}

#[cfg(not(feature = "live-fetch"))]
async fn build_domain_resolver(
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

/// Ingest one record file into the graph and return the resolution output.
async fn ingest_one(graph: &dyn GraphStore, file: &Path) -> Result<ResolveOutput> {
    let record = EntityRecord::from_path(file)
        .with_context(|| format!("ingesting {}", file.display()))?;
    let resolver = DirectRecordResolver::new(record);
    let record = resolver.harvest().await?;
    commit_record(graph, &record).await
}

/// Ingest a single file or a directory of `*.json` records.
///
/// A single file passed directly errors loudly (any parse/commit failure aborts
/// with a non-zero exit). A directory is processed **resiliently and atomically
/// per file**: subdirectories and non-`.json` entries are skipped, each file is
/// committed independently, and a failing file is recorded and the batch
/// continues rather than aborting mid-way. A summary is always printed; if any
/// file failed, the failures are listed and the process exits non-zero (but the
/// good records are already committed — never a silent half-done batch).
async fn do_ingest(graph: &dyn GraphStore, path: &Path, json: bool) -> Result<()> {
    if !path.is_dir() {
        // Single file: error loudly on any failure (unchanged behavior).
        let out = ingest_one(graph, path).await?;
        print_output(&out, json, "ingest");
        return Ok(());
    }

    // Directory: only *.json *files* (a subdir literally named `x.json` is not a
    // record and must be skipped, not treated as a file).
    let mut files: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let p = entry.path();
        if p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("json") {
            files.push(p);
        }
    }
    files.sort();

    if files.is_empty() {
        println!("0 records ingested from {}", path.display());
        return Ok(());
    }

    let mut ingested = 0usize;
    let mut failures: Vec<(PathBuf, String)> = Vec::new();
    for file in &files {
        match ingest_one(graph, file).await {
            Ok(out) => {
                print_output(&out, json, "ingest");
                ingested += 1;
            }
            Err(err) => failures.push((file.clone(), format!("{err:#}"))),
        }
    }

    println!(
        "ingested {}, skipped/failed {} (from {})",
        ingested,
        failures.len(),
        path.display()
    );
    for (file, err) in &failures {
        eprintln!("  failed: {} — {}", file.display(), err);
    }
    if !failures.is_empty() {
        bail!(
            "{} of {} file(s) failed to ingest",
            failures.len(),
            files.len()
        );
    }
    Ok(())
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

/// Round a confidence to 2 decimals for display (f32 → clean f64).
/// Serialize a resolution for `--json`. The document shape lives in
/// `sameas_core::json` so the CLI and the HTTP Worker cannot drift apart.
fn to_json(out: &ResolveOutput, action: &str) -> String {
    let value = sameas_core::json::resolve_output_json(out, action);
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_string())
}

fn print_pretty(out: &ResolveOutput, action: &str) {
    let status_word = match out.status {
        Status::New => "new entity",
        Status::Hit => "hit",
        Status::Unresolved => "unresolved",
    };
    let matched = if out.matched_via.is_empty() {
        "-".to_string()
    } else {
        out.matched_via.join(", ")
    };
    let id_display = out.canonical_id.as_deref().unwrap_or("(unresolved)");

    println!("  {status_word:<12} {id_display}");
    if !out.anchor.is_empty() {
        println!("  {:<12} {}", "anchor:", out.anchor);
    }
    if let Some(t) = &out.entity_type {
        println!("  {:<12} {}", "type:", t);
    }
    if let Some(n) = &out.name {
        println!("  {:<12} {}", "name:", n);
    }
    println!(
        "  {:<12} {:.2} ({})",
        "confidence:",
        out.confidence,
        reason_tag(&out.confidence_reason)
    );
    println!("  {:<12} {}", "matched_via:", matched);
    if let Some(hint) = &out.hint {
        println!("  {:<12} {}", "hint:", hint);
    }
    // Printed for the same reason it is on the wire: an empty candidate list that
    // came from a 403 must not look like an empty candidate list that came from
    // the hub. `--json` carries it too.
    if let Some(err) = &out.hub_error {
        println!("  {:<12} {}", "hub_error:", err);
    }
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
    if !out.candidates.is_empty() {
        println!("  candidates:  (supply a stronger identifier to disambiguate)");
        for c in &out.candidates {
            let who = if c.canonical_id.is_empty() {
                c.anchor.clone()
            } else {
                c.canonical_id.clone()
            };
            match &c.name {
                Some(n) => println!("      - {who}  ({n})"),
                None => println!("      - {who}"),
            }
        }
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

// -------------------------------------------------------------------------
// Correction + stats rendering
// -------------------------------------------------------------------------

fn print_link(outcome: &LinkOutcome, json: bool) {
    let (verb, cid) = match outcome {
        LinkOutcome::Created(c) => ("created", c),
        LinkOutcome::Attached(c) => ("attached", c),
        LinkOutcome::AlreadyLinked(c) => ("already_linked", c),
        LinkOutcome::Merged(c) => ("merged", c),
    };
    if json {
        let value = serde_json::json!({
            "action": "link",
            "outcome": verb,
            "canonical_id": cid,
        });
        println!("{}", serde_json::to_string_pretty(&value).unwrap());
    } else {
        println!("  {:<12} {} ({})", "link:", cid, verb);
    }
}

fn print_correction(action: &str, cid: &str, json: bool) {
    if json {
        let value = serde_json::json!({
            "action": action,
            "canonical_id": cid,
        });
        println!("{}", serde_json::to_string_pretty(&value).unwrap());
    } else {
        println!("  {:<12} {}", format!("{action}:"), cid);
    }
}

/// The miss rate as shown to the user: rounded ONCE (half-away-from-zero, via
/// `round2`) so JSON and the pretty "miss rate:" line always show the same value.
/// Previously JSON used `round2` while pretty used `{:.2}`'s banker's rounding,
/// so identical data could render as 0.12 vs 0.13.
fn display_miss_rate(report: &StatsReport) -> f64 {
    sameas_core::json::round2(report.miss_rate() as f32)
}

fn print_stats(report: &StatsReport, json: bool) {
    // Reuse the single rounded miss rate everywhere; the "miss:" bucket
    // percentage derives from it too, so the bucket line and the "miss rate:"
    // line can never disagree.
    let miss_rate = display_miss_rate(report);
    if json {
        let by_reason: serde_json::Map<String, serde_json::Value> = report
            .by_reason
            .iter()
            .map(|(tag, n)| (tag.clone(), serde_json::json!(n)))
            .collect();
        let value = serde_json::json!({
            "action": "stats",
            "total": report.total,
            "exact": report.exact,
            "hub": report.hub,
            "miss": report.miss,
            "miss_rate": miss_rate,
            "by_reason": by_reason,
            "entities": report.entities,
            "edges": report.edges,
        });
        println!("{}", serde_json::to_string_pretty(&value).unwrap());
        return;
    }

    let pct = |n: usize| -> f64 {
        if report.total == 0 {
            0.0
        } else {
            (n as f64 / report.total as f64 * 100.0).round()
        }
    };
    println!("  resolutions logged: {}", report.total);
    println!(
        "  {:<10} {:>6}  {:>4}%   answered from an exact key",
        "exact:",
        report.exact,
        pct(report.exact)
    );
    println!(
        "  {:<10} {:>6}  {:>4}%   required a hub lookup",
        "hub:",
        report.hub,
        pct(report.hub)
    );
    // The miss percentage derives from the single rounded miss rate (not an
    // independent `pct(miss)`), so the bucket line and the "miss rate:" line
    // below always agree.
    println!(
        "  {:<10} {:>6}  {:>4}%   unresolved (the miss set)",
        "miss:",
        report.miss,
        (miss_rate * 100.0).round()
    );
    println!(
        "  {:<10} {:>6.2}",
        "miss rate:",
        miss_rate
    );
    if !report.by_reason.is_empty() {
        println!("  by reason:");
        for (tag, n) in &report.by_reason {
            println!("      {tag:<28} {n:>6}");
        }
    }
    println!(
        "  graph: {} entities, {} edges",
        report.entities, report.edges
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn miss_rate_display_is_consistent_across_json_and_pretty() {
        // 1/8 = 0.125: a value where round2 (half-away-from-zero → 0.13) and
        // `{:.2}`'s banker's rounding (→ 0.12) used to disagree. Now both the
        // JSON `miss_rate` and the pretty "miss rate:" line use the SAME rounded
        // value, and the "miss:" bucket percentage derives from it.
        let report = StatsReport {
            total: 8,
            exact: 5,
            hub: 2,
            miss: 1,
            by_reason: vec![("needs_stronger_identifier".into(), 1)],
            entities: 3,
            edges: 4,
        };
        let shown = display_miss_rate(&report);
        // Single rounded value, half-away-from-zero.
        assert_eq!(shown, 0.13);
        // The pretty "miss:" bucket percentage derives from the same value.
        assert_eq!((shown * 100.0).round(), 13.0);
        // JSON serializes the identical rounded value (not a re-rounding).
        let json = serde_json::json!({ "miss_rate": shown });
        assert_eq!(json["miss_rate"], serde_json::json!(0.13));
    }
}
