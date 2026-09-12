//! One document, translated. `LEDGER-DESIGN.md` §6, "What is already parsed,
//! and what the import must read from raw JSON".
//!
//! [`qbo_local::project::parse`] gives the header and lines. Everything §6's
//! table says is missing from the projection — tax detail, deposit and pay
//! accounts, linked-transaction amounts, line-level account refs, discount
//! and shipping detail, journal posting sides, deposit entities, taxability —
//! is read straight out of `entity.raw_json` here. Pure: no store, no
//! `post::PostingContext`. The posting function decides what a `None` class
//! or a missing mapping means; this module only ever reports what the
//! payload said.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use chrono::NaiveDate;
use ledger_core::{round_money, Money, RoundingPolicy};
use qbo_local::project::{ParsedEntity, ParsedItem, ParsedLine, Projection};
use qbo_local::store::MirroredEntity;
use rust_decimal::Decimal;
use serde_json::Value;
use thiserror::Error;

use crate::chart;
use crate::types::{
    AccountId, Application, ClassId, ContactKind, ContactRef, DocKind, DocLine, LedgerDocument,
    LineKind, Side, TaxDetail,
};

use super::accounts::AccountMapping;
use super::classes::ClassMapping;

/// A replica item plus the default class read off its own `ClassRef`, if QBO
/// gave it one. `qbo_local::project::ParsedItem` carries no class at all —
/// QBO items rarely carry one — so this is built by the driver from the raw
/// payload rather than living on `ParsedItem` itself.
#[derive(Clone, Debug, PartialEq)]
pub struct ItemContext {
    pub item: ParsedItem,
    pub default_class: Option<ClassId>,
}

/// What [`translate`] needs about the world beyond the one document: the
/// chart and class mappings, every item by QBO id, and which customers are
/// tax exempt. Built once by the driver from the replica's master entities.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TranslateContext {
    pub accounts: AccountMapping,
    pub classes: ClassMapping,
    pub items: HashMap<String, ItemContext>,
    pub customers_exempt: HashSet<String>,
}

/// Where a line's class came from — §3's default chain, made visible on every
/// line rather than only in the posted entry (W16: `class_source`).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ClassSource {
    /// The payload's own `ClassRef` on the line.
    Payload,
    /// No line class; the item's own default class was used instead (W16).
    ItemDefault,
    /// Neither the line nor its item carried a class; the header's did.
    Header,
}

/// The result of translating one replica document.
#[derive(Clone, Debug, PartialEq)]
pub struct Translated {
    pub document: LedgerDocument,
    /// One entry per line that ended up with a class, naming where it came
    /// from. A line absent from this list has no class at all — the posting
    /// function decides whether that is fatal (§3).
    pub class_sources: Vec<(i64, ClassSource)>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Error)]
