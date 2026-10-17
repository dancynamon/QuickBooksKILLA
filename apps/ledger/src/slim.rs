//! D28's slim cutover import: QBO's masters, one opening entry and the
//! documents still open on the as-of date, checked against QBO before the
//! run counts as done. `docs/CUTOVER.md` is the runbook; DECISIONS.md D28
//! ("Addendum: how the open items and the opening entry share AR and AP")
//! is the design.
//!
//! - **Masters.** Chart and classes through the same mapping `ledger import`
//!   uses, then customers, vendors (with their terms folded into
//!   `terms_days`) and items into [`crate::books`], every one keyed by its QBO
//!   id with the id kept in `source_ref`. Numbering is moved past QBO's
//!   highest number per type ([`books::ensure_next_number`]).
//! - **Opening entry.** QBO's trial balance as of the date, one line per
//!   account, balanced by construction; the 3950 plug must come out zero.
//!   One reclass on top: customer payments still unapplied on the date leave
//!   1200 for 2300, where the ledger's own posting rule parks them (W8).
//! - **Open documents, never posted.** Invoices, credit memos, bills and
//!   vendor credits open on the date (worked out from the replica's
//!   documents with [`books::open_items`], so a payment dated after the
//!   as-of date does not close them), AR/AP lines that journal entries and
//!   deposits left open, unapplied payments, open estimates and open purchase
//!   orders. An AR or AP document's balance is already in the opening entry's
//!   1200/2000 line, so it is saved under [`books::SLIM_OPEN_ITEM_KIND`] and
//!   never posted; [`books::open_documents`] reads it like any other, which
//!   is what makes the aging tie to the control account.
//! - **Checks**, every one failing the run: the plug and 3950 are zero; AR
//!   open items equal 1200 and AP open items equal 2000; AR aging by customer
//!   and AP aging by vendor equal QBO's (its aging report when given, else the
//!   documents' own `Balance` fields); the trial balance equals QBO's on every
//!   must-tier line (§7, [`report::tb_diff`]).
//!
//! Re-runnable: masters upsert, an unchanged document or opening entry is
//! skipped, a changed one is replaced, and an open item from an earlier run
//! that is no longer open is saved again voided.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::str::FromStr;

use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde_json::Value;
use thiserror::Error;

use ledger_core::{round_money, Money, MoneyError, RoundingPolicy};
use qbo_local::domain::{EntityType, RealmId};
use qbo_local::store::{MirroredEntity, Store, StoreError};

use crate::books::{self, Book, BooksError, Contact, Item, ItemType, OpenDocument};
use crate::chart;
use crate::import::{
    self, qbo_document_id, translate, ClassFallback, ImportError, TranslateContext,
};
use crate::pipeline::{self, OpeningReport, PipelineError};
use crate::report::{self, QboTbRow, TierRules};
use crate::store::{CommandMeta, Ledger, LedgerError};
use crate::types::{
    AccountId, ClassId, CompanyId, ContactKind, ContactRef, DocKind, DocLine, JournalLine,
    LedgerDocument, LineKind,
};

/// The oplog kind open estimates and purchase orders are saved under. They
/// never post, so unlike [`books::SLIM_OPEN_ITEM_KIND`] nothing refuses a
/// later edit of them.
pub const SLIM_CONTRACT_KIND: &str = "import_slim_contract";

/// The oplog kind of the opening entry this import posts.
pub const SLIM_OPENING_KIND: &str = "import_slim_opening";

/// At most this many detail lines per failed check, so a first rehearsal
/// against the live replica stays readable.
const MAX_DETAILS: usize = 60;

