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
//! A document that has never posted (first import) always takes the
//! "different" path: it is saved and posted fresh. A non-posting kind
//! (`Estimate`, `PurchaseOrder`) never leaves a posted entry behind, so it
//! cannot use the entries-exist check above at all — it is instead
//! deduplicated by comparing its translated payload directly against the
//! latest saved version (the same `document_unchanged` comparison), and
//! skipped when it matches. Counted in [`PipelineReport::skipped_unchanged`]
//! and [`PipelineReport::non_posting`] respectively, same as a posting
//! document.
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

use chrono::{DateTime, NaiveDate, Utc};
use thiserror::Error;

use ledger_core::{round_money, RoundingPolicy};
use qbo_local::domain::RealmId;
use qbo_local::store::Store;

use crate::chart;
use crate::import::{
    self, map_accounts, map_classes, AccountMapping, ClassMapping, ImportError, ImportOptions,
    ItemContext, MappedAccount, Translated,
};
use crate::post::{self, Posting};
use crate::report;
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
            // A seed-chart account the mapping landed on: give it the QBO id
            // so the §7 diff can join it. First writer wins; a second QBO
            // account mapped onto the same number is a mapping to review,
            // and it is reported rather than silently taking the slot.
            if !ledger.set_account_source_ref(
                company,
                &mapped.ledger_number.0,
                &mapped.source_ref,
            )? && !mapped.needs_mapping
            {
                let current = ledger
                    .list_accounts(company)?
                    .into_iter()
                    .find(|account| account.number == mapped.ledger_number.0)
                    .and_then(|account| account.source_ref);
                if current.as_deref() != Some(mapped.source_ref.as_str()) {
                    needs_mapping.push(format!(
                        "{} {} (QBO {} also maps here; {} holds it)",
                        mapped.ledger_number.0,
                        mapped.name,
                        mapped.source_ref,
                        current.unwrap_or_default()
                    ));
                }
            }
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

    // Estimate and PurchaseOrder never post, so they never leave a posted
    // entry behind for the check below to key on. Compare the payload
    // directly instead: an unchanged one is skipped exactly like a posting
    // document whose entries matched, rather than re-saved as a fresh
    // version on every run.
    if !doc.kind.posts() {
        if document_unchanged(ledger, company, &document_id, doc) {
            report.skipped_unchanged += 1;
            return;
        }
        match ledger.save_document(
            company,
            doc,
            CommandMeta {
                actor_id: "import".to_string(),
                kind: "import_qbo".to_string(),
                hlc: now.to_rfc3339(),
            },
            now,
        ) {
            Ok(_) => report.non_posting += 1,
            Err(err) => report.rejected.push(Rejected {
                document_id,
                reason: err.to_string(),
            }),
        }
        return;
    }

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

// ---------------------------------------------------------------------------
// Opening balances (§6 "Full history from where, opening balances before")
// ---------------------------------------------------------------------------

/// What one call to [`apply_opening_balance`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpeningReport {
    /// The opening balance document's deterministic id
    /// (`opening-balance:<company>:<as_of>`, [`import::opening_balance_entry`]).
    pub document_id: String,
    /// A saved opening balance for this company and `as_of` already existed
    /// with an identical payload — skipped rather than reversed and reposted
    /// (the same rule [`import_replica`] applies to every document).
    pub skipped_unchanged: bool,
    /// A saved opening balance existed and differed: its posted entries were
    /// reversed as of `as_of` and the new version posted in their place.
    pub replaced: bool,
}

/// Build every mapped account this company's chart already knows about from
/// the accounts materialised in `ledger` itself — every row with a
/// `source_ref` (§2, set the first time an import or a prior opening balance
/// mapped a QBO account onto it). Unlike [`import_replica`], applying an
/// opening balance needs no replica and no realm: the only accounts a QBO
/// trial balance row can possibly land on are the ones already in this
/// company's chart.
fn account_mapping_from_ledger(
    ledger: &Ledger,
    company: &str,
) -> Result<AccountMapping, PipelineError> {
    let mapped = ledger
        .list_accounts(company)?
        .into_iter()
        .filter_map(|account| {
            let source_ref = account.source_ref?;
            Some(MappedAccount {
                source_ref,
                ledger_number: account.account_id,
                name: account.name,
                classification: account.classification,
                needs_mapping: account.needs_mapping,
            })
        })
        .collect();
    Ok(AccountMapping { mapped })
}