pub enum TranslateError {
    #[error("{qbo_id}: quarantined by the replica projector: {reason}")]
    Quarantined { qbo_id: String, reason: String },
    #[error("{qbo_id}: this entity type has no document projection")]
    NotADocument { qbo_id: String },
    /// D9: an amount over two decimal places is never rounded silently.
    #[error("{qbo_id}: `{field}` carries more than two decimal places: {found}")]
    Precision {
        qbo_id: String,
        field: String,
        found: String,
    },
    #[error("{qbo_id}: txn_date {raw:?} is not a parseable date")]
    BadDate { qbo_id: String, raw: String },
    #[error("money arithmetic: {0}")]
    Money(#[from] ledger_core::MoneyError),
}

/// Translate one mirrored document entity into a [`Translated`].
///
/// `qbo_local::project::parse` supplies the header and lines; this reads the
/// rest — tax, applications, deposit/pay accounts, per-line account refs and
/// posting sides — from `entity.raw_json` (§6). Amounts convert through
/// `round_money(.., MirroredAmount)` (D8); anything over two decimal places
/// is `TranslateError::Precision` rather than rounded (D9) — except an item's
/// `unit_cost`, which is a documented exception: a bad cost warns and comes
/// back `None` rather than failing the whole document, because a missing unit
/// cost is recoverable at posting and a missing document is not.
pub fn translate(
    entity: &MirroredEntity,
    ctx: &TranslateContext,
) -> Result<Translated, TranslateError> {
    let qbo_id = entity.qbo_id.clone();
    let raw = &entity.raw_json;

    let doc = match qbo_local::project::parse(entity) {
        Projection::Parsed(boxed) => match *boxed {
            ParsedEntity::Document(document) => document,
            _ => return Err(TranslateError::NotADocument { qbo_id }),
        },
        Projection::NotProjected => return Err(TranslateError::NotADocument { qbo_id }),
        Projection::Quarantine(reason) => {
            return Err(TranslateError::Quarantined { qbo_id, reason })
        }
    };

    let doc_kind = dockind_of(doc.doc_type);
    let txn_date = NaiveDate::parse_from_str(&doc.txn_date, "%Y-%m-%d").map_err(|_| {
        TranslateError::BadDate {
            qbo_id: qbo_id.clone(),
            raw: doc.txn_date.clone(),
        }
    })?;
    let due_date = doc
        .due_date
        .as_deref()
        .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok());

    let contact = match (doc.contact_type, doc.contact_id.clone()) {
        (Some(kind), Some(id)) => Some(ContactRef {
            kind: contact_kind_of(kind),
            id,
        }),
        _ => None,
    };

    let mut warnings = Vec::new();

    let header_class = doc
        .class_id
        .as_deref()
        .and_then(|id| ctx.classes.by_source_ref(id))
        .map(|mapped| mapped.class_id.clone());
    if doc.class_id.is_some() && header_class.is_none() {
        warnings.push(format!(
            "header class {} has no ledger mapping",
            doc.class_id.as_deref().unwrap_or_default()
        ));
    }

    let raw_lines = filtered_raw_lines(raw);
    let mut doc_lines = Vec::with_capacity(doc.lines.len());
    let mut class_sources = Vec::with_capacity(doc.lines.len());

