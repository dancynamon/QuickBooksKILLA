//! The posting-rules table as code. `LEDGER-DESIGN.md` §1, §3, §9, §11.
//!
//! One function, [`post`], turns a [`LedgerDocument`] into either
//! [`Posting::NonPosting`] (Estimate, PurchaseOrder: contracts, not
//! transactions) or a balanced [`JournalEntry`]. It is pure: no database, no
//! clock, nothing but the document, the version that produced it, and a
//! [`PostingContext`] carrying the item and customer facts the document
//! itself does not carry. `store.rs` is the only thing that persists what
//! this returns.
//!
//! Every `post_*` function below builds a list of [`Leg`]s (an account, a
//! side, an amount, an optional class, an optional entity) and hands it to
//! [`finalize`], which merges legs that share an (account, class, entity,
//! side) key into one [`JournalLine`] and numbers what remains from 1. Two
//! item lines selling the same class both credit 4100, and a real invoice
//! should not need a line for each one; merging is what keeps an entry
//! short. Two invariants that every function below relies on rather than
//! re-checking: a balance-sheet leg (AR, AP, bank, inventory, tax payable)
//! never carries a class, and an income, COGS or class-consuming expense
//! leg (4xxx, 5xxx, 6xxx, 7xxx) always does, or the document is rejected
//! before any leg is built for it (§3's default chain, [`resolve_class`],
//! enforced by [`requires_class`]).
//!
//! A voided document is not a special case threaded through every
//! `post_*` function. It posts exactly as it would otherwise, and only at
//! the very end [`apply_void`] swaps every line's debit and credit and
//! prefixes the memo `VOID: `, so the store can post the result as the
//! reversal (§6). That is why `VoidNeedsReversal` is not a [`PostError`]
//! variant: voiding never fails posting, it just changes what gets posted.

use rust_decimal::Decimal;
use thiserror::Error;

use ledger_core::{round_money, Money, MoneyError, RoundingPolicy};

use crate::chart;
use crate::types::{
    AccountId, ClassId, ContactRef, DocKind, DocLine, ItemAccounts, JournalEntry, JournalLine,
    LedgerDocument, LineKind, PostingContext, Side, TaxDetail,
};

/// What [`post`] produces. Estimates and purchase orders are the contract,
/// not the transaction (§1), and post as [`Posting::NonPosting`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Posting {
    NonPosting,
    Entry(JournalEntry),
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PostError {
    /// §3's default chain (line, then item default, then header) found
    /// nothing for a line that touches income, COGS or a class-consuming
    /// expense account.
    #[error("line {line_no}: no class on the line, its item, or the document header")]
    MissingClass { line_no: i64 },

    /// A line named an item this context has never heard of.
    #[error("unknown item {item_id}")]
    UnknownItem { item_id: String },

    /// An `Account` or `Journal` line, or an `Item` line whose item carries
    /// no usable account, with nothing to post to.
    #[error("line {line_no}: no account to post this line to")]
    MissingAccount { line_no: i64 },

    /// A `JournalEntry` document whose stated debits and credits do not
    /// agree once every line is posted.
    #[error("journal entry does not balance: debits and credits differ")]
    UnbalancedJournal,

    #[error(transparent)]
    Money(#[from] MoneyError),

    /// §11: built after cutover. Schema and posting effects are designed
    /// there; there is no store to post these into yet.
    #[error("{kind:?} does not post yet, built after cutover (LEDGER-DESIGN.md §11)")]
    Unsupported { kind: DocKind },

    /// A Payment document that states its own total (via `lines`, e.g. a
    /// deposit slip's breakdown of checks received) whose sum does not equal
    /// `applications` plus `unapplied`.
    #[error(
        "payment {document_id}: applications {applications:?} plus unapplied {unapplied:?} \
         does not equal the stated total {stated:?}"
    )]
    PaymentDoesNotAddUp {
        document_id: String,
        applications: Money,
        unapplied: Money,
        stated: Money,
    },
}

/// §3's default chain: explicit class on the line, then the item's default,
/// then the document header's class, first hit wins.
pub fn resolve_class(
    line: &DocLine,
    item: Option<&ItemAccounts>,
    header: Option<&ClassId>,
) -> Option<ClassId> {
    line.class
        .clone()
        .or_else(|| item.and_then(|item| item.default_class.clone()))
        .or_else(|| header.cloned())
}

/// §3: every line touching income, COGS or a class-consuming expense
/// account (4xxx, 5xxx, 6xxx, 7xxx) carries exactly one class. Balance sheet
/// accounts (1xxx, 2xxx, 3xxx) do not.
pub fn requires_class(account: &AccountId) -> bool {
    matches!(
        account.0.as_bytes().first(),
        Some(b'4' | b'5' | b'6' | b'7')
    )
}

/// One side of one entry, before merging. Never constructed with a zero
/// amount by anything below; [`finalize`] drops zero legs regardless, since
/// a zero-amount `JournalLine` would violate the `debit + credit > 0`
/// invariant `journal_lines` enforces (§4).
#[derive(Clone, Debug)]
struct Leg {
    account: AccountId,
    class: Option<ClassId>,
    entity: Option<ContactRef>,
    side: Side,
    amount: Money,
    /// §9 line E, carried through to the [`JournalLine`] this leg becomes.
    /// `false`/`None` for everything but an income leg `revenue_legs` builds
    /// from an Item or Shipping line — see [`Leg::with_tax`].
    is_taxable: bool,
    tax_amount: Option<Money>,
    tax_rate: Option<Decimal>,
}

impl Leg {
    /// Attaches §9 line-E taxability to an income leg: the document line's
    /// own `is_taxable`, and this line's share of the document's tax
    /// (`None` unless the line is taxable and the document carries tax).
    fn with_tax(
        mut self,
        is_taxable: bool,
        tax_amount: Option<Money>,
        tax_rate: Option<Decimal>,
    ) -> Self {
        self.is_taxable = is_taxable;
        self.tax_amount = tax_amount;
        self.tax_rate = tax_rate;
        self
    }
}