/// §6: apply — or idempotently skip or replace — the single opening balance
/// entry that stands in for every year before the boundary year. Builds the
/// entry with [`import::opening_balance_entry`] against
/// [`account_mapping_from_ledger`], saves it as a `Journal`-kind document
/// with [`CommandMeta::kind`] `"opening_balance"`, and posts it flagged
/// (§4: manual and imported journal entries carry `is_flagged`).
///
/// Idempotent, the same rule §6 gives every imported document (see this
/// module's own docs above): a saved opening balance document whose payload
/// is unchanged is skipped entirely; one that differs has its posted entries
/// reversed as of `as_of` and the new version posted in its place. Safe to
/// call again with the same `qbo_tb`, and safe to call as part of
/// [`boundary_walk`]'s per-year scratch replay.
pub fn apply_opening_balance(
    ledger: &Ledger,
    company: &str,
    as_of: NaiveDate,
    qbo_tb: &[report::QboTbRow],
    now: DateTime<Utc>,
) -> Result<OpeningReport, PipelineError> {
    let company_id = CompanyId(company.to_string());
    let mapping = account_mapping_from_ledger(ledger, company)?;
    let (doc, mut entry) = import::opening_balance_entry(&company_id, as_of, qbo_tb, &mapping)?;
    let document_id = doc.document_id.clone();

    let posted_existing: Vec<_> = ledger
        .entries_for_document(company, &document_id)?
        .into_iter()
        .filter(|(_, _, posted)| *posted)
        .collect();

    if !posted_existing.is_empty() && document_unchanged(ledger, company, &document_id, &doc) {
        return Ok(OpeningReport {
            document_id,
            skipped_unchanged: true,
            replaced: false,
        });
    }

    let replaced = !posted_existing.is_empty();
    for (entry_id, _entry, _posted) in &posted_existing {
        ledger.reverse_entry(company, entry_id, as_of, now)?;
    }

    let version = ledger.save_document(
        company,
        &doc,
        CommandMeta {
            actor_id: "opening_balance".to_string(),
            kind: "opening_balance".to_string(),
            hlc: now.to_rfc3339(),
        },
        now,
    )?;
    entry.source_version = version.version;
    ledger.post_entry(company, &entry, now)?;

    Ok(OpeningReport {
        document_id,
        skipped_unchanged: false,
        replaced,
    })
}

// ---------------------------------------------------------------------------
// The boundary-year walk (§6, same section)
// ---------------------------------------------------------------------------

/// One year of the walk: whether the replay agreed with QBO's own trial
/// balance for that year, and the diff that says so.
#[derive(Clone, Debug, PartialEq)]
pub struct YearAgreement {
    pub year: i32,
    /// `diff.must_failures.is_empty()` — the §7 tolerance rules the walk
    /// procedure itself calls for ("Compare ... on the §7 tolerance rules").
    pub agrees: bool,
    pub diff: report::TbDiff,
}

/// What [`boundary_walk`] found.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundaryReport {
    /// Every year examined, most recent first (the order the walk runs in),
    /// stopping at the first disagreement.
    pub years: Vec<YearAgreement>,
    /// The earliest year such that it and every later year agree —
    /// [`import::boundary_year`] over `years`. `None` if even the most
    /// recent year disagrees.
    pub boundary_year: Option<i32>,
}

