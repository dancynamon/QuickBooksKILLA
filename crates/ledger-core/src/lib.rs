//! Shared money type and rounding policy.
//!
//! Scope is deliberately narrow — see `DECISIONS.md` D3. This crate holds the
//! two things that must not be reimplemented differently by `qbo-local` and the
//! future ledger project: how a monetary amount is represented, and how a
//! `Decimal` becomes one. QBO entity models and domain enums live in the app
//! that uses them, not here.
//!
//! No I/O. No SQLite. No QBO. Pure types and pure functions.

mod money;
mod rounding;

pub use money::{Money, MoneyError};
pub use rounding::{round_money, RoundingPolicy};