fn debit(
    account: AccountId,
    amount: Money,
    class: Option<ClassId>,
    entity: Option<ContactRef>,
) -> Leg {
    Leg {
        account,
        class,
        entity,
        side: Side::Debit,
        amount,
        is_taxable: false,
        tax_amount: None,
        tax_rate: None,
    }
}

fn credit(
    account: AccountId,
    amount: Money,
    class: Option<ClassId>,
    entity: Option<ContactRef>,
) -> Leg {
    Leg {
        account,
        class,
        entity,
        side: Side::Credit,
        amount,
        is_taxable: false,
        tax_amount: None,
        tax_rate: None,
    }
}

/// §9: this line's own share of the document's tax (`amount × rate`,
/// rounded as a tax calculation) paired with the rate that produced it, or
/// `None` when the line is not taxable or the document carries no tax at
/// all. A non-taxable line contributed nothing to the tax total, so it has
/// no share to attribute.
fn line_tax(
    line: &DocLine,
    tax: Option<&TaxDetail>,
) -> Result<Option<(Money, Decimal)>, PostError> {
    if !line.is_taxable {
        return Ok(None);
    }
    let Some(tax) = tax else {
        return Ok(None);
    };
    let amount = Decimal::new(line.amount.minor(), 2);
    let extended = round_money(amount * tax.rate, RoundingPolicy::TaxCalculation)?;
    Ok(Some((extended, tax.rate)))
}

fn acct(number: &str) -> AccountId {
    AccountId(number.to_string())
}

/// [`resolve_class`], then reject if the account needs a class and none
/// resolved. Every `post_*` function below calls this exactly once per line
/// that produces an income, COGS or class-consuming expense leg, and passes
/// `None` for balance sheet legs without calling it at all.
fn class_for(
    line: &DocLine,
    item: Option<&ItemAccounts>,
    header: Option<&ClassId>,
    account: &AccountId,
) -> Result<Option<ClassId>, PostError> {
    let resolved = resolve_class(line, item, header);
    if requires_class(account) && resolved.is_none() {
        return Err(PostError::MissingClass {
            line_no: line.line_no,
        });
    }
    Ok(resolved)
}

/// Looks up a line's item. `Ok(None)` means the line named no item at all
/// (an `Account`, `Discount` or `Shipping` line commonly has none); `Err`
/// means it named one this context does not know.
fn lookup_item<'ctx>(
    ctx: &'ctx PostingContext,
    item_id: &Option<String>,
) -> Result<Option<&'ctx ItemAccounts>, PostError> {
    match item_id {
        Some(id) => ctx
            .items
            .get(id)
            .map(Some)
            .ok_or_else(|| PostError::UnknownItem {
                item_id: id.clone(),
            }),
        None => Ok(None),
    }
}

/// The item lookup an `Item` line needs when it posts straight to the item's
/// own accounts (Bill, VendorCredit, Purchase): the item must exist, since
/// there is no other account to fall back to.
fn required_item<'ctx>(
    ctx: &'ctx PostingContext,
    line: &DocLine,
) -> Result<&'ctx ItemAccounts, PostError> {
    lookup_item(ctx, &line.item_id)?.ok_or(PostError::MissingAccount {
        line_no: line.line_no,
    })
}

/// qty x unit cost, rounded as a line extension (§1's inventory legs, §11's
/// `RoundingPolicy::LineExtension`).
fn extend_unit_cost(qty: Decimal, unit_cost: Money) -> Result<Money, PostError> {
    let rate = Decimal::new(unit_cost.minor(), 2);
    round_money(qty * rate, RoundingPolicy::LineExtension).map_err(PostError::from)
}

/// The COGS pair an Invoice or SalesReceipt Item line produces when its item
/// is inventory-tracked and a unit cost is known: Dr 5000, Cr the item's
/// asset account, at qty x unit cost, carrying the same class as the income
/// leg it accompanies. Does nothing for a service line, an untracked item,
/// or a line with no quantity or cost to extend.
fn post_inventory_cogs(
    legs: &mut Vec<Leg>,
    line: &DocLine,
    item: Option<&ItemAccounts>,
    class: Option<ClassId>,
) -> Result<(), PostError> {
    let Some(item) = item else { return Ok(()) };
    let Some(asset) = &item.asset else {
        return Ok(());
    };
    let Some(qty) = line.qty else { return Ok(()) };
    let Some(unit_cost) = line.unit_cost.or(item.unit_cost) else {
        return Ok(());
    };
    let extended = extend_unit_cost(qty, unit_cost)?;
    if extended.is_zero() {
        return Ok(());
    }
    legs.push(debit(acct(chart::COGS), extended, class.clone(), None));
    legs.push(credit(asset.clone(), extended, None, None));
    Ok(())
}

/// The mirror of [`post_inventory_cogs`] for a CreditMemo: Dr the item's
/// asset account, Cr 5000, at the original unit cost.
fn post_inventory_return(
    legs: &mut Vec<Leg>,
    line: &DocLine,
    item: Option<&ItemAccounts>,
    class: Option<ClassId>,
) -> Result<(), PostError> {
    let Some(item) = item else { return Ok(()) };
    let Some(asset) = &item.asset else {
        return Ok(());
    };
    let Some(qty) = line.qty else { return Ok(()) };
    let Some(unit_cost) = line.unit_cost.or(item.unit_cost) else {
        return Ok(());
    };
    let extended = extend_unit_cost(qty, unit_cost)?;
    if extended.is_zero() {
        return Ok(());
    }
    legs.push(debit(asset.clone(), extended, None, None));
    legs.push(credit(acct(chart::COGS), extended, class, None));
    Ok(())
}

