//! `sameas-core` — the entity resolution & completion core.
//!
//! `sameas` resolves one partial identifier about an entity into a canonical id
//! plus the completed set of linked identifiers. The core is an **identity
//! crosswalk graph**: a union-find over typed external ids, persisted in SQLite.
//! Resolving an input attaches it to a cluster and returns the whole cluster —
//! resolve *is* completion.
//!
//! Modules:
//! * [`kind`] — the [`kind::KindSpec`] registry: the single source of truth for
//!   every identifier kind. Adding a kind = one entry + a normalizer.
//! * [`model`] — spec-backed [`ExternalId`] + [`EntityRecord`].
//! * [`normalize`] — URL→domain, phone→E.164, IMDb/QID/TMDb, Yelp slug.
//! * [`store`] — the [`GraphStore`] storage boundary (ID-to-ID edges only):
//!   [`SqliteStore`] for the CLI/tests, `store::d1::D1Store` for the Worker.
//! * [`anchor`] — deterministic canonical-anchor selection.
//! * [`resolve`] — [`Resolver`] trait, adapters, and orchestration.
//! * [`transport`] — async HTTP/JSON transport for hub adapters (offline fixture,
//!   native `reqwest`, and `worker::Fetch` for the Worker); see M2 hub
//!   bootstrapping.
//! * [`confidence`] — the `0.0`–`1.0` confidence gradient.

pub mod anchor;
pub mod complete;
pub mod confidence;
pub mod correct;
pub mod hubs;
pub mod json;
pub mod kind;
pub mod model;
pub mod normalize;
pub mod resolve;
pub mod store;
pub mod transport;

pub use complete::{
    name_not_found, resolve_and_complete, resolve_name, resolve_name_local, CompletionCtx,
    NameQuery,
};
pub use confidence::ConfidenceReason;
pub use correct::{link, merge, split, LinkOutcome};
pub use kind::{spec_for_tag, Grain, KindSpec, KINDS};
pub use model::{EntityRecord, ExternalId};
pub use resolve::{
    commit_record, commit_record_with_opts, commit_record_with_source, load_entity, resolve_id,
    Candidate, CommitOpts, DirectRecordResolver, Resolver, ResolveOutput, Status,
};
pub use store::{EntityRow, GraphStore, NameCardinality, StatsReport};
pub use transport::{FixtureTransport, HttpTransport};

// Backend-specific types stay behind their feature, mirroring how
// `ReqwestTransport` is reachable only via its module path. `D1Store` is likewise
// `sameas_core::store::d1::D1Store`.
#[cfg(feature = "sqlite")]
pub use store::SqliteStore;

#[cfg(feature = "harvest")]
pub use resolve::DomainResolver;
