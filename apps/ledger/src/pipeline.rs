//! Import replica documents through [`crate::post::post`] and into
//! [`crate::store::Ledger`]. `LEDGER-DESIGN.md` §6.
//!
//! [`crate::import`] only ever translates — it never opens the store and
//! never calls the posting function, by design. [`crate::post`] only ever
//! turns one document into one entry — it never persists anything. This
//! module is the layer above both: it builds the chart and class mappings,
//! materialises them into the ledger, builds the [`PostingContext`] the
//! posting function needs, and then walks [`crate::import::run`] with a sink
//! that saves each translated document and posts it.
//!
//! **Re-run semantics** (§6: "Import runs are idempotent and re-runnable: the
//! same replica state produces the same ledger, and a re-run replaces the
//! derived entries rather than adding to them"). Before posting a document
//! whose `source_ref` already has *posted* entries in this ledger
//! ([`crate::store::Ledger::entries_for_document`]), this module compares the
//! freshly translated document against the payload of the latest saved
//! version ([`crate::store::Ledger::latest_document_payload`]), both read as
//! `serde_json::Value` so field order never produces a false difference:
//!
//! - **Identical**: the document is skipped entirely — no new
//!   `document_versions` row, nothing reversed, nothing reposted. Counted in
//!   [`PipelineReport::skipped_unchanged`].
//! - **Different**: every posted entry for the document is reversed as of the
//!   *new* document's `txn_date` (`Ledger::reverse_entry`), a new document
//!   version is saved, and the new entry is posted. Counted in
//!   [`PipelineReport::replaced`].
//!
//! A document that has never posted (first import, or a non-posting kind
//! that never leaves an entry behind) always takes the "different" path: it
//! is saved and posted fresh. One consequence, noted rather than hidden: a
//! document that never posts (`Estimate`, `PurchaseOrder`) has no entries to
//! key the skip check on, so an unchanged estimate is re-saved as a new
//! version on every re-run rather than being deduplicated the way a posting
//! document is. Nothing in `LEDGER-DESIGN.md` §6 asks for more than that
//! today, and estimates carry no accounting weight to duplicate.
//!
//! **Error handling** (§2, "an import that stops at the first oddity never
//! finishes"): a [`crate::post::PostError`] or a [`crate::store::LedgerError`]
//! on any one document — `PeriodClosed`, `MissingClass`, an unknown account,
//! anything — never aborts the run. It is collected as a [`Rejected`] row and
//! the walk continues. The only way this function itself returns an `Err` is
//! a failure reading the replica or building the chart/class mapping up
//! front — before any document has been touched.
//!
//! [`Ledger::set_replaying`](crate::store::Ledger::set_replaying) is held
//! `true` for the whole run (§4 "How that interacts with the outbox"), so
//! nothing downstream enqueues an outbox record for history that is being
//! derived, not newly transacted.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use thiserror::Error;

use ledger_core::{round_money, RoundingPolicy};
use qbo_local::domain::RealmId;
use qbo_local::store::Store;

use crate::chart;
use crate::import::{
    self, map_accounts, map_classes, AccountMapping, ClassMapping, ImportError, ImportOptions,
    ItemContext, Translated,
};
use crate::post::{self, Posting};
use crate::store::{CommandMeta, Ledger, LedgerError};
use crate::types::{
    AccountId, CompanyId, CustomerPosting, ItemAccounts, LedgerDocument, PostingConfig,
    PostingContext,
};