/// The legs Invoice and SalesReceipt share: Cr 4100 per Item line (plus its
/// COGS pair), Cr 4300 per Shipping line, Dr 4900 per Discount line, Cr 2200
/// for the document's tax. Returns the legs and the total they sum to
/// (before the caller's own final debit), which is subtotal minus discounts
/// plus tax, exactly what the proptest at the bottom of this crate's test
/// suite checks.
fn revenue_legs(
    doc: &LedgerDocument,
    ctx: &PostingContext,
) -> Result<(Vec<Leg>, Money), PostError> {
    let mut legs = Vec::new();
    let mut total = Money::ZERO;
    let header = doc.header_class.as_ref();

    for line in &doc.lines {
        match line.kind {
            LineKind::Item => {
                let item = lookup_item(ctx, &line.item_id)?;
                let account = acct(chart::SALES_INCOME);
                let class = class_for(line, item, header, &account)?;
                let (tax_amount, tax_rate) = match line_tax(line, doc.tax.as_ref())? {
                    Some((amount, rate)) => (Some(amount), Some(rate)),
                    None => (None, None),
                };
                legs.push(credit(account, line.amount, class.clone(), None).with_tax(
                    line.is_taxable,
                    tax_amount,
                    tax_rate,
                ));
                total = total.checked_add(line.amount)?;
                post_inventory_cogs(&mut legs, line, item, class)?;
            }
            LineKind::Shipping => {
                let item = lookup_item(ctx, &line.item_id)?;
                let account = acct(chart::SHIPPING_INCOME);
                let class = class_for(line, item, header, &account)?;
                let (tax_amount, tax_rate) = match line_tax(line, doc.tax.as_ref())? {
                    Some((amount, rate)) => (Some(amount), Some(rate)),
                    None => (None, None),
                };
                legs.push(credit(account, line.amount, class, None).with_tax(
                    line.is_taxable,
                    tax_amount,
                    tax_rate,
                ));
                total = total.checked_add(line.amount)?;
            }
            LineKind::Discount => {
                let item = lookup_item(ctx, &line.item_id)?;
                let account = acct(chart::DISCOUNTS_GIVEN);
                let class = class_for(line, item, header, &account)?;
                legs.push(debit(account, line.amount, class, None));
                total = total.checked_sub(line.amount)?;
            }
            LineKind::Description => {}
            LineKind::Account | LineKind::Journal => {}
        }
    }

    if let Some(tax) = &doc.tax {
        legs.push(credit(
            acct(chart::SALES_TAX_PAYABLE),
            tax.total_tax,
            None,
            None,
        ));
        total = total.checked_add(tax.total_tax)?;
    }

    Ok((legs, total))
}

/// §1: Invoice. Dr 1200 AR, total including tax, entity the customer.
fn post_invoice(doc: &LedgerDocument, ctx: &PostingContext) -> Result<Vec<Leg>, PostError> {
    let (mut legs, total) = revenue_legs(doc, ctx)?;
    legs.push(debit(
        acct(chart::ACCOUNTS_RECEIVABLE),
        total,
        None,
        doc.contact.clone(),
    ));
    Ok(legs)
}

/// §1: SalesReceipt. As Invoice, but paid at the point of sale: Dr
/// `deposit_to` else `config.default_deposit_account`, no AR leg and so no
/// entity on the money-in leg.
fn post_sales_receipt(doc: &LedgerDocument, ctx: &PostingContext) -> Result<Vec<Leg>, PostError> {
    let (mut legs, total) = revenue_legs(doc, ctx)?;
    let deposit = doc
        .deposit_to
        .clone()
        .unwrap_or_else(|| ctx.config.default_deposit_account.clone());
    legs.push(debit(deposit, total, None, None));
    Ok(legs)
}

/// The legs CreditMemo and RefundReceipt share: Dr 4950 per Item or Shipping
/// line, plus the inventory return pair on an Item line with a known cost;
/// Dr 2200 tax reversed; Cr 4900 per Discount line (mirroring the Dr 4900 an
/// invoice posted). Returns the legs and the total they sum to.
fn returns_legs(
    doc: &LedgerDocument,
    ctx: &PostingContext,
) -> Result<(Vec<Leg>, Money), PostError> {
    let mut legs = Vec::new();
    let mut total = Money::ZERO;
    let header = doc.header_class.as_ref();

    for line in &doc.lines {
        match line.kind {
            LineKind::Item | LineKind::Shipping => {
                let item = lookup_item(ctx, &line.item_id)?;
                let account = acct(chart::RETURNS_ALLOWANCES);
                let class = class_for(line, item, header, &account)?;
                legs.push(debit(account, line.amount, class.clone(), None));
                total = total.checked_add(line.amount)?;
                if line.kind == LineKind::Item {
                    post_inventory_return(&mut legs, line, item, class)?;
                }
            }
            LineKind::Discount => {
                let item = lookup_item(ctx, &line.item_id)?;
                let account = acct(chart::DISCOUNTS_GIVEN);
                let class = class_for(line, item, header, &account)?;
                legs.push(credit(account, line.amount, class, None));
                total = total.checked_sub(line.amount)?;
            }
            LineKind::Description => {}
            LineKind::Account | LineKind::Journal => {}
        }
    }

    if let Some(tax) = &doc.tax {
        legs.push(debit(
            acct(chart::SALES_TAX_PAYABLE),
            tax.total_tax,
            None,
            None,
        ));
        total = total.checked_add(tax.total_tax)?;
    }

    Ok((legs, total))
}

/// §1: CreditMemo. Cr 1200 AR, entity the customer.
fn post_credit_memo(doc: &LedgerDocument, ctx: &PostingContext) -> Result<Vec<Leg>, PostError> {
    let (mut legs, total) = returns_legs(doc, ctx)?;
    legs.push(credit(
        acct(chart::ACCOUNTS_RECEIVABLE),
        total,
        None,
        doc.contact.clone(),
    ));
    Ok(legs)
}

/// §1: RefundReceipt. As CreditMemo, but money out with no AR leg: Cr
/// `pay_from` else the default bank account.
fn post_refund_receipt(doc: &LedgerDocument, ctx: &PostingContext) -> Result<Vec<Leg>, PostError> {
    let (mut legs, total) = returns_legs(doc, ctx)?;
    let pay_from = doc
        .pay_from
        .clone()
        .unwrap_or_else(|| ctx.config.default_bank_account.clone());
    legs.push(credit(pay_from, total, None, None));
    Ok(legs)
}