#[derive(Debug, Error)]
pub enum SlimError {
    #[error("import: {0}")]
    Import(#[from] ImportError),
    #[error("pipeline: {0}")]
    Pipeline(#[from] PipelineError),
    #[error("ledger: {0}")]
    Ledger(#[from] LedgerError),
    #[error("books: {0}")]
    Books(#[from] BooksError),
    #[error("replica: {0}")]
    Replica(#[from] StoreError),
    #[error("money: {0}")]
    Money(#[from] MoneyError),
}

/// One row of QBO's `AgedReceivables` or `AgedPayables` summary, the CSV
/// `qbo-local report --name ar-aging|ap-aging` writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QboAgingRow {
    pub entity_id: Option<String>,
    pub name: String,
    pub current: Money,
    pub days_1_30: Money,
    pub days_31_60: Money,
    pub days_61_90: Money,
    pub over_90: Money,
    pub total: Money,
}

#[derive(Clone, Debug)]
pub struct SlimOptions {
    /// The last day QBO is the book of record: 31 December 2026 (D29).
    pub as_of: NaiveDate,
    /// QBO's trial balance as of `as_of`, signed debit-positive.
    pub qbo_tb: Vec<QboTbRow>,
    /// QBO's AR aging as of `as_of`. `None` checks against the documents'
    /// own `Balance` fields, which is only sound while nothing dated after
    /// `as_of` has touched them.
    pub qbo_ar: Option<Vec<QboAgingRow>>,
    /// As `qbo_ar`, for AP.
    pub qbo_ap: Option<Vec<QboAgingRow>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub passed: bool,
    pub details: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SlimReport {
    pub as_of: Option<NaiveDate>,
    pub customers: usize,
    pub vendors: usize,
    pub items: usize,
    /// Items with no default class from QBO. A 2027 line on one of these
    /// needs a class from the line, the header or the contact.
    pub items_without_class: usize,
    pub accounts_created: usize,
    /// `"<number> <name>"` for every account in an x99N slot (§2). Joel's to
    /// map; tbdiff skips them, so they never fail the run on their own.
    pub accounts_needing_mapping: Vec<String>,
    pub classes_created: usize,
    /// The next number each numbered type will hand out.
    pub next_numbers: Vec<(DocKind, i64)>,
    pub opening: Option<OpeningReport>,
    pub opening_lines: usize,
    /// Customer payments unapplied on the date, moved from 1200 to 2300.
    pub unapplied_reclass: Money,
    /// Open documents by what they are.
    pub open_counts: BTreeMap<String, usize>,
    pub saved: usize,
    pub skipped_unchanged: usize,
    /// Open items an earlier run brought across that are no longer open.
    pub voided_stale: usize,
    pub checks: Vec<Check>,
    pub warnings: Vec<String>,
}

impl SlimReport {
    pub fn passed(&self) -> bool {
        self.checks.iter().all(|check| check.passed)
    }

    pub fn render_text(&self, company: &str) -> String {
        let mut out = String::new();
        let as_of = self.as_of.map(|d| d.to_string()).unwrap_or_default();
        out.push_str(&format!(
            "SLIM CUTOVER IMPORT   {company}   as of {as_of}\n\n"
        ));
        out.push_str("masters\n");
        out.push_str(&format!("  customers              : {}\n", self.customers));
        out.push_str(&format!("  vendors                : {}\n", self.vendors));
        out.push_str(&format!(
            "  items                  : {} ({} with no default class)\n",
            self.items, self.items_without_class
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
        out.push_str("  next numbers           :");
        for (kind, next) in &self.next_numbers {
            out.push_str(&format!(" {}={next}", kind.as_str()));
        }
        out.push('\n');

        out.push_str("\nopening entry\n");
        if let Some(opening) = &self.opening {
            let state = if opening.skipped_unchanged {
                "unchanged, skipped"
            } else if opening.replaced {
                "replaced"
            } else {
                "posted"
            };
            out.push_str(&format!("  {} ({state})\n", opening.document_id));
        }
        out.push_str(&format!(
            "  lines                  : {}\n",
            self.opening_lines
        ));
        out.push_str(&format!(
            "  unapplied 1200 -> 2300 : {}\n",
            format_money(self.unapplied_reclass)
        ));

        out.push_str("\nopen documents (saved, never posted)\n");
        for (what, count) in &self.open_counts {
            out.push_str(&format!("  {what:<34}: {count}\n"));
        }
        out.push_str(&format!(
            "  saved                             : {}\n",
            self.saved
        ));
        out.push_str(&format!(
            "  unchanged, skipped                : {}\n",
            self.skipped_unchanged
        ));
        out.push_str(&format!(
            "  no longer open, voided            : {}\n",
            self.voided_stale
        ));

        out.push_str("\nchecks\n");
        for check in &self.checks {
            out.push_str(&format!(
                "  [{}] {}\n",
                if check.passed { "ok  " } else { "FAIL" },
                check.name
            ));
            for detail in &check.details {
                out.push_str(&format!("         {detail}\n"));
            }
        }

        if !self.accounts_needing_mapping.is_empty() {
            out.push_str("\naccounts needing mapping (Joel; §2):\n");
            for name in &self.accounts_needing_mapping {
                out.push_str(&format!("  - {name}\n"));
            }
        }
        if !self.warnings.is_empty() {
            out.push_str("\nwarnings\n");
            for warning in &self.warnings {
                out.push_str(&format!("  - {warning}\n"));
            }
        }
        out.push_str(&format!(
            "\n{}\n",
            if self.passed() {
                "RESULT: every check passed"
            } else {
                "RESULT: FAILED — see the checks above"
            }
        ));
        out
    }
}

/// Run the slim import into `ledger`'s `company` book. Writes as it goes;
/// the CLI rehearses on a scratch copy first and only touches the real book
/// when the rehearsal passed (`docs/CUTOVER.md`).
pub fn import_slim(
    ledger: &Ledger,
    replica: &Store,
    realm: &RealmId,
    company: &str,
    options: &SlimOptions,
    now: DateTime<Utc>,
) -> Result<SlimReport, SlimError> {
    let as_of = options.as_of;
    let company_id = CompanyId(company.to_string());
    let mut report = SlimReport {
        as_of: Some(as_of),
        ..SlimReport::default()
    };
    let fallback = ClassFallback {
        class: ClassId(chart::IMPORT_FALLBACK_CLASS.0.to_string()),
        before: as_of
            .succ_opt()
            .expect("the as-of date is not the last representable day"),
    };
    let ctx = import::translate_context(replica, realm, &company_id, Some(fallback))?;

    // --- 1. masters -------------------------------------------------------
    let (accounts_created, needing_mapping) =
        pipeline::materialize_accounts(ledger, company, &ctx.accounts)?;
    report.accounts_created = accounts_created;
    report.accounts_needing_mapping = needing_mapping;
    report.classes_created = pipeline::materialize_classes(ledger, company, &ctx.classes)?;
    if pipeline::ensure_class(ledger, company, chart::IMPORT_FALLBACK_CLASS.0)? {
        report.classes_created += 1;
    }
    for qbo_id in &ctx.classes.unmatched {
        report.warnings.push(format!(
            "class {qbo_id} did not match the §3 taxonomy; kept under its QBO name"
        ));
    }

    let terms = read_terms(replica, realm, &mut report.warnings)?;
    import_contacts(ledger, replica, realm, company, &terms, now, &mut report)?;
    import_items(ledger, replica, realm, company, &ctx, now, &mut report)?;
    report.warnings.push(
        "QBO sales orders are not mirrored in the replica (no SalesOrder entity), so open sales \
         orders are not imported; re-enter them in the ledger or carry them as estimates"
            .to_string(),
    );

    // --- 2. open documents --------------------------------------------------
    let ar_ap = ar_ap_accounts(replica, realm)?;
    let docs = read_documents(replica, realm, &ctx, &ar_ap, as_of, &mut report.warnings)?;
    let plan = plan_open_documents(&docs, as_of, &mut report)?;
    save_open_documents(ledger, company, &plan, as_of, now, &mut report)?;

    // --- 3. the opening entry -----------------------------------------------
    let qbo_net = Money::checked_sum(options.qbo_tb.iter().map(|row| row.balance))?;
    let (opening_doc, mut entry) =
        import::opening_balance_entry(&company_id, as_of, &options.qbo_tb, &ctx.accounts)?;
    let mut opening_doc = opening_doc;
    let unapplied = Money::checked_sum(plan.unapplied_payments.iter().map(|doc| doc.unapplied))?;
    report.unapplied_reclass = unapplied;
    if !unapplied.is_zero() {
        let next = entry.lines.iter().map(|l| l.line_no).max().unwrap_or(0) + 1;
        let description = format!("Customer payments unapplied at {as_of}, held in 2300 (W8)");
        entry.lines.push(JournalLine::debit(
            next,
            AccountId(chart::ACCOUNTS_RECEIVABLE.into()),
            unapplied,
        ));
        entry.lines.push(JournalLine::credit(
            next + 1,
            AccountId(chart::CUSTOMER_DEPOSITS.into()),
            unapplied,
        ));
        for (line_no, account) in [
            (next, chart::ACCOUNTS_RECEIVABLE),
            (next + 1, chart::CUSTOMER_DEPOSITS),
        ] {
            opening_doc.lines.push(account_line(
                line_no,
                account,
                unapplied,
                description.clone(),
            ));
        }
    }
    report.opening_lines = entry.lines.len();
    if entry.lines.is_empty() {
        report
            .warnings
            .push("QBO's trial balance has no non-zero row; no opening entry posted".to_string());
    } else {
        report.opening = Some(pipeline::apply_flagged_entry(
            ledger,
            company,
            &opening_doc,
            entry,
            SLIM_OPENING_KIND,
            now,
        )?);
    }

    // --- 4. numbering -------------------------------------------------------
    for (kind, entity) in [
        (DocKind::Estimate, EntityType::Estimate),
        (DocKind::Invoice, EntityType::Invoice),
        (DocKind::SalesReceipt, EntityType::SalesReceipt),
        (DocKind::CreditMemo, EntityType::CreditMemo),
        (DocKind::RefundReceipt, EntityType::RefundReceipt),
        (DocKind::PurchaseOrder, EntityType::PurchaseOrder),
    ] {
        let highest = highest_doc_number(replica, realm, entity)?;
        let next = books::ensure_next_number(
            ledger,
            company,
            kind,
            highest.map_or(books::FIRST_NUMBER, |n| n + 1),
        )?;
        report.next_numbers.push((kind, next));
    }

    // --- 5. checks ----------------------------------------------------------
    run_checks(ledger, company, options, &docs, &plan, qbo_net, &mut report)?;
    Ok(report)
}

// ---------------------------------------------------------------------------
// Masters
// ---------------------------------------------------------------------------

/// Every live entity of one type, deleted ones skipped.
fn entities(
    replica: &Store,
    realm: &RealmId,
    entity_type: EntityType,
) -> Result<Vec<MirroredEntity>, StoreError> {
    let mut out = Vec::new();
    for entry in replica.entity_index(realm, entity_type)? {
        if let Some(entity) = replica.get_entity(realm, entity_type, &entry.qbo_id)? {
            if !entity.is_deleted {
                out.push(entity);
            }
        }
    }
    out.sort_by(|a, b| a.qbo_id.cmp(&b.qbo_id));
    Ok(out)
}

/// QBO term id to net days. A date-driven term (due on a day of the month)
/// has no net-days equivalent: it is reported and treated as due on receipt.
fn read_terms(
    replica: &Store,
    realm: &RealmId,
    warnings: &mut Vec<String>,
) -> Result<HashMap<String, Option<i64>>, StoreError> {
    let mut terms = HashMap::new();
    for entity in entities(replica, realm, EntityType::Term)? {
        let raw = &entity.raw_json;
        let days = raw.get("DueDays").and_then(Value::as_i64);
        if days.is_none() {
            warnings.push(format!(
                "term {} ({}) is not a net-days term; contacts on it are due on receipt",
                entity.qbo_id,
                text(raw, "Name").unwrap_or_default()
            ));
        }
        terms.insert(entity.qbo_id.clone(), days);
    }
    Ok(terms)
}

fn import_contacts(
    ledger: &Ledger,
    replica: &Store,
    realm: &RealmId,
    company: &str,
    terms: &HashMap<String, Option<i64>>,
    now: DateTime<Utc>,
    report: &mut SlimReport,
) -> Result<(), SlimError> {
    for (entity_type, kind, term_field) in [
        (EntityType::Customer, ContactKind::Customer, "SalesTermRef"),
        (EntityType::Vendor, ContactKind::Vendor, "TermRef"),
    ] {
        for entity in entities(replica, realm, entity_type)? {
            let raw = &entity.raw_json;
            let Some(display_name) = text(raw, "DisplayName") else {
                report.warnings.push(format!(
                    "{} {} has no DisplayName; skipped",
                    entity_type.as_str(),
                    entity.qbo_id
                ));
                continue;
            };
            let is_tax_exempt = kind == ContactKind::Customer
                && (raw.get("Taxable").and_then(Value::as_bool) == Some(false)
                    || raw.get("ResaleNum").is_some());
            let contact = Contact {
                contact_id: entity.qbo_id.clone(),
                kind,
                display_name,
                company_name: text(raw, "CompanyName"),
                email: nested_text(raw, "PrimaryEmailAddr", "Address"),
                phone: nested_text(raw, "PrimaryPhone", "FreeFormNumber"),
                bill_address: address(raw.get("BillAddr")),
                ship_address: address(raw.get("ShipAddr")),
                terms_days: reference(raw, term_field)
                    .and_then(|id| terms.get(&id).copied().flatten()),
                is_tax_exempt,
                default_class: None,
                is_active: raw.get("Active").and_then(Value::as_bool).unwrap_or(true),
                source_ref: Some(entity.qbo_id.clone()),
            };
            match books::upsert_contact(ledger, company, &contact, now) {
                Ok(()) => match kind {
                    ContactKind::Customer => report.customers += 1,
                    ContactKind::Vendor => report.vendors += 1,
                },
                Err(err) => report.warnings.push(format!(
                    "{} {} ({}): not imported: {err}",
                    entity_type.as_str(),
                    entity.qbo_id,
                    contact.display_name
                )),
            }
        }
    }
    Ok(())
}

fn import_items(
    ledger: &Ledger,
    replica: &Store,
    realm: &RealmId,
    company: &str,
    ctx: &TranslateContext,
    now: DateTime<Utc>,
    report: &mut SlimReport,
) -> Result<(), SlimError> {
    let mapped = |id: Option<&str>| {
        id.and_then(|id| ctx.accounts.by_source_ref(id))
            .map(|m| m.ledger_number.clone())
    };
    let mut groups = 0usize;
    let mut bad_prices = 0usize;
    for entity in entities(replica, realm, EntityType::Item)? {
        let raw = &entity.raw_json;
        let Some(name) = text(raw, "FullyQualifiedName").or_else(|| text(raw, "Name")) else {
            continue;
        };
        let item_type = match text(raw, "Type").as_deref() {
            Some("Category") => continue,
            Some("Inventory") => ItemType::Inventory,
            Some("Service") => ItemType::Service,
            Some("Group") => {
                groups += 1;
                ItemType::NonInventory
            }
            _ => ItemType::NonInventory,
        };
        let asset_account = match item_type {
            ItemType::Inventory => Some(
                mapped(reference(raw, "AssetAccountRef").as_deref()).unwrap_or_else(|| {
                    report.warnings.push(format!(
                        "item {} ({name}) is inventory with no mapped asset account; using 1310",
                        entity.qbo_id
                    ));
                    AccountId(chart::INVENTORY_FINISHED.into())
                }),
            ),
            _ => None,
        };
        let mut price = |field: &str| match decimal(raw.get(field)) {
            None => None,
            Some(d) => match round_money(d, RoundingPolicy::MirroredAmount) {
                Ok(m) => Some(m),
                Err(_) => {
                    bad_prices += 1;
                    None
                }
            },
        };
        let sales_price = price("UnitPrice");
        let purchase_cost = price("PurchaseCost");
        let default_class = ctx
            .items
            .get(&entity.qbo_id)
            .and_then(|context| context.default_class.clone());
        if default_class.is_none() {
            report.items_without_class += 1;
        }
        let mut item = Item {
            item_id: entity.qbo_id.clone(),
            sku: text(raw, "Sku").filter(|s| !s.trim().is_empty()),
            name,
            description: text(raw, "Description"),
            item_type,
            income_account: mapped(reference(raw, "IncomeAccountRef").as_deref())
                .unwrap_or_else(|| AccountId(chart::SALES_INCOME.into())),
            expense_account: mapped(reference(raw, "ExpenseAccountRef").as_deref())
                .unwrap_or_else(|| AccountId(chart::COGS.into())),
            asset_account,
            default_class,
            sales_price,
            purchase_cost,
            unit_cost: if item_type == ItemType::Inventory {
                purchase_cost
            } else {
                None
            },
            is_taxable: raw.get("Taxable").and_then(Value::as_bool).unwrap_or(true),
            is_active: raw.get("Active").and_then(Value::as_bool).unwrap_or(true),
            source_ref: Some(entity.qbo_id.clone()),
        };
        let mut result = books::upsert_item(ledger, company, &item, now);
        if result.is_err() && item.sku.is_some() {
            // QBO does not keep SKUs unique; the ledger does.
            report.warnings.push(format!(
                "item {} ({}): SKU {:?} is already taken; imported without it",
                item.item_id,
                item.name,
                item.sku.as_deref().unwrap_or_default()
            ));
            item.sku = None;
            result = books::upsert_item(ledger, company, &item, now);
        }
        match result {
            Ok(()) => report.items += 1,
            Err(err) => report.warnings.push(format!(
                "item {} ({}): not imported: {err}",
                item.item_id, item.name
            )),
        }
    }
    if groups > 0 {
        report.warnings.push(format!(
            "{groups} QBO group (bundle) items imported as non-inventory items"
        ));
    }
    if bad_prices > 0 {
        report.warnings.push(format!(
            "{bad_prices} item prices or costs carried more than two decimal places and were left unset"
        ));
    }
    if report.items_without_class > 0 {
        report.warnings.push(format!(
            "{} items have no default class; a 2027 line on one needs its class from the line, \
             header or customer (D27 item 1)",
            report.items_without_class
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Documents
// ---------------------------------------------------------------------------

/// QBO account ids of type Accounts Receivable and Accounts Payable.
struct ArApAccounts {
    receivable: HashSet<String>,
    payable: HashSet<String>,
}

fn ar_ap_accounts(replica: &Store, realm: &RealmId) -> Result<ArApAccounts, ImportError> {
    let mut out = ArApAccounts {
        receivable: HashSet::new(),
        payable: HashSet::new(),
    };
    for account in import::parsed_accounts(replica, realm)? {
        let kind = account
            .account_type
            .as_deref()
            .unwrap_or_default()
            .replace(' ', "")
            .to_ascii_lowercase();
        if kind == "accountsreceivable" {
            out.receivable.insert(account.qbo_id);
        } else if kind == "accountspayable" {
            out.payable.insert(account.qbo_id);
        }
    }
    Ok(out)
}

/// The replica documents the import works from, translated. Settling
/// documents dated after the as-of date are kept (they only ever count as
/// "later activity"); `open_items` ignores them by date.
struct Documents {
    /// Invoices, credit memos, payments, and the AR items journal entries
    /// and deposits left, as documents [`books::open_items`] reads.
    receivable: Vec<LedgerDocument>,
    /// Bills, vendor credits, bill payments, and the AP items of journal
    /// entries and deposits.
    payable: Vec<LedgerDocument>,
    /// Open estimates and purchase orders dated on or before the as-of date.
    contracts: Vec<LedgerDocument>,
    /// `document_id` to QBO's own open balance for the balance-field check:
    /// signed like an open item, by contact and due date.
    qbo_balances: Vec<(Book, String, NaiveDate, Money)>,
    /// A settling document dated after the as-of date exists, so the
    /// replica's `Balance` fields no longer describe that date.
    later_activity: bool,
}

fn read_documents(
    replica: &Store,
    realm: &RealmId,
    ctx: &TranslateContext,
    ar_ap: &ArApAccounts,
    as_of: NaiveDate,
    warnings: &mut Vec<String>,
) -> Result<Documents, SlimError> {
    let mut docs = Documents {
        receivable: Vec::new(),
        payable: Vec::new(),
        contracts: Vec::new(),
        qbo_balances: Vec::new(),
        later_activity: false,
    };
    let mut precision = 0usize;

    for entity_type in [
        EntityType::Invoice,
        EntityType::CreditMemo,
        EntityType::Payment,
        EntityType::Bill,
        EntityType::VendorCredit,
        EntityType::BillPayment,
        EntityType::Estimate,
        EntityType::PurchaseOrder,
    ] {
        for entity in entities(replica, realm, entity_type)? {
            let raw = &entity.raw_json;
            let translated = match translate(&entity, ctx) {
                Ok(t) => t,
                Err(import::TranslateError::Precision { .. }) => {
                    precision += 1;
                    continue;
                }
                Err(err) => {
                    warnings.push(format!("{} {}: {err}", entity_type.as_str(), entity.qbo_id));
                    continue;
                }
            };
            let mut doc = translated.document;
            if doc.is_voided {
                continue;
            }
            let dated_after = doc.txn_date > as_of;
            match entity_type {
                EntityType::Estimate | EntityType::PurchaseOrder => {
                    let status = if entity_type == EntityType::PurchaseOrder {
                        text(raw, "POStatus")
                    } else {
                        text(raw, "TxnStatus")
                    };
                    let open = match entity_type {
                        EntityType::PurchaseOrder => status.as_deref() != Some("Closed"),
                        _ => {
                            !matches!(status.as_deref(), Some("Closed" | "Rejected" | "Converted"))
                        }
                    };
                    if open && !dated_after {
                        docs.contracts.push(doc);
                    }
                    continue;
                }
                EntityType::Payment
                | EntityType::CreditMemo
                | EntityType::BillPayment
                | EntityType::VendorCredit
                    if dated_after =>
                {
                    docs.later_activity = true;
                }
                _ => {}
            }

            // A document whose translated total is not QBO's own (a discount
            // or group line the translation reads differently) is carried as
            // one line at QBO's total, so its open balance is QBO's.
            if matches!(
                doc.kind,
                DocKind::Invoice | DocKind::CreditMemo | DocKind::Bill | DocKind::VendorCredit
            ) {
                if let Some(total) = money(raw.get("TotalAmt")) {
                    if books::document_total(&doc)? != total {
                        warnings.push(format!(
                            "{} {}: lines and tax add to {}, QBO's total is {}; carried at QBO's total",
                            entity_type.as_str(),
                            entity.qbo_id,
                            format_money(books::document_total(&doc)?),
                            format_money(total)
                        ));
                        let account = control_account(doc.kind);
                        doc.lines = vec![account_line(1, account, total, "QBO total".into())];
                        doc.tax = None;
                    }
                }
            }

            if !dated_after {
                let balance_field = match doc.kind {
                    DocKind::Invoice | DocKind::Bill => money(raw.get("Balance")),
                    DocKind::CreditMemo => money(raw.get("RemainingCredit"))
                        .or_else(|| money(raw.get("Balance")))
                        .map(Money::checked_neg)
                        .transpose()?,
                    DocKind::VendorCredit => money(raw.get("Balance"))
                        .map(Money::checked_neg)
                        .transpose()?,
                    DocKind::Payment => money(raw.get("UnappliedAmt"))
                        .map(Money::checked_neg)
                        .transpose()?,
                    _ => None,
                };
                if let (Some(balance), Some(contact)) = (balance_field, &doc.contact) {
                    if !balance.is_zero() {
                        let book = if matches!(
                            doc.kind,
                            DocKind::Invoice | DocKind::CreditMemo | DocKind::Payment
                        ) {
                            Book::Receivable
                        } else {
                            Book::Payable
                        };
                        let due = match doc.kind {
                            DocKind::Invoice | DocKind::Bill => {
                                doc.due_date.unwrap_or(doc.txn_date)
                            }
                            _ => doc.txn_date,
                        };
                        docs.qbo_balances
                            .push((book, contact.id.clone(), due, balance));
                    }
                }
            }

            match doc.kind {
                DocKind::Invoice | DocKind::CreditMemo | DocKind::Payment => {
                    docs.receivable.push(doc)
                }
                _ => docs.payable.push(doc),
            }
        }
    }

    // AR and AP lines that journal entries and deposits carry for a named
    // customer or vendor are open items in QBO too.
    for entity_type in [EntityType::JournalEntry, EntityType::Deposit] {
        for entity in entities(replica, realm, entity_type)? {
            let items = control_lines(&entity, ar_ap, warnings);
            let Some(date) = text(&entity.raw_json, "TxnDate")
                .and_then(|d| NaiveDate::parse_from_str(&d, "%Y-%m-%d").ok())
            else {
                continue;
            };
            if text(&entity.raw_json, "TxnStatus").as_deref() == Some("Voided") {
                continue;
            }
            if date > as_of {
                if !items.is_empty() {
                    docs.later_activity = true;
                }
                continue;
            }
            let split = items.len() > 1;
            for ((book, contact_kind, contact_id), net) in items {
                if net.is_zero() {
                    continue;
                }
                let base = qbo_document_id(
                    if entity_type == EntityType::Deposit {
                        DocKind::Deposit
                    } else {
                        DocKind::JournalEntry
                    },
                    &entity.qbo_id,
                );
                let document_id = if split {
                    format!("{base}#{contact_id}")
                } else {
                    base
                };
                // AR: a debit is owed to us (an invoice), a credit is owed to
                // the customer (a credit). AP the other way round.
                let positive = !net.is_negative();
                let kind = match (book, positive) {
                    (Book::Receivable, true) => DocKind::Invoice,
                    (Book::Receivable, false) => DocKind::CreditMemo,
                    (Book::Payable, true) => DocKind::VendorCredit,
                    (Book::Payable, false) => DocKind::Bill,
                };
                let amount = if positive { net } else { net.checked_neg()? };
                let what = if entity_type == EntityType::Deposit {
                    "Deposit"
                } else {
                    "Journal entry"
                };
                let number = text(&entity.raw_json, "DocNumber");
                let doc = LedgerDocument {
                    document_id,
                    kind,
                    number: number.clone(),
                    txn_date: date,
                    due_date: None,
                    contact: Some(ContactRef {
                        kind: contact_kind,
                        id: contact_id.clone(),
                    }),
                    header_class: None,
                    lines: vec![account_line(
                        1,
                        control_account(kind),
                        amount,
                        format!(
                            "{what} {}: {} line for this {}",
                            number.as_deref().unwrap_or(&entity.qbo_id),
                            if book == Book::Receivable { "AR" } else { "AP" },
                            if contact_kind == ContactKind::Customer {
                                "customer"
                            } else {
                                "vendor"
                            },
                        ),
                    )],
                    tax: None,
                    deposit_to: None,
                    pay_from: None,
                    applications: Vec::new(),
                    unapplied: Money::ZERO,
                    is_voided: false,
                    source_ref: Some(entity.qbo_id.clone()),
                    memo: text(&entity.raw_json, "PrivateNote"),
                };
                match book {
                    Book::Receivable => docs.receivable.push(doc),
                    Book::Payable => docs.payable.push(doc),
                }
            }
        }
    }

    // A payment that applies one customer's share of a split journal entry
    // names the entry; point it at that customer's item.
    let split_ids: HashSet<String> = docs
        .receivable
        .iter()
        .chain(docs.payable.iter())
        .filter_map(|doc| {
            doc.document_id
                .split_once('#')
                .map(|(base, _)| base.to_string())
        })
        .collect();
    for doc in docs.receivable.iter_mut().chain(docs.payable.iter_mut()) {
        let Some(contact) = doc.contact.clone() else {
            continue;
        };
        for application in &mut doc.applications {
            if split_ids.contains(&application.target_document_id) {
                application.target_document_id =
                    format!("{}#{}", application.target_document_id, contact.id);
            }
        }
    }

    if precision > 0 {
        warnings.push(format!(
            "{precision} documents carry an amount with more than two decimal places and were \
             not read; any of them still open shows up in the aging check"
        ));
    }
    Ok(docs)
}

/// Per (book, contact) the signed net a journal entry or deposit puts on AR
/// or AP: AR debit-positive, AP debit-positive as well (a vendor credit).
fn control_lines(
    entity: &MirroredEntity,
    ar_ap: &ArApAccounts,
    warnings: &mut Vec<String>,
) -> BTreeMap<(Book, ContactKind, String), Money> {
    let mut nets: BTreeMap<(Book, ContactKind, String), Money> = BTreeMap::new();
    let Some(lines) = entity.raw_json.get("Line").and_then(Value::as_array) else {
        return nets;
    };
    let is_deposit = entity.entity_type == EntityType::Deposit;
    for line in lines {
        let detail = if is_deposit {
            line.get("DepositLineDetail")
        } else {
            line.get("JournalEntryLineDetail")
        };
        let Some(detail) = detail else { continue };
        let Some(account) = reference(detail, "AccountRef") else {
            continue;
        };
        let book = if ar_ap.receivable.contains(&account) {
            Book::Receivable
        } else if ar_ap.payable.contains(&account) {
            Book::Payable
        } else {
            continue;
        };
        let Some(amount) = money(line.get("Amount")) else {
            continue;
        };
        let signed = if is_deposit {
            // A deposit line is always a credit to the account it names.
            amount.checked_neg().unwrap_or(amount)
        } else {
            match text(detail, "PostingType").as_deref() {
                Some("Debit") => amount,
                Some("Credit") => amount.checked_neg().unwrap_or(amount),
                _ => continue,
            }
        };
        let entity_node = detail.get("Entity");
        let (contact_type, contact_id) = if is_deposit {
            (
                entity_node.and_then(|e| text(e, "type")),
                entity_node.and_then(|e| text(e, "value")),
            )
        } else {
            (
                entity_node.and_then(|e| text(e, "Type")),
                entity_node.and_then(|e| reference(e, "EntityRef")),
            )
        };
        let contact_kind = match contact_type.as_deref() {
            Some("Customer") => ContactKind::Customer,
            Some("Vendor") => ContactKind::Vendor,
            _ => {
                warnings.push(format!(
                    "{} {}: an {} line names no customer or vendor; it is in the opening entry \
                     but in nobody's aging",
                    entity.entity_type.as_str(),
                    entity.qbo_id,
                    if book == Book::Receivable { "AR" } else { "AP" }
                ));
                continue;
            }
        };
        let Some(contact_id) = contact_id else {
            continue;
        };
        let entry = nets
            .entry((book, contact_kind, contact_id))
            .or_insert(Money::ZERO);
        *entry = entry.checked_add(signed).unwrap_or(*entry);
    }
    nets
}

/// What will be saved: every document open on the date, in the shape it is
/// saved in.
struct Plan {
    open_ar: Vec<OpenDocument>,
    open_ap: Vec<OpenDocument>,
    /// Documents to save as open items (AR/AP), keyed by their id.
    items: Vec<LedgerDocument>,
    /// Customer payments with something unapplied on the date. Also in
    /// `items`.
    unapplied_payments: Vec<LedgerDocument>,
    contracts: Vec<LedgerDocument>,
}

fn plan_open_documents(
    docs: &Documents,
    as_of: NaiveDate,
    report: &mut SlimReport,
) -> Result<Plan, SlimError> {
    let names = HashMap::new();
    let dates: HashMap<&str, NaiveDate> = docs
        .receivable
        .iter()
        .chain(docs.payable.iter())
        .map(|doc| (doc.document_id.as_str(), doc.txn_date))
        .collect();

    // An application to a document dated after the as-of date was not made
    // yet on that date: it counts as unapplied.
    let unapplied_on = |doc: &LedgerDocument| -> Result<Money, MoneyError> {
        let mut unapplied = doc.unapplied;
        for application in &doc.applications {
            if dates
                .get(application.target_document_id.as_str())
                .is_some_and(|date| *date > as_of)
            {
                unapplied = unapplied.checked_add(application.amount)?;
            }
        }
        Ok(unapplied)
    };

    let mut payable = docs.payable.clone();
    for doc in payable
        .iter_mut()
        .filter(|d| d.kind == DocKind::BillPayment)
    {
        doc.unapplied = unapplied_on(doc)?;
    }

    let open_ar = books::open_items(&docs.receivable, Book::Receivable, as_of, &names)?;
    let open_ap = books::open_items(&payable, Book::Payable, as_of, &names)?;

    let by_id: HashMap<&str, &LedgerDocument> = docs
        .receivable
        .iter()
        .chain(payable.iter())
        .map(|doc| (doc.document_id.as_str(), doc))
        .collect();

    let mut items = Vec::new();
    let mut count = |what: &str| *report.open_counts.entry(what.to_string()).or_insert(0) += 1;
    for open in open_ar.iter().chain(open_ap.iter()) {
        let original = by_id[open.document_id.as_str()];
        let open_abs = abs(open.open)?;
        let total_abs = abs(open.total)?;
        let mut doc = original.clone();
        let source = if original.document_id.starts_with("qbo:JournalEntry:") {
            "journal entry line"
        } else if original.document_id.starts_with("qbo:Deposit:") {
            "deposit line"
        } else {
            original.kind.as_str()
        };
        if original.kind == DocKind::BillPayment {
            doc.lines = vec![description_line(open_abs, format!("Unapplied at {as_of}"))];
            doc.applications.clear();
            doc.unapplied = open_abs;
            count("bill_payment (unapplied)");
        } else if open_abs == total_abs && original.applications.is_empty() {
            count(source);
        } else {
            let applied = total_abs.checked_sub(open_abs)?;
            doc.lines = vec![account_line(
                1,
                control_account(original.kind),
                open_abs,
                format!(
                    "Open balance at {as_of} of {} {}: total {}, {} settled in QBO",
                    original.kind.as_str(),
                    original.number.as_deref().unwrap_or("(no number)"),
                    format_money(total_abs),
                    format_money(applied),
                ),
            )];
            doc.tax = None;
            doc.applications.clear();
            count(&format!("{source} (partly settled, one line)"));
        }
        items.push(doc);
    }

    let mut unapplied_payments = Vec::new();
    for payment in docs
        .receivable
        .iter()
        .filter(|d| d.kind == DocKind::Payment && d.txn_date <= as_of)
    {
        let unapplied = unapplied_on(payment)?;
        if unapplied.is_zero() {
            continue;
        }
        let mut doc = payment.clone();
        doc.lines = vec![description_line(unapplied, format!("Unapplied at {as_of}"))];
        doc.applications.clear();
        doc.unapplied = unapplied;
        count("payment (unapplied, in 2300)");
        unapplied_payments.push(doc.clone());
        items.push(doc);
    }

    for contract in &docs.contracts {
        count(contract.kind.as_str());
    }

    Ok(Plan {
        open_ar,
        open_ap,
        items,
        unapplied_payments,
        contracts: docs.contracts.clone(),
    })
}

fn save_open_documents(
    ledger: &Ledger,
    company: &str,
    plan: &Plan,
    as_of: NaiveDate,
    now: DateTime<Utc>,
    report: &mut SlimReport,
) -> Result<(), SlimError> {
    let mut current: HashSet<&str> = HashSet::new();
    let batches: [(&[LedgerDocument], &str); 2] = [
        (&plan.items, books::SLIM_OPEN_ITEM_KIND),
        (&plan.contracts, SLIM_CONTRACT_KIND),
    ];
    for (docs, kind) in batches {
        for doc in docs {
            current.insert(doc.document_id.as_str());
            if pipeline::document_unchanged(ledger, company, &doc.document_id, doc) {
                report.skipped_unchanged += 1;
                continue;
            }
            ledger.save_document(company, doc, command(kind, now), now)?;
            report.saved += 1;
        }
    }

    // An earlier run's open item that is no longer open on this run.
    for (document_id, kind) in slim_documents(ledger, company)? {
        if current.contains(document_id.as_str()) {
            continue;
        }
        let Some(raw) = ledger.latest_document_payload(company, &document_id)? else {
            continue;
        };
        let Ok(mut doc) = serde_json::from_str::<LedgerDocument>(&raw) else {
            report.warnings.push(format!(
                "{document_id}: saved payload does not parse; left alone"
            ));
            continue;
        };
        if doc.is_voided || doc.txn_date > as_of {
            continue;
        }
        doc.is_voided = true;
        ledger.save_document(company, &doc, command(&kind, now), now)?;
        report.voided_stale += 1;
    }
    Ok(())
}

/// Every document any slim run saved, with the oplog kind it was saved under.
fn slim_documents(ledger: &Ledger, company: &str) -> Result<Vec<(String, String)>, LedgerError> {
    let mut stmt = ledger
        .conn()
        .prepare(
            "SELECT DISTINCT dv.document_id, o.kind FROM document_versions dv JOIN oplog o
               ON o.company_id = dv.company_id AND o.command_id = dv.command_id
             WHERE dv.company_id = ?1 AND o.kind IN (?2, ?3)
             ORDER BY dv.document_id",
        )
        .map_err(LedgerError::from)?;
    let rows = stmt
        .query_map(
            rusqlite::params![company, books::SLIM_OPEN_ITEM_KIND, SLIM_CONTRACT_KIND],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(LedgerError::from)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(LedgerError::from)?);
    }
    Ok(out)
}

fn highest_doc_number(
    replica: &Store,
    realm: &RealmId,
    entity_type: EntityType,
) -> Result<Option<i64>, StoreError> {
    Ok(entities(replica, realm, entity_type)?
        .iter()
        .filter_map(|entity| text(&entity.raw_json, "DocNumber"))
        .filter(|n| !n.is_empty() && n.len() < 18 && n.bytes().all(|b| b.is_ascii_digit()))
        .filter_map(|n| n.parse::<i64>().ok())
        .max())
}

// ---------------------------------------------------------------------------
// Checks
// ---------------------------------------------------------------------------

type Buckets = [Money; 6];

fn run_checks(
    ledger: &Ledger,
    company: &str,
    options: &SlimOptions,
    docs: &Documents,
    plan: &Plan,
    qbo_net: Money,
    report: &mut SlimReport,
) -> Result<(), SlimError> {
    let as_of = options.as_of;
    let tb = report::trial_balance(ledger, company, as_of)?;
    let balance_of = |number: &str| {
        tb.rows
            .iter()
            .find(|row| row.number == number)
            .map_or(Money::ZERO, |row| row.balance)
    };

    report.checks.push(Check {
        name: "opening entry balances without a 3950 plug".into(),
        passed: qbo_net.is_zero(),
        details: if qbo_net.is_zero() {
            Vec::new()
        } else {
            vec![format!(
                "QBO's trial balance rows net to {} (debit-positive); the plug took it",
                format_money(qbo_net)
            )]
        },
    });
    let obe = balance_of(chart::OPENING_BALANCE_EQUITY);
    report.checks.push(Check {
        name: "3950 opening balance equity nets to zero".into(),
        passed: obe.is_zero(),
        details: if obe.is_zero() {
            Vec::new()
        } else {
            vec![format!(
                "3950 stands at {}: QBO's own opening balance equity, or the plug above",
                format_money(obe)
            )]
        },
    });

    let ar_aging = books::aging(ledger, company, Book::Receivable, as_of)?;
    let ap_aging = books::aging(ledger, company, Book::Payable, as_of)?;
    let ar_control = balance_of(chart::ACCOUNTS_RECEIVABLE);
    let ap_control = balance_of(chart::ACCOUNTS_PAYABLE).checked_neg()?;
    for (name, aging_total, control, account) in [
        (
            "AR open items tie to 1200",
            ar_aging.totals.total,
            ar_control,
            "1200",
        ),
        (
            "AP open items tie to 2000",
            ap_aging.totals.total,
            ap_control,
            "2000",
        ),
    ] {
        let passed = aging_total == control;
        report.checks.push(Check {
            name: name.into(),
            passed,
            details: if passed {
                Vec::new()
            } else {
                vec![format!(
                    "open items {} against {account} {} (difference {})",
                    format_money(aging_total),
                    format_money(control),
                    format_money(aging_total.checked_sub(control)?)
                )]
            },
        });
    }

    // The ledger's side, by contact, in QBO's terms: AR also carries the
    // payments the ledger holds in 2300.
    let mut ledger_ar = buckets_from_aging(&ar_aging);
    for payment in &plan.unapplied_payments {
        let Some(contact) = &payment.contact else {
            continue;
        };
        add_bucket(
            ledger_ar.entry(contact.id.clone()).or_default(),
            (as_of - payment.txn_date).num_days(),
            payment.unapplied.checked_neg()?,
        )?;
    }
    let ledger_ap = buckets_from_aging(&ap_aging);

    let names = contact_names(ledger, company)?;
    for (book, label, ledger_side, csv, open) in [
        (
            Book::Receivable,
            "AR aging by customer",
            &ledger_ar,
            &options.qbo_ar,
            &plan.open_ar,
        ),
        (
            Book::Payable,
            "AP aging by vendor",
            &ledger_ap,
            &options.qbo_ap,
            &plan.open_ap,
        ),
    ] {
        let (source, reference) = match csv {
            Some(rows) => ("QBO's aging report", Ok(buckets_from_csv(rows, &names)?)),
            None if docs.later_activity => (
                "the documents' Balance fields",
                Err(format!(
                    "documents dated after {as_of} have moved the replica's Balance fields; pass \
                     QBO's aging report as of {as_of} (--qbo-{})",
                    if book == Book::Receivable { "ar" } else { "ap" }
                )),
            ),
            None => (
                "the documents' Balance fields",
                Ok(buckets_from_balances(docs, open, book, as_of)?),
            ),
        };
        let name = format!("{label} equals QBO's ({source})");
        match reference {
            Err(reason) => report.checks.push(Check {
                name,
                passed: false,
                details: vec![reason],
            }),
            Ok(reference) => {
                let details = compare_buckets(ledger_side, &reference, &names)?;
                report.checks.push(Check {
                    name,
                    passed: details.is_empty(),
                    details,
                });
            }
        }
    }

    let diff = report::tb_diff(&tb, &options.qbo_tb, &TierRules::default())?;
    let details = diff
        .rows
        .iter()
        .filter(|row| diff.must_failures.contains(&row.label))
        .map(|row| {
            format!(
                "{}: ledger {} qbo {} delta {}",
                row.label,
                format_money(row.ledger_amount),
                format_money(row.qbo_amount),
                format_money(row.delta)
            )
        })
        .collect::<Vec<_>>();
    report.checks.push(Check {
        name: "trial balance equals QBO's on every must-tier line (§7)".into(),
        passed: details.is_empty(),
        details,
    });
    Ok(())
}

fn contact_names(ledger: &Ledger, company: &str) -> Result<HashMap<String, String>, BooksError> {
    Ok(books::list_contacts(ledger, company, None, None, true)?
        .into_iter()
        .map(|c| (c.contact_id, c.display_name))
        .collect())
}

fn buckets_from_aging(aging: &books::Aging) -> BTreeMap<String, Buckets> {
    aging
        .rows
        .iter()
        .map(|row| {
            (
                row.contact_id.clone(),
                [
                    row.current,
                    row.days_1_30,
                    row.days_31_60,
                    row.days_61_90,
                    row.over_90,
                    row.total,
                ],
            )
        })
        .collect()
}

fn add_bucket(buckets: &mut Buckets, days_past_due: i64, amount: Money) -> Result<(), MoneyError> {
    let mut row = books::AgingRow {
        current: buckets[0],
        days_1_30: buckets[1],
        days_31_60: buckets[2],
        days_61_90: buckets[3],
        over_90: buckets[4],
        total: buckets[5],
        ..books::AgingRow::default()
    };
    row.add(days_past_due, amount)?;
    *buckets = [
        row.current,
        row.days_1_30,
        row.days_31_60,
        row.days_61_90,
        row.over_90,
        row.total,
    ];
    Ok(())
}

/// QBO's aging rows by contact id: the row's own entity id, else the
/// contact whose display name it carries, else the name itself.
fn buckets_from_csv(
    rows: &[QboAgingRow],
    names: &HashMap<String, String>,
) -> Result<BTreeMap<String, Buckets>, MoneyError> {
    let by_name: HashMap<&str, &str> = names
        .iter()
        .map(|(id, name)| (name.as_str(), id.as_str()))
        .collect();
    let mut out: BTreeMap<String, Buckets> = BTreeMap::new();
    for row in rows {
        let key = row
            .entity_id
            .clone()
            .filter(|id| !id.is_empty())
            .or_else(|| by_name.get(row.name.as_str()).map(|id| id.to_string()))
            .unwrap_or_else(|| row.name.clone());
        let entry = out.entry(key).or_default();
        for (slot, amount) in entry.iter_mut().zip([
            row.current,
            row.days_1_30,
            row.days_31_60,
            row.days_61_90,
            row.over_90,
            row.total,
        ]) {
            *slot = slot.checked_add(amount)?;
        }
    }
    Ok(out)
}

/// QBO's open balances read off the documents themselves. Journal entry and
/// deposit lines carry no balance field; those come from the import's own
/// reckoning, so for them this check proves only that they were saved.
fn buckets_from_balances(
    docs: &Documents,
    open: &[OpenDocument],
    book: Book,
    as_of: NaiveDate,
) -> Result<BTreeMap<String, Buckets>, MoneyError> {
    let mut out: BTreeMap<String, Buckets> = BTreeMap::new();
    for (b, contact, due, balance) in &docs.qbo_balances {
        if *b == book {
            add_bucket(
                out.entry(contact.clone()).or_default(),
                (as_of - *due).num_days(),
                *balance,
            )?;
        }
    }
    for item in open.iter().filter(|item| {
        item.document_id.starts_with("qbo:JournalEntry:")
            || item.document_id.starts_with("qbo:Deposit:")
            || item.kind == DocKind::BillPayment
    }) {
        add_bucket(
            out.entry(item.contact_id.clone()).or_default(),
            (as_of - item.due_date).num_days(),
            item.open,
        )?;
    }
    Ok(out)
}

fn compare_buckets(
    ledger_side: &BTreeMap<String, Buckets>,
    reference: &BTreeMap<String, Buckets>,
    names: &HashMap<String, String>,
) -> Result<Vec<String>, MoneyError> {
    let keys: std::collections::BTreeSet<&String> =
        ledger_side.keys().chain(reference.keys()).collect();
    let zero = [Money::ZERO; 6];
    let mut details = Vec::new();
    let mut mismatches = 0usize;
    for key in keys {
        let ours = ledger_side.get(key).unwrap_or(&zero);
        let theirs = reference.get(key).unwrap_or(&zero);
        if ours == theirs {
            continue;
        }
        mismatches += 1;
        if details.len() < MAX_DETAILS {
            let render = |b: &Buckets| {
                b.iter()
                    .map(|m| format_money(*m))
                    .collect::<Vec<_>>()
                    .join(" / ")
            };
            details.push(format!(
                "{} ({key}): ledger {} | qbo {}",
                names.get(key).map(String::as_str).unwrap_or("?"),
                render(ours),
                render(theirs)
            ));
        }
    }
    if mismatches > details.len() {
        details.push(format!("… and {} more", mismatches - details.len()));
    }
    if mismatches > 0 {
        details.insert(
            0,
            "buckets: current / 1-30 / 31-60 / 61-90 / over 90 / total".into(),
        );
    }
    Ok(details)
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn command(kind: &str, now: DateTime<Utc>) -> CommandMeta {
    CommandMeta {
        actor_id: "import_slim".to_string(),
        kind: kind.to_string(),
        hlc: now.to_rfc3339(),
    }
}

/// 1200 for a receivable document, 2000 for a payable one.
fn control_account(kind: DocKind) -> &'static str {
    match kind {
        DocKind::Bill | DocKind::VendorCredit | DocKind::BillPayment => chart::ACCOUNTS_PAYABLE,
        _ => chart::ACCOUNTS_RECEIVABLE,
    }
}

fn account_line(line_no: i64, account: &str, amount: Money, description: String) -> DocLine {
    DocLine {
        line_no,
        kind: LineKind::Account,
        amount,
        class: None,
        item_id: None,
        account: Some(AccountId(account.to_string())),
        is_taxable: false,
        qty: None,
        unit_cost: None,
        description: Some(description),
        posting: None,
        entity: None,
    }
}

fn description_line(amount: Money, description: String) -> DocLine {
    DocLine {
        line_no: 1,
        kind: LineKind::Description,
        amount,
        class: None,
        item_id: None,
        account: None,
        is_taxable: false,
        qty: None,
        unit_cost: None,
        description: Some(description),
        posting: None,
        entity: None,
    }
}

fn abs(m: Money) -> Result<Money, MoneyError> {
    if m.is_negative() {
        m.checked_neg()
    } else {
        Ok(m)
    }
}

fn format_money(amount: Money) -> String {
    let minor = amount.minor();
    let sign = if minor < 0 { "-" } else { "" };
    let abs = minor.unsigned_abs();
    format!("{sign}{}.{:02}", abs / 100, abs % 100)
}

fn text(value: &Value, key: &str) -> Option<String> {
    match value.get(key)? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn nested_text(value: &Value, outer: &str, inner: &str) -> Option<String> {
    text(value.get(outer)?, inner)
}

/// `{"value": "..."}` under `key`.
fn reference(value: &Value, key: &str) -> Option<String> {
    nested_text(value, key, "value")
}

fn decimal(value: Option<&Value>) -> Option<Decimal> {
    match value? {
        Value::Number(n) => Decimal::from_str(&n.to_string())
            .or_else(|_| Decimal::from_scientific(&n.to_string()))
            .ok(),
        Value::String(s) => Decimal::from_str(s.trim()).ok(),
        _ => None,
    }
}

/// A QBO amount as money; `None` when absent or over two decimal places.
fn money(value: Option<&Value>) -> Option<Money> {
    round_money(decimal(value)?, RoundingPolicy::MirroredAmount).ok()
}

fn address(value: Option<&Value>) -> Option<String> {
    let value = value?;
    let mut parts: Vec<String> = ["Line1", "Line2", "Line3", "Line4", "Line5"]
        .iter()
        .filter_map(|key| text(value, key))
        .filter(|s| !s.trim().is_empty())
        .collect();
    let city_line = [
        text(value, "City"),
        text(value, "CountrySubDivisionCode"),
        text(value, "PostalCode"),
    ]
    .into_iter()
    .flatten()
    .filter(|s| !s.trim().is_empty())
    .collect::<Vec<_>>()
    .join(" ");
    if !city_line.is_empty() {
        parts.push(city_line);
    }
    (!parts.is_empty()).then(|| parts.join("\n"))
}

// ---------------------------------------------------------------------------
// The book the import writes into
// ---------------------------------------------------------------------------

/// Creates `company` (seeding its chart and classes, as `ledger init` does)
/// when the book does not have it yet; an existing company is left exactly
/// as it is. Returns whether it was created.
pub fn ensure_company(
    ledger: &Ledger,
    company: &str,
    realm: &RealmId,
    now: DateTime<Utc>,
) -> Result<bool, LedgerError> {
    let exists = ledger
        .conn()
        .query_row(
            "SELECT 1 FROM companies WHERE company_id = ?1",
            rusqlite::params![company],
            |_| Ok(()),
        )
        .map(|()| true)
        .or_else(|err| match err {
            rusqlite::Error::QueryReturnedNoRows => Ok(false),
            other => Err(LedgerError::from(other)),
        })?;
    if exists {
        return Ok(false);
    }
    ledger.create_company(company, company, Some(realm.as_str()), now)?;
    Ok(true)
}

/// A consistent copy of the whole book at `dest`, which must not exist yet:
/// the scratch book a dry run (and the rehearsal before a real run) writes
/// into, so the real book is never touched until the rehearsal passed.
pub fn copy_book(ledger: &Ledger, dest: &std::path::Path) -> Result<(), LedgerError> {
    ledger
        .conn()
        .execute(
            "VACUUM INTO ?1",
            rusqlite::params![dest.to_string_lossy().to_string()],
        )
        .map_err(LedgerError::from)?;
    Ok(())
}