#[derive(Debug, Error)]
pub enum PipelineError {
    #[error("import: {0}")]
    Import(#[from] ImportError),
    #[error("store: {0}")]
    Store(#[from] LedgerError),
}

/// One document that could not be saved or posted. The run continues past
/// it; this is the record that it happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rejected {
    pub document_id: String,
    pub reason: String,
}

/// What one call to [`import_replica`] did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PipelineReport {
    /// Documents [`crate::import::run`] translated, in the requested
    /// `from`/`to` window.
    pub translated: usize,
    /// Translated documents that posted a new (or replacement) journal entry.
    pub posted: usize,
    /// Translated documents that are a non-posting kind (`Estimate`,
    /// `PurchaseOrder`, §1).
    pub non_posting: usize,
    /// Documents whose saved payload exactly matched what was already posted
    /// — skipped rather than reversed and reposted.
    pub skipped_unchanged: usize,
    /// Documents whose prior posted entries were reversed and replaced with
    /// a new posting, because the payload changed since the last run.
    pub replaced: usize,
    /// Documents that failed to save or post, and why. Never aborts the run.
    pub rejected: Vec<Rejected>,
    /// New rows written to `accounts` (unmapped QBO accounts in their x99N
    /// slot, plus any extra bank/card account past the seed chart's first).
    /// A number the seed chart already carries is left alone.
    pub accounts_created: usize,
    /// `"<number> <name>"` for every mapped account with `needs_mapping`,
    /// whether or not it was newly created this run (§2: sign-off is blocked
    /// while any of these carries a non-zero balance, not while any of these
    /// is new).
    pub accounts_needing_mapping: Vec<String>,
    /// New rows written to `classes` (a QBO class the §3 taxonomy has no
    /// match for, kept under its own QBO name).
    pub classes_created: usize,
    /// Every per-document translation warning, plus one line per precision
    /// failure and per unmatched class from [`crate::import::ImportReport`].
    pub warnings: Vec<String>,
}

impl PipelineReport {
    /// A terminal-friendly summary, for `ledger import` to print.
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        out.push_str("IMPORT\n");
        out.push_str(&format!("  translated             : {}\n", self.translated));
        out.push_str(&format!("  posted                 : {}\n", self.posted));
        out.push_str(&format!(
            "  non-posting            : {}\n",
            self.non_posting
        ));
        out.push_str(&format!(
            "  skipped (unchanged)    : {}\n",
            self.skipped_unchanged
        ));
        out.push_str(&format!("  replaced (re-imported) : {}\n", self.replaced));
        out.push_str(&format!(
            "  rejected               : {}\n",
            self.rejected.len()
        ));
        out.push_str(&format!(
            "  accounts created       : {}\n",
            self.accounts_created
        ));
        out.push_str(&format!(
            "  accounts needing mapping: {}\n",
            self.accounts_needing_mapping.len()
        ));
        out.push_str(&format!(
            "  classes created        : {}\n",
            self.classes_created
        ));

        if !self.accounts_needing_mapping.is_empty() {
            out.push_str("\n  accounts needing mapping (§2 — blocks sign-off if non-zero):\n");
            for name in &self.accounts_needing_mapping {
                out.push_str(&format!("    - {name}\n"));
            }
        }
        if !self.rejected.is_empty() {
            out.push_str("\n  rejected:\n");
            for row in &self.rejected {
                out.push_str(&format!("    - {}: {}\n", row.document_id, row.reason));
            }
        }
        if !self.warnings.is_empty() {
            out.push_str("\n  warnings:\n");
            for warning in &self.warnings {
                out.push_str(&format!("    - {warning}\n"));
            }
        }
        out
    }
}

/// Walk `replica`'s documents for `realm`, translating, saving and posting
/// each one into `ledger`'s `company` book. See the module docs for the
/// re-run and error-handling rules.
pub fn import_replica(
    ledger: &Ledger,
    replica: &Store,
    realm: &RealmId,
    company: &str,
    options: &ImportOptions,
    now: DateTime<Utc>,
) -> Result<PipelineReport, PipelineError> {
    let company_id = CompanyId(company.to_string());

    // --- the chart and class mappings, materialised into the ledger -------
    let parsed_accounts = import::parsed_accounts(replica, realm)?;
    let account_mapping = map_accounts(&parsed_accounts);
    let parsed_classes = import::parsed_classes(replica, realm)?;
    let class_mapping = map_classes(&parsed_classes, &company_id);

    let (accounts_created, accounts_needing_mapping) =
        materialize_accounts(ledger, company, &account_mapping)?;
    let classes_created = materialize_classes(ledger, company, &class_mapping)?;

    // --- the posting context: items, customers, config ---------------------
    let item_contexts = import::build_item_contexts(replica, realm, &class_mapping)?;
    let items = build_item_accounts(&item_contexts, &account_mapping);
    let customers_exempt = import::build_exempt_customers(replica, realm)?;
    let customers = customers_exempt
        .into_iter()
        .map(|id| {
            (
                id,
                CustomerPosting {
                    is_tax_exempt: true,
                },
            )
        })
        .collect();

    let posting_ctx = PostingContext {
        items,
        customers,
        config: PostingConfig::default(),
    };

    let mut report = PipelineReport {
        accounts_created,
        accounts_needing_mapping,
        classes_created,
        ..PipelineReport::default()
    };

    // --- walk and post, replaying so the outbox stays quiet -----------------
    ledger.set_replaying(true);
    let run_result = import::run(replica, realm, &company_id, options, &mut |translated| {
        process_document(ledger, company, &translated, &posting_ctx, now, &mut report);
        Ok(())
    });
    ledger.set_replaying(false);

    let import_report = run_result?;
    report.translated = import_report.translated;
    report.warnings = import_report.warnings;
    for qbo_id in &import_report.precision_failures {
        report
            .warnings
            .push(format!("{qbo_id}: precision failure, not imported"));
    }
    for qbo_id in &import_report.unmatched_classes {
        report
            .warnings
            .push(format!("class {qbo_id} did not match the §3 taxonomy"));
    }

    Ok(report)
}

