//! Replica to documents: chart mapping, class mapping, document translation,
//! opening balances. `LEDGER-DESIGN.md` §6; the chart it maps onto is §2, the
//! class taxonomy is §3.
//!
//! This module is a pure translator plus a driver, not a writer. It never
//! opens `crate::store` and never calls `crate::post` — those tracks are
//! built in parallel — so its output is [`translate::Translated`] documents
//! handed to a sink, nothing more. History in the ledger is derived by
//! feeding those documents through the same posting function every other
//! caller uses (`LEDGER-DESIGN.md` §6), which happens one layer up from here.
//!
//! - [`accounts`]  — §2: QBO accounts onto the chart
//! - [`classes`]   — §3, W15/W16: QBO classes onto the taxonomy
//! - [`translate`] — one document, header to lines, read from the projection
//!   plus whatever §6's table says is missing from it
//! - [`driver`]    — the walk over the replica and the report it produces
//! - [`opening`]   — the pre-boundary opening balance entry and the
//!   boundary-year walk

mod accounts;
mod classes;
mod driver;
mod opening;
mod translate;

pub use accounts::{map_accounts, AccountMapping, MappedAccount};
pub use classes::{map_classes, ClassMapping, MappedClass};
pub use driver::{run, ImportError, ImportOptions, ImportReport};
pub use opening::{boundary_year, opening_balance_entry, QboTbRow, TbAgreement};
pub use translate::{
    translate, ClassSource, ItemContext, TranslateContext, TranslateError, Translated,
};