/// §1: Payment. Dr `deposit_to` else 1150 undeposited funds; Cr 1200 for the
/// sum of `applications`, Cr 2300 for `unapplied`, both entitled to the
/// customer. When the document also states lines (a deposit slip's
/// breakdown of what was actually received), their sum must equal
/// `applications` plus `unapplied` or the document is rejected rather than
/// posted against a total nobody actually handed over.
fn post_payment(doc: &LedgerDocument, _ctx: &PostingContext) -> Result<Vec<Leg>, PostError> {
    let applications_total = Money::checked_sum(
        doc.applications
            .iter()
            .map(|application| application.amount),
    )?;
    let total = applications_total.checked_add(doc.unapplied)?;

    if !doc.lines.is_empty() {
        let stated = doc.subtotal()?;
        if stated != total {
            return Err(PostError::PaymentDoesNotAddUp {
                document_id: doc.document_id.clone(),
                applications: applications_total,
                unapplied: doc.unapplied,
                stated,
            });
        }
    }

    let mut legs = Vec::new();
    let entity = doc.contact.clone();
    if !applications_total.is_zero() {
        legs.push(credit(
            acct(chart::ACCOUNTS_RECEIVABLE),
            applications_total,
            None,
            entity.clone(),
        ));
    }
    if !doc.unapplied.is_zero() {
        legs.push(credit(
            acct(chart::CUSTOMER_DEPOSITS),
            doc.unapplied,
            None,
            entity,
        ));
    }
    let deposit = doc
        .deposit_to
        .clone()
        .unwrap_or_else(|| acct(chart::UNDEPOSITED_FUNDS));
    legs.push(debit(deposit, total, None, None));
    Ok(legs)
}

/// Where an `Item` line posts on a purchase-side document (Bill,
/// VendorCredit, Purchase): the item's asset account when it is
/// inventory-tracked, else its expense or COGS account (§1's Bill row).
fn purchase_side_account(item: &ItemAccounts) -> AccountId {
    item.asset.clone().unwrap_or_else(|| item.expense.clone())
}

/// §1: Bill. Dr per line (an Item line to its item's asset or expense
/// account, an Account line to the account it names), Cr 2000 AP for the
/// total, entity the vendor. No 2200 leg: vendor-charged tax is part of the
/// cost (§1).
fn post_bill(doc: &LedgerDocument, ctx: &PostingContext) -> Result<Vec<Leg>, PostError> {
    let mut legs = Vec::new();
    let mut total = Money::ZERO;
    let header = doc.header_class.as_ref();

    for line in &doc.lines {
        match line.kind {
            LineKind::Item => {
                let item = required_item(ctx, line)?;
                let account = purchase_side_account(item);
                let class = class_for(line, Some(item), header, &account)?;
                legs.push(debit(account, line.amount, class, None));
                total = total.checked_add(line.amount)?;
            }
            LineKind::Account => {
                let account = line.account.clone().ok_or(PostError::MissingAccount {
                    line_no: line.line_no,
                })?;
                let class = class_for(line, None, header, &account)?;
                legs.push(debit(account, line.amount, class, None));
                total = total.checked_add(line.amount)?;
            }
            LineKind::Description | LineKind::Discount | LineKind::Shipping | LineKind::Journal => {
            }
        }
    }

    legs.push(credit(
        acct(chart::ACCOUNTS_PAYABLE),
        total,
        None,
        doc.contact.clone(),
    ));
    Ok(legs)
}

/// §1: BillPayment. Dr 2000 for the applications paid, Cr `pay_from` else
/// the default bank account. An unapplied remainder is a vendor prepayment
/// (a debit balance sitting in AP until a future bill relieves it), so it
/// debits 2000 exactly as an application would rather than parking anywhere
/// else.
fn post_bill_payment(doc: &LedgerDocument, ctx: &PostingContext) -> Result<Vec<Leg>, PostError> {
    let applications_total = Money::checked_sum(
        doc.applications
            .iter()
            .map(|application| application.amount),
    )?;
    let ap_total = applications_total.checked_add(doc.unapplied)?;

    let mut legs = Vec::new();
    legs.push(debit(
        acct(chart::ACCOUNTS_PAYABLE),
        ap_total,
        None,
        doc.contact.clone(),
    ));
    let pay_from = doc
        .pay_from
        .clone()
        .unwrap_or_else(|| ctx.config.default_bank_account.clone());
    legs.push(credit(pay_from, ap_total, None, None));
    Ok(legs)
}

/// §1: VendorCredit. Dr 2000, entity the vendor; Cr the item's asset account
/// on an Item line whose item is tracked, else the item's expense account
/// (W3: a vendor credit reduces cost, it never posts to other income).
fn post_vendor_credit(doc: &LedgerDocument, ctx: &PostingContext) -> Result<Vec<Leg>, PostError> {
    let mut legs = Vec::new();
    let mut total = Money::ZERO;
    let header = doc.header_class.as_ref();

    for line in &doc.lines {
        match line.kind {
            LineKind::Item => {
                let item = required_item(ctx, line)?;
                let account = purchase_side_account(item);
                let class = class_for(line, Some(item), header, &account)?;
                legs.push(credit(account, line.amount, class, None));
                total = total.checked_add(line.amount)?;
            }
            LineKind::Account => {
                let account = line.account.clone().ok_or(PostError::MissingAccount {
                    line_no: line.line_no,
                })?;
                let class = class_for(line, None, header, &account)?;
                legs.push(credit(account, line.amount, class, None));
                total = total.checked_add(line.amount)?;
            }
            LineKind::Description | LineKind::Discount | LineKind::Shipping | LineKind::Journal => {
            }
        }
    }

    legs.push(debit(
        acct(chart::ACCOUNTS_PAYABLE),
        total,
        None,
        doc.contact.clone(),
    ));
    Ok(legs)
}