    for (parsed_line, raw_line) in doc.lines.iter().zip(raw_lines.iter()) {
        let detail_type = json_text(raw_line, "DetailType");
        let detail = detail_of(raw_line, detail_type.as_deref());

        let item = parsed_line
            .item_id
            .as_deref()
            .and_then(|id| ctx.items.get(id));
        let is_shipping_item = item.is_some_and(|context| {
            context.item.name.to_lowercase().contains("shipping")
                || context
                    .item
                    .sku
                    .as_deref()
                    .is_some_and(|sku| sku.to_lowercase().contains("shipping"))
        });

        let kind = if detail_type.as_deref() == Some("ShippingLineDetail") || is_shipping_item {
            LineKind::Shipping
        } else {
            match detail_type.as_deref() {
                Some("AccountBasedExpenseLineDetail") | Some("DepositLineDetail") => {
                    LineKind::Account
                }
                Some("SalesItemLineDetail") | Some("ItemBasedExpenseLineDetail") => LineKind::Item,
                Some("DiscountLineDetail") => LineKind::Discount,
                Some("JournalEntryLineDetail") => LineKind::Journal,
                Some("DescriptionOnly") => LineKind::Description,
                _ if parsed_line.item_id.is_some() => LineKind::Item,
                _ => LineKind::Description,
            }
        };

        let account = match kind {
            LineKind::Account | LineKind::Journal => {
                let account_ref = detail.and_then(|d| json_reference(d, "AccountRef"));
                match account_ref {
                    Some(id) => match ctx.accounts.by_source_ref(&id) {
                        Some(mapped) => Some(mapped.ledger_number.clone()),
                        None => {
                            warnings.push(format!(
                                "line {}: account {id} has no ledger mapping",
                                parsed_line.line_no
                            ));
                            None
                        }
                    },
                    None => {
                        warnings.push(format!(
                            "line {}: {} line names no AccountRef",
                            parsed_line.line_no,
                            detail_type.as_deref().unwrap_or("unknown")
                        ));
                        None
                    }
                }
            }
            _ => None,
        };

        let posting = if kind == LineKind::Journal {
            detail
                .and_then(|d| json_text(d, "PostingType"))
                .and_then(|posting_type| match posting_type.as_str() {
                    "Debit" => Some(Side::Debit),
                    "Credit" => Some(Side::Credit),
                    _ => None,
                })
        } else {
            None
        };

        let entity_ref = if detail_type.as_deref() == Some("DepositLineDetail") {
            detail.and_then(deposit_line_entity)
        } else {
            None
        };

        let unit_cost = if kind == LineKind::Item {
            item.and_then(|context| context.item.purchase_cost).and_then(|cost| {
                if cost.scale() > 2 {
                    warnings.push(format!(
                        "line {}: item {} purchase cost carries more than two decimal places; unit_cost left unset",
                        parsed_line.line_no,
                        parsed_line.item_id.clone().unwrap_or_default()
                    ));
                    None
                } else {
                    round_money(cost, RoundingPolicy::MirroredAmount).ok()
                }
            })
        } else {
            None
        };

        // §3: only a line that touches income, COGS or a class-consuming
        // expense carries a class. Balance sheet lines (`Account`) and
        // description-only lines are not asked for one, and are not warned
        // about lacking one.
        let (class, source) = if matches!(
            kind,
            LineKind::Item | LineKind::Discount | LineKind::Shipping | LineKind::Journal
        ) {
            resolve_line_class(parsed_line, item, &header_class, ctx, &mut warnings)
        } else {
            (None, None)
        };
        if let Some(source) = source {
            class_sources.push((parsed_line.line_no, source));
        }

        doc_lines.push(DocLine {
            line_no: parsed_line.line_no,
            kind,
            amount: parsed_line.amount,
            class,
            item_id: parsed_line.item_id.clone(),
            account,
            is_taxable: parsed_line.is_taxable,
            qty: parsed_line.qty,
            unit_cost,
            description: parsed_line.description.clone(),
            posting,
            entity: entity_ref,
        });
    }

    // A header `ShipAmt` with no explicit shipping line becomes a synthetic
    // Shipping line, per §6's line-detection rule.
    if let Some(ship_amount) = json_money(raw, "ShipAmt", &qbo_id)? {
        if !ship_amount.is_zero() {
            let line_no = doc_lines.iter().map(|line| line.line_no).max().unwrap_or(0) + 1;
            doc_lines.push(DocLine {
                line_no,
                kind: LineKind::Shipping,
                amount: ship_amount,
                class: header_class.clone(),
                item_id: None,
                account: None,
                is_taxable: false,
                qty: None,
                unit_cost: None,
                description: Some("Shipping".to_string()),
                posting: None,
                entity: None,
            });
            if header_class.is_some() {
                class_sources.push((line_no, ClassSource::Header));
            }
        }
    }

    let tax = parse_tax_detail(raw, &doc.lines, &qbo_id)?;

    let deposit_to = json_reference(raw, "DepositToAccountRef").and_then(|id| {
        match ctx.accounts.by_source_ref(&id) {
            Some(mapped) => Some(mapped.ledger_number.clone()),
            None => {
                warnings.push(format!("deposit-to account {id} has no ledger mapping"));
                None
            }
        }
    });

    let pay_from = resolve_pay_from(raw, doc_kind, &ctx.accounts, &mut warnings);
    let applications = collect_applications(raw, &mut warnings);
    let unapplied = json_money(raw, "UnappliedAmt", &qbo_id)?.unwrap_or(Money::ZERO);
    let is_voided = entity.is_deleted || json_text(raw, "TxnStatus").as_deref() == Some("Voided");
    let document_id = format!("qbo:{}:{}", entity.entity_type.as_str(), qbo_id);
    let memo = doc
        .private_note
        .clone()
        .or_else(|| doc.customer_memo.clone());

