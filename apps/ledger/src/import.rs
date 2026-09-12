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
pub use opening::{boundary_year, opening_balance_entry, TbAgreement};
pub use translate::{
    translate, ClassSource, ItemContext, TranslateContext, TranslateError, Translated,
};

/// `opening_balance_entry` and the §7 nightly diff (`crate::report`) both once
/// needed a signed, debit-positive QBO trial balance row keyed by account id;
/// `crate::report::QboTbRow` is the one surviving definition (D-note, this
/// commit: the two tracks that each grew one in parallel are unified here).
pub use crate::report::QboTbRow;

/// Read the replica's chart and class masters and materialise the ledger's
/// [`AccountMapping`] and [`ClassMapping`] against them, without walking any
/// documents. [`crate::pipeline::import_replica`] uses this to seed the
/// ledger's chart before posting a single entry; nothing else in this crate
/// needs the raw masters, so the fetch itself stays `pub(crate)` on
/// [`driver`].
pub(crate) use driver::{
    build_exempt_customers, build_item_contexts, parsed_accounts, parsed_classes,
};
