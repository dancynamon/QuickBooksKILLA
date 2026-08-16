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
//! - [`sync`]     — §4: CDC cursor strategy and its failure modes
//! - [`auth`]     — §5: token rotation and durable persistence
//! - [`ratelimit`]— §4.4: per-realm token buckets
//! - [`outbox`]   — §6: the write queue and its state machine

pub mod auth;
pub mod domain;
pub mod outbox;
pub mod ratelimit;
pub mod store;
pub mod sync;