    let document = LedgerDocument {
        document_id,
        kind: doc_kind,
        number: doc.doc_number.clone(),
        txn_date,
        due_date,
        contact,
        header_class,
        lines: doc_lines,
        tax,
        deposit_to,
        pay_from,
        applications,
        unapplied,
        is_voided,
        source_ref: Some(qbo_id),
        memo,
    };

    Ok(Translated {
        document,
        class_sources,
        warnings,
    })
}

/// §3's default chain: payload `ClassRef`, then the item's own default class
/// (W16, `ClassSource::ItemDefault`), then the header's class, then none.
fn resolve_line_class(
    parsed_line: &ParsedLine,
    item: Option<&ItemContext>,
    header_class: &Option<ClassId>,
    ctx: &TranslateContext,
    warnings: &mut Vec<String>,
) -> (Option<ClassId>, Option<ClassSource>) {
    if let Some(class_id) = parsed_line.class_id.as_deref() {
        return match ctx.classes.by_source_ref(class_id) {
            Some(mapped) => (Some(mapped.class_id.clone()), Some(ClassSource::Payload)),
            None => {
                warnings.push(format!(
                    "line {}: class {class_id} has no ledger mapping",
                    parsed_line.line_no
                ));
                (None, None)
            }
        };
    }
    if let Some(default_class) = item.and_then(|context| context.default_class.clone()) {
        return (Some(default_class), Some(ClassSource::ItemDefault));
    }
    if let Some(header) = header_class.clone() {
        return (Some(header), Some(ClassSource::Header));
    }
    warnings.push(format!(
        "line {}: no class from the payload, the item's default, or the header",
        parsed_line.line_no
    ));
    (None, None)
}

fn deposit_line_entity(detail: &Value) -> Option<ContactRef> {
    let id = json_reference(detail, "Entity")?;
    let kind = match json_nested_text(detail, "Entity", "type")?.as_str() {
        "Customer" => ContactKind::Customer,
        "Vendor" => ContactKind::Vendor,
        _ => return None,
    };
    Some(ContactRef { kind, id })
}

fn parse_tax_detail(
    raw: &Value,
    lines: &[ParsedLine],
    qbo_id: &str,
) -> Result<Option<TaxDetail>, TranslateError> {
    let Some(detail) = raw.get("TxnTaxDetail") else {
        return Ok(None);
    };
    let total_tax = json_money(detail, "TotalTax", qbo_id)?.unwrap_or(Money::ZERO);
    let tax_lines = detail.get("TaxLine").and_then(Value::as_array);

    let taxable_base = match tax_lines {
        Some(entries) if !entries.is_empty() => {
            let mut sum = Decimal::ZERO;
            let mut any = false;
            for entry in entries {
                if let Some(net) = entry
                    .get("TaxLineDetail")
                    .and_then(|d| d.get("NetAmountTaxable"))
                    .and_then(json_decimal)
                {
                    sum += net;
                    any = true;
                }
            }
            if any {
                if sum.scale() > 2 {
                    return Err(TranslateError::Precision {
                        qbo_id: qbo_id.to_string(),
                        field: "TaxLineDetail.NetAmountTaxable".to_string(),
                        found: sum.to_string(),
                    });
                }
                round_money(sum, RoundingPolicy::MirroredAmount)?
            } else {
                taxable_line_sum(lines)?
            }
        }
        _ => taxable_line_sum(lines)?,
    };

    let rate = tax_lines
        .and_then(|entries| entries.first())
        .and_then(|entry| entry.get("TaxLineDetail"))
        .and_then(|detail| detail.get("TaxPercent"))
        .and_then(json_decimal)
        .map(|percent| percent / Decimal::from(100))
        .unwrap_or_else(|| Decimal::new(6625, 5));

    Ok(Some(TaxDetail {
        total_tax,
        taxable_base,
        rate,
    }))
}

