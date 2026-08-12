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
//! * [`graph`] — union-find over SQLite (ID-to-ID edges only).
//! * [`anchor`] — deterministic canonical-anchor selection.
//! * [`resolve`] — [`Resolver`] trait, adapters, and orchestration.

pub mod anchor;
pub mod graph;
pub mod kind;
pub mod model;
pub mod normalize;
pub mod resolve;

pub use graph::Graph;
pub use kind::{spec_for_tag, KindSpec, KINDS};
pub use model::{EntityRecord, ExternalId};
pub use resolve::{
    commit_record, load_entity, resolve_id, DirectRecordResolver, DomainResolver, Resolver,
    ResolveOutput, Status,
};
