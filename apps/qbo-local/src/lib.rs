//! `qbo-local` — a local replica of QuickBooks Online plus a fast UI over it.
//!
//! QBO remains the book of record. This crate's job is to keep a complete,
//! current copy on local disk so that no read ever touches the network, and to
//! queue writes durably so that no write ever blocks the UI on Intuit.
//!
//! See `DESIGN.md` at the repository root. Module layout follows its sections:
//!
//! - [`api`]      — the UI's read-only command surface (M1)
//! - [`demo`]     — §11: a synthetic book for the UI and benchmarks
//! - [`compose`]  — drafts typed in the UI to QBO payloads
//! - [`mfg`]      — manufacturing costing: landed cost, build sheets, builds
//! - [`mfg_import`] — CSV import of materials, build sheets, counts
//! - [`domain`]   — §2: realm scoping, entity types
//! - [`store`]    — §3: replica schema and migrations
//! - [`project`]  — §3.2: raw payloads to the parsed projection
//! - [`safety`]   — §8: write audit log and dated replica snapshots
//! - [`serve`]    — loopback dev server over [`api`] for browser testing
//! - [`sync`]     — §4: CDC cursor strategy and its failure modes
//! - [`driver`]   — §4: the loop that runs them against a client and a store
//! - [`auth`]     — §5: token rotation and durable persistence
//! - [`ratelimit`]— §4.4: per-realm token buckets
//! - [`outbox`]   — §6: the write queue and its state machine
//! - [`client`]   — the QBO API boundary, plus an in-memory double
//! - [`http`]     — the HTTP implementation of that boundary
//! - [`keychain`] — §5.1: the macOS keychain token backend
//! - [`oauth`]    — §5: authorisation, refresh, rotation against Intuit
//! - [`writes`]   — §6/§8: queue locally, drain to QBO, the write gate
//! - [`worker`]   — §6.3-6.5: draining the outbox, dependency resolution

pub mod api;
pub mod auth;
pub mod client;
pub mod compose;
pub mod demo;
pub mod domain;
pub mod mfg;
pub mod driver;
pub mod http;
pub mod keychain;
pub mod mfg_import;
pub mod oauth;
pub mod outbox;
pub mod project;
pub mod ratelimit;
pub mod store;
pub mod safety;
pub mod serve;
pub mod sync;
pub mod worker;
pub mod writes;