fn taxable_line_sum(lines: &[ParsedLine]) -> Result<Money, TranslateError> {
    Ok(Money::checked_sum(
        lines
            .iter()
            .filter(|line| line.is_taxable)
            .map(|line| line.amount),
    )?)
}

fn resolve_pay_from(
    raw: &Value,
    doc_kind: DocKind,
    accounts: &AccountMapping,
    warnings: &mut Vec<String>,
) -> Option<AccountId> {
    match doc_kind {
        DocKind::BillPayment => {
            if let Some(detail) = raw.get("CreditCardPayment") {
                let id = json_reference(detail, "CCAccountRef");
                return Some(resolve_pay_account(
                    id,
                    accounts,
                    chart::CREDIT_CARD,
                    warnings,
                ));
            }
            if let Some(detail) = raw.get("CheckPayment") {
                let id = json_reference(detail, "BankAccountRef");
                return Some(resolve_pay_account(id, accounts, chart::CHECKING, warnings));
            }
            None
        }
        DocKind::Purchase => match json_text(raw, "PaymentType").as_deref() {
            Some("CreditCard") => {
                let id = json_reference(raw, "AccountRef");
                Some(resolve_pay_account(
                    id,
                    accounts,
                    chart::CREDIT_CARD,
                    warnings,
                ))
            }
            Some("Check") | Some("Cash") => {
                let id = json_reference(raw, "AccountRef");
                Some(resolve_pay_account(id, accounts, chart::CHECKING, warnings))
            }
            _ => None,
        },
        _ => None,
    }
}

fn resolve_pay_account(
    id: Option<String>,
    accounts: &AccountMapping,
    default_number: &str,
    warnings: &mut Vec<String>,
) -> AccountId {
    match id.and_then(|id| {
        accounts
            .by_source_ref(&id)
            .cloned()
            .map(|mapped| (id, mapped))
    }) {
        Some((_, mapped)) => mapped.ledger_number,
        None => {
            warnings.push(format!(
                "pay-from account has no ledger mapping; defaulting to {default_number}"
            ));
            AccountId(default_number.to_string())
        }
    }
}

fn collect_applications(raw: &Value, warnings: &mut Vec<String>) -> Vec<Application> {
    let mut applications = Vec::new();
    let Some(lines) = raw.get("Line").and_then(Value::as_array) else {
        return applications;
    };
    for line in lines {
        if json_text(line, "DetailType").as_deref() == Some("SubTotalLineDetail") {
            continue;
        }
        let Some(linked) = line.get("LinkedTxn").and_then(Value::as_array) else {
            continue;
        };
        for link in linked {
            let (Some(txn_id), Some(txn_type)) =
                (json_text(link, "TxnId"), json_text(link, "TxnType"))
            else {
                continue;
            };
            let amount = link
                .get("Amount")
                .and_then(json_decimal)
                .and_then(|d| round_money(d, RoundingPolicy::MirroredAmount).ok())
                .unwrap_or(Money::ZERO);
            match map_txn_type(&txn_type) {
                Some(target_kind) => applications.push(Application {
                    target_document_id: txn_id,
                    target_kind,
                    amount,
                }),
                // D11: an unrecognized link type is reported, never dropped silently.
                None => warnings.push(format!(
                    "linked transaction {txn_id} has type {txn_type:?}, which has no ledger document kind; not applied"
                )),
            }
        }
    }
    applications
}