impl BoundaryReport {
    /// The §7-style fixed-width table, plus the boundary year itself.
    pub fn render_text(&self, company: &str) -> String {
        let mut out = String::new();
        out.push_str(&format!("BOUNDARY WALK   {company}\n\n"));
        out.push_str(&format!(
            "{:<8}{:<10}{:>16}\n",
            "year", "agrees", "must-failures"
        ));
        for year in &self.years {
            out.push_str(&format!(
                "{:<8}{:<10}{:>16}\n",
                year.year,
                if year.agrees { "yes" } else { "no" },
                year.diff.must_failures.len(),
            ));
        }
        match self.boundary_year {
            Some(y) => out.push_str(&format!("\nboundary year: {y}\n")),
            None => out.push_str("\nboundary year: none — even the most recent year disagrees\n"),
        }
        out
    }
}

/// The §6 boundary walk, made mechanical. For each year in `snapshots`, most
/// recent first: replay only that year's documents (`ImportOptions` bounded
/// to 1 January .. 31 December of that year) on top of an opening balance
/// taken from the *prior* year's own snapshot (when one is given), against a
/// fresh **scratch, in-memory `Ledger`** — one per year, and never `ledger`,
/// the caller's real one, which this function only borrows and never writes
/// to. Every scratch ledger is built the normal way, through
/// [`import_replica`] and [`apply_opening_balance`], so the replay a human
/// would get by actually running those commands against a fresh database is
/// exactly what this function checks.
///
/// Stops at the first year (walking backwards) whose replayed 31 December
/// trial balance disagrees with `qbo_tb` on the §7 tiers — everything
/// earlier is not examined, because §6 turns it into one opening balance
/// entry rather than replaying it. Returns the earliest agreeing year via
/// [`import::boundary_year`] plus each examined year's diff.
pub fn boundary_walk(
    ledger: &Ledger,
    replica: &Store,
    realm: &RealmId,
    company: &str,
    snapshots: &[(i32, Vec<report::QboTbRow>)],
    tiers: &report::TierRules,
    now: DateTime<Utc>,
) -> Result<BoundaryReport, PipelineError> {
    // `ledger` is taken so a caller can pass the same handle it uses for
    // everything else; the walk itself never reads or writes it — see the
    // doc comment above.
    let _ = ledger;

    let mut years_sorted: Vec<i32> = snapshots.iter().map(|(year, _)| *year).collect();
    years_sorted.sort_unstable();
    let rows_for = |year: i32| -> Option<&Vec<report::QboTbRow>> {
        snapshots
            .iter()
            .find(|(y, _)| *y == year)
            .map(|(_, rows)| rows)
    };

    let mut years = Vec::new();

    for year in years_sorted.into_iter().rev() {
        let scratch = Ledger::open_in_memory()?;
        scratch.create_company(company, company, Some(realm.as_str()), now)?;

        let from = NaiveDate::from_ymd_opt(year, 1, 1).expect("1 January is always a valid date");
        let to = NaiveDate::from_ymd_opt(year, 12, 31).expect("31 December is always a valid date");
        let options = ImportOptions {
            from: Some(from),
            to: Some(to),
        };
        import_replica(&scratch, replica, realm, company, &options, now)?;

        if let Some(prior_rows) = rows_for(year - 1) {
            let as_of = NaiveDate::from_ymd_opt(year - 1, 12, 31)
                .expect("31 December is always a valid date");
            apply_opening_balance(&scratch, company, as_of, prior_rows, now)?;
        }

        let qbo_rows = rows_for(year).expect("year was read from snapshots itself");
        let tb = report::trial_balance(&scratch, company, to)?;
        let diff = report::tb_diff(&tb, qbo_rows, tiers)?;
        let agrees = diff.must_failures.is_empty();

        years.push(YearAgreement { year, agrees, diff });

        if !agrees {
            break;
        }
    }

    let agreements: Vec<(i32, import::TbAgreement)> = years
        .iter()
        .map(|year| {
            (
                year.year,
                import::TbAgreement {
                    agrees: year.agrees,
                },
            )
        })
        .collect();
    let boundary_year = import::boundary_year(&agreements);

    Ok(BoundaryReport {
        years,
        boundary_year,
    })
}
