//! Sales orders in the own book (DECISIONS.md D42).
//!
//! A sales order is a [`DocKind::SalesOrder`] document: numbered like an
//! invoice, naming a customer, never posting ([`DocKind::is_contract`]). It
//! carries no status of its own. What closes it is the invoice made from it:
//! that invoice carries an [`Application`] targeting the order, for the
//! pre-tax subtotal it took, which moves no money anywhere
//! (`books::open_items` and `post` both ignore applications to a contract)
//! and is the link back. So
//!
//! - **open** = the order's pre-tax subtotal less what the non-voided
//!   invoices linked to it took, when that is above zero and the order is
//!   not voided;
//! - **converting** ([`invoice_from_sales_order`]) builds that invoice from
//!   the order's lines, every one by default or the named ones (optionally
//!   at a smaller quantity), and the caller saves and posts it like any
//!   other invoice. Converting the whole order closes it;
//! - **closing a remainder** nobody will invoice is voiding the order
//!   (`void_document`), which keeps the invoices it already produced.

use std::collections::HashMap;

use chrono::NaiveDate;
use ledger_core::{round_money, Money, MoneyError, RoundingPolicy};
use rust_decimal::Decimal;
use serde::Serialize;
use thiserror::Error;

use crate::books::{self, BooksError};
use crate::store::Ledger;
use crate::types::{Application, DocKind, DocLine, LedgerDocument, LineKind};

#[derive(Debug, Error)]
pub enum SalesOrderError {
    #[error(transparent)]
    Books(#[from] BooksError),
    #[error(transparent)]
    Money(#[from] MoneyError),
    #[error("{0} is a {1}, not a sales order")]
    NotASalesOrder(String, &'static str),
    #[error("sales order {0} is voided")]
    Voided(String),
    #[error("sales order {0} has nothing left to invoice")]
    NothingOpen(String),
    #[error(
        "sales order {0} is already partly invoiced; name the lines to invoice (`lines`) so \
         nothing is invoiced twice"
    )]
    PartlyInvoiced(String),
    #[error("sales order {id} has no line {line_no}")]
    NoSuchLine { id: String, line_no: i64 },
    #[error("sales order {id} line {line_no}: {reason}")]
    BadQuantity {
        id: String,
        line_no: i64,
        reason: String,
    },
    #[error("sales order {id} has {open} left to invoice; this invoice would take {asked}")]
    Overinvoiced {
        id: String,
        open: String,
        asked: String,
    },
    #[error("no line to invoice")]
    NoLines,
}

/// One sales order with what is still to be invoiced on it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OpenSalesOrder {
    pub document_id: String,
    pub number: Option<String>,
    pub contact_id: String,
    pub contact_name: String,
    pub txn_date: NaiveDate,
    /// Lines less discounts, before tax.
    pub subtotal: Money,
    /// What linked invoices took, before tax.
    pub invoiced: Money,
    pub open: Money,
    /// The invoices made from it, oldest first.
    pub invoice_ids: Vec<String>,
    pub memo: Option<String>,
    pub source_ref: Option<String>,
}

/// Lines less discounts, before tax: what a sales order is worth and what
/// an invoice made from one takes from it.
pub fn subtotal(doc: &LedgerDocument) -> Result<Money, MoneyError> {
    let mut total = Money::ZERO;
    for line in &doc.lines {
        match line.kind {
            LineKind::Item | LineKind::Shipping | LineKind::Account => {
                total = total.checked_add(line.amount)?
            }
            LineKind::Discount => total = total.checked_sub(line.amount)?,
            LineKind::Description | LineKind::Journal => {}
        }
    }
    Ok(total)
}

/// What every non-voided invoice dated on or before `as_of` took from each
/// sales order: document id to (amount, invoice ids).
fn invoiced_by_order(
    ledger: &Ledger,
    company: &str,
    as_of: NaiveDate,
) -> Result<HashMap<String, (Money, Vec<String>)>, SalesOrderError> {
    let mut out: HashMap<String, (Money, Vec<String>)> = HashMap::new();
    for invoice in books::latest_documents(ledger, company, &[DocKind::Invoice], as_of)? {
        for link in invoice
            .applications
            .iter()
            .filter(|a| a.target_kind == DocKind::SalesOrder)
        {
            let entry = out
                .entry(link.target_document_id.clone())
                .or_insert((Money::ZERO, Vec::new()));
            entry.0 = entry.0.checked_add(link.amount)?;
            if !entry.1.contains(&invoice.document_id) {
                entry.1.push(invoice.document_id.clone());
            }
        }
    }
    Ok(out)
}

