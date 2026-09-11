//! `qbo-local` — a local replica of QuickBooks Online plus a fast UI over it.
//!
//! QBO remains the book of record. This crate's job is to keep a complete,
//! current copy on local disk so that no read ever touches the network, and to
//! queue writes durably so that no write ever blocks the UI on Intuit.
//!
//! See `DESIGN.md` at the repository root. Module layout follows its sections:
//!
//! - [`domain`]   — §2: realm scoping, entity types
//! - [`store`]    — §3: replica schema and migrations
//! - [`project`]  — §3.2: raw payloads to the parsed projection
//! - [`sync`]     — §4: CDC cursor strategy and its failure modes
//! - [`driver`]   — §4: the loop that runs them against a client and a store
//! - [`auth`]     — §5: token rotation and durable persistence
//! - [`ratelimit`]— §4.4: per-realm token buckets
//! - [`outbox`]   — §6: the write queue and its state machine
//! - [`client`]   — the QBO API boundary, plus an in-memory double
//! - [`worker`]   — §6.3-6.5: draining the outbox, dependency resolution
//! - [`clock`]    — `HANDOFF.md` §2.5: a testable source of time and waiting
//! - [`daemon`]   — `HANDOFF.md` §2.5, `DESIGN.md` §8: the CDC poll loop and
//!   nightly snapshots

pub mod auth;
pub mod client;
pub mod clock;
pub mod daemon;
pub mod domain;
pub mod driver;
pub mod outbox;
pub mod project;
pub mod ratelimit;
pub mod store;
pub mod sync;
pub mod worker;