/// Every mapped account not already present under this company (a number the
/// seed chart carries, or one a previous run already materialised) is
/// written through `add_account`; existing rows are left alone. Returns the
/// count created and `"<number> <name>"` for every account that
/// `needs_mapping`, present or new.
fn materialize_accounts(
    ledger: &Ledger,
    company: &str,
    mapping: &AccountMapping,
) -> Result<(usize, Vec<String>), PipelineError> {
    let mut existing: HashSet<String> = ledger
        .list_accounts(company)?
        .into_iter()
        .map(|account| account.number)
        .collect();

    let mut created = 0usize;
    let mut needs_mapping = Vec::new();

    for mapped in &mapping.mapped {
        if mapped.needs_mapping {
            needs_mapping.push(format!("{} {}", mapped.ledger_number.0, mapped.name));
        }
        if existing.contains(&mapped.ledger_number.0) {
            continue;
        }
        ledger.add_account(
            company,
            &mapped.ledger_number.0,
            &mapped.name,
            mapped.classification,
            false,
            Some(mapped.source_ref.as_str()),
            mapped.needs_mapping,
        )?;
        existing.insert(mapped.ledger_number.0.clone());
        created += 1;
    }

    Ok((created, needs_mapping))
}

/// As [`materialize_accounts`], for classes: every mapped class not already
/// present is written through `add_class`; existing rows (the seed six, or a
/// class a previous run already added) are left alone.
fn materialize_classes(
    ledger: &Ledger,
    company: &str,
    mapping: &ClassMapping,
) -> Result<usize, PipelineError> {
    let mut existing: HashSet<String> = ledger
        .list_classes(company)?
        .into_iter()
        .map(|class| class.class_id.0)
        .collect();

    let mut created = 0usize;
    for mapped in &mapping.mapped {
        if existing.contains(&mapped.class_id.0) {
            continue;
        }
        ledger.add_class(
            company,
            &mapped.class_id.0,
            &mapped.name,
            Some(mapped.source_ref.as_str()),
        )?;
        existing.insert(mapped.class_id.0.clone());
        created += 1;
    }
    Ok(created)
}

