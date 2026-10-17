//! The import driver. `LEDGER-DESIGN.md` §6.
//!
//! A pure translator plus a walk: it never opens the ledger store and never
//! calls the posting function — those are built in parallel — so its only
//! output is the `Translated` documents it hands to `sink`. Idempotent by
//! construction: masters are read fresh into the mappings, documents are
//! walked in a fixed type order and sorted by `(txn_date, qbo_id)`, so the
//! same replica state always produces the same sequence.

use std::collections::{HashMap, HashSet};

use chrono::NaiveDate;
use qbo_local::domain::{DocumentType, EntityType, RealmId};
use qbo_local::project::{ParsedAccount, ParsedClass, ParsedEntity, Projection};
use qbo_local::store::{Store, StoreError};
use serde_json::Value;
use thiserror::Error;

use crate::chart;
use crate::types::{ClassId, CompanyId, Side};

use super::accounts::map_accounts;
use super::classes::{map_classes, ClassMapping};
use super::translate::{
    json_reference, translate, ClassFallback, ItemContext, TranslateContext, TranslateError,
    Translated,
};

/// Masters first (built into the mappings below), then documents in this
/// order — the §1 table's order, which is also `DocKind::ALL`'s prefix.
const DOCUMENT_TYPE_ORDER: &[DocumentType] = &[
    DocumentType::Estimate,
    DocumentType::Invoice,
    DocumentType::SalesReceipt,
    DocumentType::CreditMemo,
    DocumentType::RefundReceipt,
    DocumentType::Payment,
    DocumentType::PurchaseOrder,
    DocumentType::Bill,
    DocumentType::BillPayment,
    DocumentType::VendorCredit,
    DocumentType::Purchase,
    DocumentType::Deposit,
    DocumentType::JournalEntry,
    DocumentType::Transfer,
    DocumentType::CreditCardPayment,
    DocumentType::InventoryAdjustment,
];

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ImportOptions {
    pub from: Option<NaiveDate>,
    pub to: Option<NaiveDate>,
    /// D28's import-only fallback class. `None` leaves every unclassed line
    /// for the posting gate to refuse.
    pub class_fallback: Option<ClassFallback>,
}

