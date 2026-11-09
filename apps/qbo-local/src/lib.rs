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
//! - [`reconcile`]— §7: the verification sweep against QBO's own index
//! - [`clock`]    — `HANDOFF.md` §2.5: a testable source of time and waiting
//! - [`daemon`]   — `HANDOFF.md` §2.5, `DESIGN.md` §8: the CDC poll loop and
//!   nightly snapshots
//! - [`fixture`]  — `DESIGN.md` §10, `HANDOFF.md` §2.6: record/replay HTTP
//!   fixtures so the suite runs offline
//! - [`http`]     — `HANDOFF.md` §2.3: [`client::QboClient`] over real HTTP
//! - [`oauth`]    — `HANDOFF.md` §2.2: the authorization-code loopback flow
//! - [`config`]   — `.local/config.toml`, what `--live` reads to build an
//!   [`http::HttpQboClient`]
//! - [`mcp`]      — `ROADMAP.md` §G: an MCP server over the read-only query
//!   API, so Dan's existing skills can be repointed off the QuickBooks MCP
//! - [`reports`]  — `LEDGER-DESIGN.md` §6, §7: parsing the Reports API's
//!   JSON into trial-balance and aging rows, and the CSV `ledger` reads
//! - [`slice`]    — `HANDOFF.md` §2.6, D31: a small, referentially closed
//!   slice of the replica, recorded as fixtures for the scrubber

pub mod api;
pub mod auth;
pub mod budget;
pub mod client;
pub mod clock;
pub mod compose;
pub mod config;
pub mod daemon;
pub mod demo;
pub mod domain;
pub mod mfg;
pub mod driver;
pub mod fixture;
pub mod http;
#[cfg(target_os = "macos")]
pub mod keychain;
pub mod mcp;
pub mod mfg_import;
pub mod oauth;
pub mod outbox;
pub mod project;
pub mod ratelimit;
pub mod reconcile;
pub mod reports;
pub mod sales_orders;
pub mod slice;
pub mod store;
pub mod safety;
pub mod serve;
pub mod sync;
pub mod worker;
pub mod writes;