/// §1: Purchase (non-PO expense, check, card charge). Dr per line, Cr
/// `pay_from` else the default bank account.
fn post_purchase(doc: &LedgerDocument, ctx: &PostingContext) -> Result<Vec<Leg>, PostError> {
    let mut legs = Vec::new();
    let mut total = Money::ZERO;
    let header = doc.header_class.as_ref();

    for line in &doc.lines {
        match line.kind {
            LineKind::Item => {
                let item = required_item(ctx, line)?;
                let account = purchase_side_account(item);
                let class = class_for(line, Some(item), header, &account)?;
                legs.push(debit(account, line.amount, class, None));
                total = total.checked_add(line.amount)?;
            }
            LineKind::Account => {
                let account = line.account.clone().ok_or(PostError::MissingAccount {
                    line_no: line.line_no,
                })?;
                let class = class_for(line, None, header, &account)?;
                legs.push(debit(account, line.amount, class, None));
                total = total.checked_add(line.amount)?;
            }
            LineKind::Description | LineKind::Discount | LineKind::Shipping | LineKind::Journal => {
            }
        }
    }

    let pay_from = doc
        .pay_from
        .clone()
        .unwrap_or_else(|| ctx.config.default_bank_account.clone());
    legs.push(credit(pay_from, total, None, None));
    Ok(legs)
}

/// §1: Deposit. Dr `deposit_to` else the default bank account for the net;
/// Cr each Account line to the account it names. A negative line's amount is
/// a fee netted inside the deposit (the merchant fee row in §1) rather than
/// money grouped into the deposit, so it debits its account instead of
/// crediting it, and the bank leg nets to the true amount that hit the
/// account.
fn post_deposit(doc: &LedgerDocument, ctx: &PostingContext) -> Result<Vec<Leg>, PostError> {
    let mut legs = Vec::new();
    let mut net = Money::ZERO;
    let header = doc.header_class.as_ref();

    for line in &doc.lines {
        if line.kind != LineKind::Account {
            continue;
        }
        let account = line.account.clone().ok_or(PostError::MissingAccount {
            line_no: line.line_no,
        })?;
        let class = class_for(line, None, header, &account)?;
        if line.amount.is_negative() {
            let fee = line.amount.checked_neg()?;
            legs.push(debit(account, fee, class, None));
            net = net.checked_sub(fee)?;
        } else {
            legs.push(credit(account, line.amount, class, None));
            net = net.checked_add(line.amount)?;
        }
    }

    let deposit_to = doc
        .deposit_to
        .clone()
        .unwrap_or_else(|| ctx.config.default_bank_account.clone());
    legs.push(debit(deposit_to, net, None, None));
    Ok(legs)
}

/// §1: JournalEntry. Every `Journal` line posts to its own named account on
/// its own stated side, with its own entity, carried straight through. No
/// item and no default account applies; a line naming no account, or naming
/// no side, has nothing to post.
fn post_journal_entry(doc: &LedgerDocument) -> Result<Vec<Leg>, PostError> {
    let mut legs = Vec::new();
    let header = doc.header_class.as_ref();

    for line in &doc.lines {
        if line.kind != LineKind::Journal {
            continue;
        }
        let account = line.account.clone().ok_or(PostError::MissingAccount {
            line_no: line.line_no,
        })?;
        let side = line.posting.ok_or(PostError::MissingAccount {
            line_no: line.line_no,
        })?;
        let class = class_for(line, None, header, &account)?;
        legs.push(Leg {
            account,
            class,
            entity: line.entity.clone(),
            side,
            amount: line.amount,
            is_taxable: false,
            tax_amount: None,
            tax_rate: None,
        });
    }

    Ok(legs)
}

/// §1, §10: a confirmed bank statement line (BankLine documents in this
/// crate represent the movement Dan has already confirmed, not the
/// unmatched proposal itself, which posts nothing). Its single `Account`
/// line names the other side of the movement; the bank or card account is
/// `deposit_to` when set, else `pay_from`, else the default bank account,
/// since one statement's lines share one account regardless of direction. A
/// positive amount is money in (Dr the bank, Cr the named account); a
/// negative amount is money out (Dr the named account, Cr the bank).
fn post_bank_line(doc: &LedgerDocument, ctx: &PostingContext) -> Result<Vec<Leg>, PostError> {
    let header = doc.header_class.as_ref();
    let line = doc
        .lines
        .iter()
        .find(|line| line.kind == LineKind::Account)
        .ok_or(PostError::MissingAccount { line_no: 0 })?;
    let account = line.account.clone().ok_or(PostError::MissingAccount {
        line_no: line.line_no,
    })?;
    let class = class_for(line, None, header, &account)?;
    let bank = doc
        .deposit_to
        .clone()
        .or_else(|| doc.pay_from.clone())
        .unwrap_or_else(|| ctx.config.default_bank_account.clone());

    let legs = if line.amount.is_negative() {
        let amount = line.amount.checked_neg()?;
        vec![
            debit(account, amount, class, None),
            credit(bank, amount, None, None),
        ]
    } else {
        vec![
            debit(bank, line.amount, None, None),
            credit(account, line.amount, class, None),
        ]
    };
    Ok(legs)
}