impl ImportOptions {
    /// What `ledger import`, the nightly run and the boundary walk use: the
    /// given window, plus D28's fallback class for anything dated before the
    /// new books open on 1 January 2027 (D24).
    pub fn with_d28_fallback(from: Option<NaiveDate>, to: Option<NaiveDate>) -> Self {
        ImportOptions {
            from,
            to,
            class_fallback: Some(ClassFallback {
                class: ClassId(chart::IMPORT_FALLBACK_CLASS.0.to_string()),
                before: NaiveDate::from_ymd_opt(2027, 1, 1).expect("1 January 2027 is a date"),
            }),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ImportReport {
    pub translated: usize,
    pub voided: usize,
    /// QBO ids that failed with `TranslateError::Precision`.
    pub precision_failures: Vec<String>,
    pub warnings: Vec<String>,
    /// QBO class ids that did not match a §3 class (§7-style reporting).
    pub unmatched_classes: Vec<String>,
    /// QBO account ids materialised in an x990 slot (§2, `needs_mapping`).
    pub accounts_needing_mapping: Vec<String>,
}

#[derive(Debug, Error)]
pub enum ImportError {
    #[error("replica store: {0}")]
    Store(#[from] StoreError),
    #[error("sink rejected a translated document: {0}")]
    Sink(Box<dyn std::error::Error>),
    /// Opening balances only (§6): a QBO trial balance row names an account
    /// the chart mapping never resolved.
    #[error("opening balance: no ledger mapping for QBO account {0}")]
    UnmappedAccount(String),
    #[error("money arithmetic: {0}")]
    Money(#[from] ledger_core::MoneyError),
}

/// Walk the replica and hand every non-estimate, non-purchase-order document
/// version to `sink`, translated. Masters (accounts, classes, items,
/// customers) are read once up front to build the mappings every document
/// translates against.
pub fn run(
    store: &Store,
    realm: &RealmId,
    company: &CompanyId,
    options: &ImportOptions,
    sink: &mut dyn FnMut(Translated) -> Result<(), Box<dyn std::error::Error>>,
) -> Result<ImportReport, ImportError> {
    let replica_accounts = parsed_accounts(store, realm)?;
    let journal_ar_ap_sides = build_journal_ar_ap_sides(store, realm, &replica_accounts)?;
    let accounts = map_accounts(&replica_accounts);
    let classes = map_classes(&parsed_classes(store, realm)?, company);
    let items = build_item_contexts(store, realm, &classes)?;
    let customers_exempt = build_exempt_customers(store, realm)?;

    let mut report = ImportReport {
        unmatched_classes: classes.unmatched.clone(),
        accounts_needing_mapping: accounts
            .mapped
            .iter()
            .filter(|mapped| mapped.needs_mapping)
            .map(|mapped| mapped.source_ref.clone())
            .collect(),
        ..ImportReport::default()
    };

    let ctx = TranslateContext {
        accounts,
        classes,
        items,
        customers_exempt,
        class_fallback: options.class_fallback.clone(),
        journal_ar_ap_sides,
    };

    let mut ordered: Vec<(NaiveDate, String, Translated)> = Vec::new();

    for doc_type in DOCUMENT_TYPE_ORDER {
        let entity_type = doc_type.as_entity();
        for entry in store.entity_index(realm, entity_type)? {
            let Some(entity) = store.get_entity(realm, entity_type, &entry.qbo_id)? else {
                continue;
            };
            match translate(&entity, &ctx) {
                Ok(translated) => {
                    let date = translated.document.txn_date;
                    if options.from.is_some_and(|from| date < from) {
                        continue;
                    }
                    if options.to.is_some_and(|to| date > to) {
                        continue;
                    }
                    report.warnings.extend(translated.warnings.iter().cloned());
                    ordered.push((date, entry.qbo_id.clone(), translated));
                }
                Err(TranslateError::Precision { qbo_id, .. }) => {
                    report.precision_failures.push(qbo_id);
                }
                Err(other) => {
                    report.warnings.push(format!("{}: {other}", entry.qbo_id));
                }
            }
        }
    }

    ordered.sort_by(|a, b| (a.0, a.1.as_str()).cmp(&(b.0, b.1.as_str())));

    for (_, _, translated) in ordered {
        if translated.document.is_voided {
            report.voided += 1;
        }
        report.translated += 1;
        sink(translated).map_err(ImportError::Sink)?;
    }

    Ok(report)
}

pub(crate) fn parsed_accounts(
    store: &Store,
    realm: &RealmId,
) -> Result<Vec<ParsedAccount>, ImportError> {
    let mut out = Vec::new();
    for entry in store.entity_index(realm, EntityType::Account)? {
        let Some(entity) = store.get_entity(realm, EntityType::Account, &entry.qbo_id)? else {
            continue;
        };
        if let Projection::Parsed(parsed) = qbo_local::project::parse(&entity) {
            if let ParsedEntity::Account(account) = *parsed {
                out.push(account);
            }
        }
    }
    Ok(out)
}

pub(crate) fn parsed_classes(
    store: &Store,
    realm: &RealmId,
) -> Result<Vec<ParsedClass>, ImportError> {
    let mut out = Vec::new();
    for entry in store.entity_index(realm, EntityType::Class)? {
        let Some(entity) = store.get_entity(realm, EntityType::Class, &entry.qbo_id)? else {
            continue;
        };
        if let Projection::Parsed(parsed) = qbo_local::project::parse(&entity) {
            if let ParsedEntity::Class(class) = *parsed {
                out.push(class);
            }
        }
    }
    Ok(out)
}

pub(crate) fn build_item_contexts(
    store: &Store,
    realm: &RealmId,
    classes: &ClassMapping,
) -> Result<HashMap<String, ItemContext>, ImportError> {
    let mut items = HashMap::new();
    for entry in store.entity_index(realm, EntityType::Item)? {
        let Some(entity) = store.get_entity(realm, EntityType::Item, &entry.qbo_id)? else {
            continue;
        };
        let Projection::Parsed(parsed) = qbo_local::project::parse(&entity) else {
            continue;
        };
        let ParsedEntity::Item(item) = *parsed else {
            continue;
        };
        let default_class = json_reference(&entity.raw_json, "ClassRef")
            .and_then(|id| classes.by_source_ref(&id))
            .map(|mapped| mapped.class_id.clone());
        items.insert(
            item.qbo_id.clone(),
            ItemContext {
                item,
                default_class,
            },
        );
    }
    Ok(items)
}

/// For every JournalEntry with a line on an AR or AP account, the side its
/// AR/AP lines net to, per entity on those lines (`(je_id, entity_id)`) and
/// across the whole entry (`(je_id, "")`). A payment line linked to such an
/// entry has no sign of its own (D29); this is where it comes from. Per
/// entity, because one entry often moves AR between customers.
fn build_journal_ar_ap_sides(
    store: &Store,
    realm: &RealmId,
    accounts: &[ParsedAccount],
) -> Result<HashMap<(String, String), Side>, ImportError> {
    let ar_ap: HashSet<&str> = accounts
        .iter()
        .filter(|account| {
            let kind = account
                .account_type
                .as_deref()
                .unwrap_or_default()
                .replace(' ', "")
                .to_ascii_lowercase();
            kind == "accountsreceivable" || kind == "accountspayable"
        })
        .map(|account| account.qbo_id.as_str())
        .collect();

    let mut sides = HashMap::new();
    for entry in store.entity_index(realm, EntityType::JournalEntry)? {
        let Some(entity) = store.get_entity(realm, EntityType::JournalEntry, &entry.qbo_id)? else {
            continue;
        };
        let Some(lines) = entity.raw_json.get("Line").and_then(Value::as_array) else {
            continue;
        };
        let mut nets: HashMap<String, rust_decimal::Decimal> = HashMap::new();
        for line in lines {
            let Some(detail) = line.get("JournalEntryLineDetail") else {
                continue;
            };
            let Some(account) = json_reference(detail, "AccountRef") else {
                continue;
            };
            if !ar_ap.contains(account.as_str()) {
                continue;
            }
            let amount = line
                .get("Amount")
                .and_then(|value| value.to_string().parse::<rust_decimal::Decimal>().ok())
                .unwrap_or_default();
            let signed = match detail.get("PostingType").and_then(Value::as_str) {
                Some("Debit") => amount,
                Some("Credit") => -amount,
                _ => continue,
            };
            let entity = detail
                .get("Entity")
                .and_then(|entity| json_reference(entity, "EntityRef"))
                .unwrap_or_default();
            *nets.entry(String::new()).or_default() += signed;
            if !entity.is_empty() {
                *nets.entry(entity).or_default() += signed;
            }
        }
        for (entity, net) in nets {
            if net.is_zero() {
                continue;
            }
            let side = if net.is_sign_positive() {
                Side::Debit
            } else {
                Side::Credit
            };
            sides.insert((entry.qbo_id.clone(), entity), side);
        }
    }
    Ok(sides)
}

/// §6: `Customer.Taxable == false` or a `ResaleNum` on file — a customer
/// exempt from tax, for the report layer that is out of scope here.
pub(crate) fn build_exempt_customers(
    store: &Store,
    realm: &RealmId,
) -> Result<HashSet<String>, ImportError> {
    let mut exempt = HashSet::new();
    for entry in store.entity_index(realm, EntityType::Customer)? {
        let Some(entity) = store.get_entity(realm, EntityType::Customer, &entry.qbo_id)? else {
            continue;
        };
        let raw = &entity.raw_json;
        let taxable: Option<bool> = raw.get("Taxable").and_then(Value::as_bool);
        let has_resale_number = raw.get("ResaleNum").is_some();
        if taxable == Some(false) || has_resale_number {
            exempt.insert(entry.qbo_id.clone());
        }
    }
    Ok(exempt)
}