/// [`ItemContext`] (a replica item plus its own default class) resolved
/// through the account mapping into the [`ItemAccounts`] the posting
/// function needs. An item whose `IncomeAccountRef` or `ExpenseAccountRef`
/// is missing or unmapped falls back to 4100 / 5000 respectively, rather
/// than leaving `post()` nothing to post an item line to — `income` and
/// `expense` are not optional on [`ItemAccounts`].
fn build_item_accounts(
    items: &HashMap<String, ItemContext>,
    accounts: &AccountMapping,
) -> HashMap<String, ItemAccounts> {
    items
        .iter()
        .map(|(item_id, context)| {
            let income = context
                .item
                .income_account_id
                .as_deref()
                .and_then(|id| accounts.by_source_ref(id))
                .map(|mapped| mapped.ledger_number.clone())
                .unwrap_or_else(|| AccountId(chart::SALES_INCOME.to_string()));
            let expense = context
                .item
                .expense_account_id
                .as_deref()
                .and_then(|id| accounts.by_source_ref(id))
                .map(|mapped| mapped.ledger_number.clone())
                .unwrap_or_else(|| AccountId(chart::COGS.to_string()));
            let asset = context
                .item
                .asset_account_id
                .as_deref()
                .and_then(|id| accounts.by_source_ref(id))
                .map(|mapped| mapped.ledger_number.clone());
            // D9-consistent, but only in the "recoverable" direction §6 marks
            // out for unit_cost specifically: an over-precise cost warns and
            // comes back `None` (checked again here, independent of
            // `translate`'s own per-line check) rather than failing the item.
            let unit_cost = context.item.purchase_cost.and_then(|cost| {
                if cost.scale() > 2 {
                    None
                } else {
                    round_money(cost, RoundingPolicy::MirroredAmount).ok()
                }
            });

            (
                item_id.clone(),
                ItemAccounts {
                    income,
                    expense,
                    asset,
                    default_class: context.default_class.clone(),
                    unit_cost,
                },
            )
        })
        .collect()
}

/// One translated document: save, post, and update `report`. Never returns
/// an error — every failure is caught and turned into a [`Rejected`] row so
/// the walk in [`import_replica`] keeps going (§2).
fn process_document(
    ledger: &Ledger,
    company: &str,
    translated: &Translated,
    ctx: &PostingContext,
    now: DateTime<Utc>,
    report: &mut PipelineReport,
) {
    let doc = &translated.document;
    let document_id = doc.document_id.clone();

    let existing = match ledger.entries_for_document(company, &document_id) {
        Ok(rows) => rows,
        Err(err) => {
            report.rejected.push(Rejected {
                document_id,
                reason: err.to_string(),
            });
            return;
        }
    };
    let posted_existing: Vec<_> = existing
        .into_iter()
        .filter(|(_, _, posted)| *posted)
        .collect();

    let mut is_replace = false;

    if !posted_existing.is_empty() {
        if document_unchanged(ledger, company, &document_id, doc) {
            report.skipped_unchanged += 1;
            return;
        }

        for (entry_id, _entry, _posted) in &posted_existing {
            if let Err(err) = ledger.reverse_entry(company, entry_id, doc.txn_date, now) {
                report.rejected.push(Rejected {
                    document_id: document_id.clone(),
                    reason: format!("reversing prior entry {entry_id}: {err}"),
                });
                return;
            }
        }
        is_replace = true;
    }

    let version = match ledger.save_document(
        company,
        doc,
        CommandMeta {
            actor_id: "import".to_string(),
            kind: "import_qbo".to_string(),
            hlc: now.to_rfc3339(),
        },
        now,
    ) {
        Ok(version) => version,
        Err(err) => {
            report.rejected.push(Rejected {
                document_id,
                reason: err.to_string(),
            });
            return;
        }
    };

    // §6: a voided document's entry is already the reversal — `post::post`
    // swapped its sides — so it is posted exactly like any other entry here.
    match post::post(doc, version.version, ctx) {
        Ok(Posting::NonPosting) => {
            report.non_posting += 1;
        }
        Ok(Posting::Entry(entry)) => match ledger.post_entry(company, &entry, now) {
            Ok(_) => {
                report.posted += 1;
                if is_replace {
                    report.replaced += 1;
                }
            }
            Err(err) => report.rejected.push(Rejected {
                document_id,
                reason: err.to_string(),
            }),
        },
        Err(err) => report.rejected.push(Rejected {
            document_id,
            reason: err.to_string(),
        }),
    }
}

/// Whether `doc` is byte-for-byte (as `serde_json::Value`, so field order
/// never matters) the same document already saved as the latest version of
/// `document_id`. `false` on any read or parse failure — an ambiguous
/// comparison always takes the safer "something changed" path.
fn document_unchanged(
    ledger: &Ledger,
    company: &str,
    document_id: &str,
    doc: &LedgerDocument,
) -> bool {
    let Ok(Some(raw)) = ledger.latest_document_payload(company, document_id) else {
        return false;
    };
    let Ok(saved) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    let Ok(fresh) = serde_json::to_value(doc) else {
        return false;
    };
    saved == fresh
}