/// §1: an Authorize.net settlement, combined with its fee in the same
/// entry. Dr the bank (net payout) plus Dr 6700 (the batch's fee, when the
/// document states one), Cr 1160 clearing (gross of the batch): the
/// customer payment already debited 1160 when it settled, and this entry is
/// what moves the batch out to the bank. The document states the batch as
/// two `Account` lines, one naming 1160 (the gross, required) and one
/// naming 6700 (the fee, optional; a batch with no fee simply has none).
///
/// Extended for `crate::authnet` (D15): the fee line's own `class` becomes
/// the 6700 leg's class, rather than the `None` an earlier draft of this
/// function used. §1's table calls the Authorize.net fee row's class "none,
/// it is overhead", but §3's rule and the class check `store.rs` runs at
/// post time apply uniformly to every 6xxx leg with no overhead exception —
/// so a fee line with no class would post here and then be refused at
/// `Ledger::post_entry`. Every caller that can produce a nonzero fee line
/// (`crate::authnet::settlement_documents`, `crate::bank`'s fee-aware
/// settlement match) now supplies one.
fn post_settlement(doc: &LedgerDocument, ctx: &PostingContext) -> Result<Vec<Leg>, PostError> {
    let is_account_named = |line: &&DocLine, number: &str| {
        line.kind == LineKind::Account
            && line
                .account
                .as_ref()
                .is_some_and(|account| account.0 == number)
    };

    let gross_line = doc
        .lines
        .iter()
        .find(|line| is_account_named(line, chart::AUTHNET_CLEARING))
        .ok_or(PostError::MissingAccount { line_no: 0 })?;
    let gross = gross_line.amount;

    let fee_line = doc
        .lines
        .iter()
        .find(|line| is_account_named(line, chart::MERCHANT_FEES));
    let fee = fee_line.map_or(Money::ZERO, |line| line.amount);

    let net = gross.checked_sub(fee)?;
    let bank = doc
        .deposit_to
        .clone()
        .unwrap_or_else(|| ctx.config.default_bank_account.clone());

    let mut legs = vec![
        debit(bank, net, None, None),
        credit(acct(chart::AUTHNET_CLEARING), gross, None, None),
    ];
    if !fee.is_zero() {
        let class = fee_line.and_then(|line| line.class.clone());
        legs.push(debit(acct(chart::MERCHANT_FEES), fee, class, None));
    }
    Ok(legs)
}

/// Merges legs sharing an (account, class, entity, side, is_taxable) key
/// into one [`JournalLine`] and numbers what remains from 1. `is_taxable` is
/// in the key alongside the rest (§9 line E): two Item lines of the same
/// class still merge when both are taxable or both are not, but a taxable
/// and a non-taxable line on the same account and class stay two lines, so
/// the taxable one's flag and its tax share are never averaged away into an
/// untaxed sibling's. Linear rather than hashed: a posted entry has at most
/// a handful of distinct legs, and [`ContactRef`] carries no
/// [`std::hash::Hash`] impl by design (it is not used as a map key anywhere
/// else in this crate either).
fn merge_legs(legs: Vec<Leg>) -> Result<Vec<JournalLine>, PostError> {
    let mut merged: Vec<Leg> = Vec::new();
    for leg in legs {
        if leg.amount.is_zero() {
            continue;
        }
        match merged.iter_mut().find(|existing| {
            existing.account == leg.account
                && existing.class == leg.class
                && existing.entity == leg.entity
                && existing.side == leg.side
                && existing.is_taxable == leg.is_taxable
        }) {
            Some(existing) => {
                existing.amount = existing.amount.checked_add(leg.amount)?;
                existing.tax_amount = match (existing.tax_amount, leg.tax_amount) {
                    (Some(a), Some(b)) => Some(a.checked_add(b)?),
                    (Some(a), None) => Some(a),
                    (None, tax_amount) => tax_amount,
                };
                existing.tax_rate = existing.tax_rate.or(leg.tax_rate);
            }
            None => merged.push(leg),
        }
    }

    let mut lines = Vec::with_capacity(merged.len());
    let mut line_no = 1i64;
    for leg in merged {
        if leg.amount.is_zero() {
            continue;
        }
        let mut line = match leg.side {
            Side::Debit => JournalLine::debit(line_no, leg.account, leg.amount),
            Side::Credit => JournalLine::credit(line_no, leg.account, leg.amount),
        };
        line.class = leg.class;
        line.entity = leg.entity;
        line = line.with_tax(leg.is_taxable, leg.tax_amount, leg.tax_rate);
        lines.push(line);
        line_no += 1;
    }
    Ok(lines)
}

fn finalize(
    doc: &LedgerDocument,
    version: i64,
    legs: Vec<Leg>,
    is_flagged: bool,
) -> Result<JournalEntry, PostError> {
    Ok(JournalEntry {
        entry_date: doc.txn_date,
        memo: doc.memo.clone(),
        source_type: doc.kind,
        source_id: doc.document_id.clone(),
        source_version: version,
        reversal_of: None,
        is_flagged,
        lines: merge_legs(legs)?,
    })
}

/// §6: a voided document posts a reversal rather than vanishing. Rather than
/// thread `is_voided` through every `post_*` function, the normal entry is
/// built first and this swaps every line's debit and credit and prefixes
/// the memo, so the store can post the result directly.
fn apply_void(mut entry: JournalEntry, is_voided: bool) -> JournalEntry {
    if !is_voided {
        return entry;
    }
    for line in entry.lines.iter_mut() {
        std::mem::swap(&mut line.debit, &mut line.credit);
    }
    let original = entry.memo.unwrap_or_default();
    entry.memo = Some(format!("VOID: {original}"));
    entry
}