/// Every sales order dated on or before `as_of` (every one when `None`),
/// with what is left to invoice; only those with something left when
/// `open_only`. Voided orders are never listed. Oldest first.
pub fn sales_orders(
    ledger: &Ledger,
    company: &str,
    as_of: Option<NaiveDate>,
    open_only: bool,
) -> Result<Vec<OpenSalesOrder>, SalesOrderError> {
    let as_of = as_of.unwrap_or_else(books::far_future);
    let invoiced = invoiced_by_order(ledger, company, as_of)?;
    let names = books::contact_names(ledger, company)?;
    let mut out = Vec::new();
    for doc in books::latest_documents(ledger, company, &[DocKind::SalesOrder], as_of)? {
        let total = subtotal(&doc)?;
        let (taken, invoice_ids) = invoiced
            .get(&doc.document_id)
            .cloned()
            .unwrap_or((Money::ZERO, Vec::new()));
        let open = total.checked_sub(taken)?;
        if open_only && open.minor() <= 0 {
            continue;
        }
        let contact_id = doc
            .contact
            .as_ref()
            .map(|c| c.id.clone())
            .unwrap_or_default();
        out.push(OpenSalesOrder {
            contact_name: names
                .get(&contact_id)
                .cloned()
                .unwrap_or_else(|| contact_id.clone()),
            contact_id,
            document_id: doc.document_id,
            number: doc.number,
            txn_date: doc.txn_date,
            subtotal: total,
            invoiced: taken,
            open: open.max(Money::ZERO),
            invoice_ids,
            memo: doc.memo,
            source_ref: doc.source_ref,
        });
    }
    Ok(out)
}

/// Which line of the order to invoice, and how much of it.
#[derive(Clone, Debug, PartialEq)]
pub struct ConvertLine {
    pub line_no: i64,
    /// Invoice this many rather than the line's whole quantity. The amount
    /// scales with it.
    pub qty: Option<Decimal>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ConvertRequest {
    pub document_id: String,
    pub txn_date: NaiveDate,
    /// `None`: every line, allowed only while nothing is invoiced yet.
    pub lines: Option<Vec<ConvertLine>>,
    pub number: Option<String>,
    pub memo: Option<String>,
}

/// The invoice for `sales_order_id`, linked back to it, not yet prepared,
/// posted or saved: pass it through `books::prepare_document` (which numbers
/// it, dates it due, and taxes it) and save and post it like any invoice.
pub fn invoice_from_sales_order(
    ledger: &Ledger,
    company: &str,
    sales_order_id: &str,
    request: ConvertRequest,
) -> Result<LedgerDocument, SalesOrderError> {
    let order = books::latest_document(ledger, company, sales_order_id)?;
    if order.kind != DocKind::SalesOrder {
        return Err(SalesOrderError::NotASalesOrder(
            sales_order_id.to_string(),
            order.kind.as_str(),
        ));
    }
    if order.is_voided {
        return Err(SalesOrderError::Voided(sales_order_id.to_string()));
    }
    let taken = invoiced_by_order(ledger, company, books::far_future())?
        .remove(sales_order_id)
        .map_or(Money::ZERO, |(amount, _)| amount);
    let open = subtotal(&order)?.checked_sub(taken)?;
    if open.minor() <= 0 {
        return Err(SalesOrderError::NothingOpen(sales_order_id.to_string()));
    }

    let picked: Vec<(DocLine, Option<Decimal>)> = match &request.lines {
        None => {
            if !taken.is_zero() {
                return Err(SalesOrderError::PartlyInvoiced(sales_order_id.to_string()));
            }
            order.lines.iter().map(|l| (l.clone(), None)).collect()
        }
        Some(wanted) => {
            let mut out = Vec::with_capacity(wanted.len());
            for want in wanted {
                let line = order
                    .lines
                    .iter()
                    .find(|l| l.line_no == want.line_no)
                    .ok_or_else(|| SalesOrderError::NoSuchLine {
                        id: sales_order_id.to_string(),
                        line_no: want.line_no,
                    })?;
                out.push((line.clone(), want.qty));
            }
            out
        }
    };

    let mut lines = Vec::with_capacity(picked.len());
    for (index, (mut line, qty)) in picked.into_iter().enumerate() {
        if let Some(qty) = qty {
            let bad = |reason: &str| SalesOrderError::BadQuantity {
                id: sales_order_id.to_string(),
                line_no: line.line_no,
                reason: reason.to_string(),
            };
            let Some(ordered) = line.qty.filter(|q| !q.is_zero()) else {
                return Err(bad("the line has no quantity to take part of"));
            };
            if qty <= Decimal::ZERO || qty > ordered {
                return Err(bad(&format!(
                    "quantity must be above 0 and at most {ordered}"
                )));
            }
            line.amount = round_money(
                Decimal::new(line.amount.minor(), 2) * qty / ordered,
                RoundingPolicy::LineExtension,
            )?;
            line.qty = Some(qty);
        }
        line.line_no = index as i64 + 1;
        lines.push(line);
    }
    if lines.is_empty() {
        return Err(SalesOrderError::NoLines);
    }

    let mut invoice = LedgerDocument {
        document_id: request.document_id,
        kind: DocKind::Invoice,
        number: request.number,
        txn_date: request.txn_date,
        due_date: None,
        contact: order.contact.clone(),
        header_class: order.header_class.clone(),
        lines,
        tax: None,
        deposit_to: None,
        pay_from: None,
        applications: Vec::new(),
        unapplied: Money::ZERO,
        is_voided: false,
        source_ref: None,
        memo: request.memo.or_else(|| order.memo.clone()),
    };
    let asked = subtotal(&invoice)?;
    if asked > open {
        return Err(SalesOrderError::Overinvoiced {
            id: sales_order_id.to_string(),
            open: Money::display(open),
            asked: Money::display(asked),
        });
    }
    invoice.applications.push(Application {
        target_document_id: order.document_id,
        target_kind: DocKind::SalesOrder,
        amount: asked,
    });
    Ok(invoice)
}