fn map_txn_type(raw: &str) -> Option<DocKind> {
    match raw {
        "Estimate" => Some(DocKind::Estimate),
        "Invoice" => Some(DocKind::Invoice),
        "SalesReceipt" => Some(DocKind::SalesReceipt),
        "CreditMemo" => Some(DocKind::CreditMemo),
        "RefundReceipt" => Some(DocKind::RefundReceipt),
        "Payment" => Some(DocKind::Payment),
        "PurchaseOrder" => Some(DocKind::PurchaseOrder),
        "Bill" => Some(DocKind::Bill),
        "BillPayment" | "BillPaymentCheck" | "BillPaymentCreditCard" => Some(DocKind::BillPayment),
        "VendorCredit" => Some(DocKind::VendorCredit),
        "Purchase" | "Check" | "CreditCardCredit" | "Expense" => Some(DocKind::Purchase),
        "Deposit" => Some(DocKind::Deposit),
        "JournalEntry" => Some(DocKind::JournalEntry),
        _ => None,
    }
}

fn dockind_of(doc_type: qbo_local::domain::DocumentType) -> DocKind {
    use qbo_local::domain::DocumentType as D;
    match doc_type {
        D::Estimate => DocKind::Estimate,
        D::Invoice => DocKind::Invoice,
        D::SalesReceipt => DocKind::SalesReceipt,
        D::CreditMemo => DocKind::CreditMemo,
        D::RefundReceipt => DocKind::RefundReceipt,
        D::Payment => DocKind::Payment,
        D::PurchaseOrder => DocKind::PurchaseOrder,
        D::Bill => DocKind::Bill,
        D::BillPayment => DocKind::BillPayment,
        D::VendorCredit => DocKind::VendorCredit,
        D::Purchase => DocKind::Purchase,
        D::Deposit => DocKind::Deposit,
        D::JournalEntry => DocKind::JournalEntry,
    }
}

fn contact_kind_of(kind: qbo_local::domain::ContactType) -> ContactKind {
    match kind {
        qbo_local::domain::ContactType::Customer => ContactKind::Customer,
        qbo_local::domain::ContactType::Vendor => ContactKind::Vendor,
    }
}

/// The same subtotal-skipping filter `qbo_local::project::parse_lines` uses,
/// so this list zips 1:1 against `ParsedDocument::lines`.
fn filtered_raw_lines(raw: &Value) -> Vec<&Value> {
    let Some(lines) = raw.get("Line").and_then(Value::as_array) else {
        return Vec::new();
    };
    lines
        .iter()
        .filter(|line| json_text(line, "DetailType").as_deref() != Some("SubTotalLineDetail"))
        .collect()
}

/// As `qbo_local::project::detail_object`: follow `DetailType`, falling back
/// to whichever key ends in `LineDetail`.
fn detail_of<'a>(line: &'a Value, detail_type: Option<&str>) -> Option<&'a Value> {
    if let Some(key) = detail_type {
        if let Some(found) = line.get(key) {
            return Some(found);
        }
    }
    line.as_object()?
        .iter()
        .find(|(key, _)| key.ends_with("LineDetail"))
        .map(|(_, value)| value)
}

fn json_text(value: &Value, key: &str) -> Option<String> {
    match value.get(key)? {
        Value::String(s) => {
            let trimmed = s.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

pub(super) fn json_nested_text(value: &Value, outer: &str, inner: &str) -> Option<String> {
    json_text(value.get(outer)?, inner)
}

pub(super) fn json_reference(value: &Value, key: &str) -> Option<String> {
    json_nested_text(value, key, "value")
}

fn json_decimal(value: &Value) -> Option<Decimal> {
    match value {
        Value::Number(n) => Decimal::from_str(&n.to_string()).ok(),
        Value::String(s) => Decimal::from_str(s.trim()).ok(),
        _ => None,
    }
}

fn json_money(value: &Value, key: &str, qbo_id: &str) -> Result<Option<Money>, TranslateError> {
    let Some(found) = value.get(key) else {
        return Ok(None);
    };
    let Some(decimal) = json_decimal(found) else {
        return Ok(None);
    };
    if decimal.scale() > 2 {
        return Err(TranslateError::Precision {
            qbo_id: qbo_id.to_string(),
            field: key.to_string(),
            found: decimal.to_string(),
        });
    }
    Ok(Some(round_money(decimal, RoundingPolicy::MirroredAmount)?))
}