/// The posting-rules table (§1), as one function. Pure: everything it needs
/// beyond the document is in `ctx`. Estimate and PurchaseOrder never post
/// ([`DocKind::posts`]); RawMaterialReceipt, Build and InventoryAdjustment
/// are designed in §11 but not yet built, and return
/// [`PostError::Unsupported`] rather than posting something wrong.
pub fn post(
    doc: &LedgerDocument,
    version: i64,
    ctx: &PostingContext,
) -> Result<Posting, PostError> {
    if !doc.kind.posts() {
        return Ok(Posting::NonPosting);
    }

    if matches!(
        doc.kind,
        DocKind::RawMaterialReceipt | DocKind::Build | DocKind::InventoryAdjustment
    ) {
        return Err(PostError::Unsupported { kind: doc.kind });
    }

    let is_journal = doc.kind == DocKind::JournalEntry;
    let legs = if is_journal {
        post_journal_entry(doc)?
    } else {
        match doc.kind {
            DocKind::Invoice => post_invoice(doc, ctx)?,
            DocKind::SalesReceipt => post_sales_receipt(doc, ctx)?,
            DocKind::CreditMemo => post_credit_memo(doc, ctx)?,
            DocKind::RefundReceipt => post_refund_receipt(doc, ctx)?,
            DocKind::Payment => post_payment(doc, ctx)?,
            DocKind::Bill => post_bill(doc, ctx)?,
            DocKind::BillPayment => post_bill_payment(doc, ctx)?,
            DocKind::VendorCredit => post_vendor_credit(doc, ctx)?,
            DocKind::Purchase => post_purchase(doc, ctx)?,
            DocKind::Deposit => post_deposit(doc, ctx)?,
            DocKind::BankLine => post_bank_line(doc, ctx)?,
            DocKind::Settlement => post_settlement(doc, ctx)?,
            DocKind::Estimate | DocKind::PurchaseOrder => {
                unreachable!("DocKind::posts() filtered these out above")
            }
            DocKind::JournalEntry
            | DocKind::RawMaterialReceipt
            | DocKind::Build
            | DocKind::InventoryAdjustment => {
                unreachable!("handled above")
            }
        }
    };

    let entry = finalize(doc, version, legs, is_journal)?;

    if is_journal && !entry.is_balanced() {
        return Err(PostError::UnbalancedJournal);
    }
    debug_assert!(
        entry.is_balanced(),
        "post() produced an unbalanced entry for {:?} {}",
        doc.kind,
        doc.document_id
    );

    Ok(Posting::Entry(apply_void(entry, doc.is_voided)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ContactKind, TaxDetail};
    use chrono::NaiveDate;
    use rust_decimal_macros::dec;

    fn line(no: i64, kind: LineKind, amount: i64) -> DocLine {
        DocLine {
            line_no: no,
            kind,
            amount: Money::from_minor(amount),
            class: None,
            item_id: None,
            account: None,
            is_taxable: false,
            qty: None,
            unit_cost: None,
            description: None,
            posting: None,
            entity: None,
        }
    }

    fn class(name: &str) -> ClassId {
        ClassId(name.to_string())
    }

    #[test]
    fn resolve_class_prefers_line_then_item_then_header() {
        let mut a_line = line(1, LineKind::Item, 100);
        let item = ItemAccounts {
            income: acct(chart::SALES_INCOME),
            expense: acct(chart::COGS),
            asset: None,
            default_class: Some(class("item")),
            unit_cost: None,
        };
        let header = class("header");

        // Nothing set anywhere: none resolves.
        assert_eq!(resolve_class(&a_line, None, None), None);
        // Header only.
        assert_eq!(
            resolve_class(&a_line, None, Some(&header)),
            Some(class("header"))
        );
        // Item beats header.
        assert_eq!(
            resolve_class(&a_line, Some(&item), Some(&header)),
            Some(class("item"))
        );
        // Line beats item and header.
        a_line.class = Some(class("line"));
        assert_eq!(
            resolve_class(&a_line, Some(&item), Some(&header)),
            Some(class("line"))
        );
    }

    #[test]
    fn requires_class_matches_the_four_bands() {
        assert!(requires_class(&acct(chart::SALES_INCOME))); // 4xxx
        assert!(requires_class(&acct(chart::COGS))); // 5xxx
        assert!(requires_class(&acct(chart::SHOP_SUPPLIES))); // 6xxx
        assert!(requires_class(&acct(chart::INTEREST_EXPENSE))); // 7xxx
        assert!(!requires_class(&acct(chart::ACCOUNTS_RECEIVABLE))); // 1xxx
        assert!(!requires_class(&acct(chart::ACCOUNTS_PAYABLE))); // 2xxx
        assert!(!requires_class(&acct(chart::OWNER_CAPITAL))); // 3xxx
    }

    #[test]
    fn merge_legs_combines_same_account_class_entity_side() {
        let customer = ContactRef {
            kind: ContactKind::Customer,
            id: "cust-1".into(),
        };
        let legs = vec![
            credit(
                acct(chart::SALES_INCOME),
                Money::from_minor(1000),
                Some(class("foam")),
                None,
            ),
            credit(
                acct(chart::SALES_INCOME),
                Money::from_minor(500),
                Some(class("foam")),
                None,
            ),
            debit(
                acct(chart::ACCOUNTS_RECEIVABLE),
                Money::from_minor(1500),
                None,
                Some(customer),
            ),
        ];
        let merged = merge_legs(legs).expect("merge succeeds");
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].line_no, 1);
        assert_eq!(merged[0].credit, Money::from_minor(1500));
        assert_eq!(merged[1].line_no, 2);
        assert_eq!(merged[1].debit, Money::from_minor(1500));
    }

    #[test]
    fn merge_legs_drops_zero_amount_legs() {
        let legs = vec![
            debit(acct(chart::COGS), Money::ZERO, Some(class("foam")), None),
            credit(
                acct(chart::SALES_INCOME),
                Money::from_minor(100),
                Some(class("foam")),
                None,
            ),
        ];
        let merged = merge_legs(legs).expect("merge succeeds");
        assert_eq!(merged.len(), 1);
    }

    fn minimal_doc(kind: DocKind) -> LedgerDocument {
        LedgerDocument {
            document_id: "doc-1".into(),
            kind,
            number: None,
            txn_date: NaiveDate::from_ymd_opt(2026, 9, 12).expect("valid date"),
            due_date: None,
            contact: None,
            header_class: None,
            lines: Vec::new(),
            tax: None,
            deposit_to: None,
            pay_from: None,
            applications: Vec::new(),
            unapplied: Money::ZERO,
            is_voided: false,
            source_ref: None,
            memo: None,
        }
    }

    #[test]
    fn estimate_and_purchase_order_never_post() {
        let ctx = PostingContext::default();
        for kind in [DocKind::Estimate, DocKind::PurchaseOrder] {
            let doc = minimal_doc(kind);
            assert_eq!(post(&doc, 1, &ctx), Ok(Posting::NonPosting));
        }
    }

    #[test]
    fn manufacturing_rows_are_unsupported_for_now() {
        let ctx = PostingContext::default();
        for kind in [
            DocKind::RawMaterialReceipt,
            DocKind::Build,
            DocKind::InventoryAdjustment,
        ] {
            let doc = minimal_doc(kind);
            assert_eq!(post(&doc, 1, &ctx), Err(PostError::Unsupported { kind }));
        }
    }

    #[test]
    fn journal_entry_that_does_not_balance_errors() {
        let ctx = PostingContext::default();
        let mut doc = minimal_doc(DocKind::JournalEntry);
        let mut debit_line = line(1, LineKind::Journal, 100);
        debit_line.account = Some(acct(chart::CHECKING));
        debit_line.posting = Some(Side::Debit);
        let mut credit_line = line(2, LineKind::Journal, 50);
        credit_line.account = Some(acct(chart::OWNER_CAPITAL));
        credit_line.posting = Some(Side::Credit);
        doc.lines = vec![debit_line, credit_line];

        assert_eq!(post(&doc, 1, &ctx), Err(PostError::UnbalancedJournal));
    }

    #[test]
    fn void_swaps_sides_and_prefixes_memo() {
        let ctx = PostingContext::default();
        let mut doc = minimal_doc(DocKind::JournalEntry);
        doc.memo = Some("adjusting entry".into());
        let mut debit_line = line(1, LineKind::Journal, 100);
        debit_line.account = Some(acct(chart::CHECKING));
        debit_line.posting = Some(Side::Debit);
        let mut credit_line = line(2, LineKind::Journal, 100);
        credit_line.account = Some(acct(chart::OWNER_CAPITAL));
        credit_line.posting = Some(Side::Credit);
        doc.lines = vec![debit_line, credit_line];
        doc.is_voided = true;

        let Posting::Entry(entry) = post(&doc, 1, &ctx).expect("posts") else {
            panic!("expected an entry");
        };
        assert_eq!(entry.memo, Some("VOID: adjusting entry".to_string()));
        let checking = entry
            .lines
            .iter()
            .find(|entry_line| entry_line.account == acct(chart::CHECKING))
            .expect("checking line present");
        // The original document debited checking; the void credits it.
        assert_eq!(checking.debit, Money::ZERO);
        assert_eq!(checking.credit, Money::from_minor(100));
        assert!(entry.is_balanced());
    }

    #[test]
    fn a_balance_sheet_leg_never_carries_a_class() {
        let ctx = PostingContext::default();
        let mut doc = minimal_doc(DocKind::Invoice);
        doc.header_class = Some(class("foam"));
        let mut item_line = line(1, LineKind::Item, 10000);
        item_line.class = Some(class("foam"));
        doc.lines = vec![item_line];

        let Posting::Entry(entry) = post(&doc, 1, &ctx).expect("posts") else {
            panic!("expected an entry");
        };
        let ar_line = entry
            .lines
            .iter()
            .find(|entry_line| entry_line.account == acct(chart::ACCOUNTS_RECEIVABLE))
            .expect("AR line present");
        assert_eq!(ar_line.class, None);
        let income_line = entry
            .lines
            .iter()
            .find(|entry_line| entry_line.account == acct(chart::SALES_INCOME))
            .expect("income line present");
        assert_eq!(income_line.class, Some(class("foam")));
    }

    #[test]
    fn missing_class_on_an_income_line_is_rejected() {
        let ctx = PostingContext::default();
        let mut doc = minimal_doc(DocKind::Invoice);
        doc.lines = vec![line(1, LineKind::Item, 10000)];
        assert_eq!(
            post(&doc, 1, &ctx),
            Err(PostError::MissingClass { line_no: 1 })
        );
    }

    #[test]
    fn discount_reduces_the_ar_debit() {
        let ctx = PostingContext::default();
        let mut doc = minimal_doc(DocKind::Invoice);
        let mut item_line = line(1, LineKind::Item, 10000);
        item_line.class = Some(class("foam"));
        let mut discount_line = line(2, LineKind::Discount, 1000);
        discount_line.class = Some(class("foam"));
        doc.lines = vec![item_line, discount_line];

        let Posting::Entry(entry) = post(&doc, 1, &ctx).expect("posts") else {
            panic!("expected an entry");
        };
        let ar_line = entry
            .lines
            .iter()
            .find(|entry_line| entry_line.account == acct(chart::ACCOUNTS_RECEIVABLE))
            .expect("AR line present");
        assert_eq!(ar_line.debit, Money::from_minor(9000));
        let discount = entry
            .lines
            .iter()
            .find(|entry_line| entry_line.account == acct(chart::DISCOUNTS_GIVEN))
            .expect("discount line present");
        assert_eq!(discount.debit, Money::from_minor(1000));
    }

    #[test]
    fn tax_leg_present_only_when_tax_is_some() {
        let ctx = PostingContext::default();
        let mut without_tax = minimal_doc(DocKind::Invoice);
        let mut without_line = line(1, LineKind::Item, 10000);
        without_line.class = Some(class("foam"));
        without_tax.lines = vec![without_line];

        let Posting::Entry(entry) = post(&without_tax, 1, &ctx).expect("posts") else {
            panic!("expected an entry");
        };
        assert!(entry
            .lines
            .iter()
            .all(|entry_line| entry_line.account != acct(chart::SALES_TAX_PAYABLE)));

        let mut with_tax = minimal_doc(DocKind::Invoice);
        let mut with_line = line(1, LineKind::Item, 10000);
        with_line.class = Some(class("foam"));
        with_tax.lines = vec![with_line];
        with_tax.tax = Some(TaxDetail {
            total_tax: Money::from_minor(663),
            taxable_base: Money::from_minor(10000),
            rate: dec!(0.06625),
        });

        let Posting::Entry(entry) = post(&with_tax, 1, &ctx).expect("posts") else {
            panic!("expected an entry");
        };
        let tax_line = entry
            .lines
            .iter()
            .find(|entry_line| entry_line.account == acct(chart::SALES_TAX_PAYABLE))
            .expect("tax line present");
        assert_eq!(tax_line.credit, Money::from_minor(663));
    }
}
