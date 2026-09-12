//! The shapes every ledger module shares. `LEDGER-DESIGN.md` §1, §4.
//!
//! A [`LedgerDocument`] is what the UI saves and what the importer derives from
//! a QBO payload: it is deliberately not QBO's shape. A [`JournalEntry`] is what
//! [`crate::post`] produces from one and what [`crate::store`] persists. Nothing
//! in this file touches a database or Intuit; it is the contract the three
//! sides are built against, so change it deliberately and in one place.

use chrono::NaiveDate;
use ledger_core::Money;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// One of the two books. `'aquamentor'` or `'waterline'`; the two share
/// nothing, so every function takes the company first (`DESIGN.md` §2.1 rule,
/// carried over).
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct CompanyId(pub String);

/// A ledger account, keyed by its number in the §2 chart (`"1200"`), never by
/// name. [`crate::chart`] holds the constants.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, PartialOrd, Ord)]
pub struct AccountId(pub String);

/// A class from the §3 taxonomy (`"foam"`, `"sign"`, ...).
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct ClassId(pub String);

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum ContactKind {
    Customer,
    Vendor,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ContactRef {
    pub kind: ContactKind,
    pub id: String,
}

/// Every document the §1 table has a row for. The order is the table's order.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum DocKind {
    Estimate,
    Invoice,
    SalesReceipt,
    CreditMemo,
    RefundReceipt,
    Payment,
    PurchaseOrder,
    Bill,
    BillPayment,
    VendorCredit,
    Purchase,
    Deposit,
    JournalEntry,
    RawMaterialReceipt,
    Build,
    InventoryAdjustment,
    BankLine,
    Settlement,
}

impl DocKind {
    pub const ALL: &'static [DocKind] = &[
        DocKind::Estimate,
        DocKind::Invoice,
        DocKind::SalesReceipt,
        DocKind::CreditMemo,
        DocKind::RefundReceipt,
        DocKind::Payment,
        DocKind::PurchaseOrder,
        DocKind::Bill,
        DocKind::BillPayment,
        DocKind::VendorCredit,
        DocKind::Purchase,
        DocKind::Deposit,
        DocKind::JournalEntry,
        DocKind::RawMaterialReceipt,
        DocKind::Build,
        DocKind::InventoryAdjustment,
        DocKind::BankLine,
        DocKind::Settlement,
    ];

    /// Estimates and purchase orders are contracts, not transactions (§1).
    pub const fn posts(self) -> bool {
        !matches!(self, DocKind::Estimate | DocKind::PurchaseOrder)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            DocKind::Estimate => "estimate",
            DocKind::Invoice => "invoice",
            DocKind::SalesReceipt => "sales_receipt",
            DocKind::CreditMemo => "credit_memo",
            DocKind::RefundReceipt => "refund_receipt",
            DocKind::Payment => "payment",
            DocKind::PurchaseOrder => "purchase_order",
            DocKind::Bill => "bill",
            DocKind::BillPayment => "bill_payment",
            DocKind::VendorCredit => "vendor_credit",
            DocKind::Purchase => "purchase",
            DocKind::Deposit => "deposit",
            DocKind::JournalEntry => "journal_entry",
            DocKind::RawMaterialReceipt => "raw_material_receipt",
            DocKind::Build => "build",
            DocKind::InventoryAdjustment => "inventory_adjustment",
            DocKind::BankLine => "bank_line",
            DocKind::Settlement => "settlement",
        }
    }

    pub fn parse(raw: &str) -> Option<DocKind> {
        DocKind::ALL
            .iter()
            .copied()
            .find(|kind| kind.as_str() == raw)
    }
}

/// What a document line is, which decides which §1 legs it produces.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum LineKind {
    /// Sells or buys a catalogue item; the item's accounts decide the legs.
    Item,
    /// Posts straight to a named account (non item bill, expense, deposit line).
    Account,
    /// Discount against the document (W9, 4900).
    Discount,
    /// Shipping charged to the customer (W6, 4300).
    Shipping,
    /// A description-only line. Contributes nothing (D9).
    Description,
    /// A journal entry line: `posting` says which side.
    Journal,
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum Side {
    Debit,
    Credit,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct DocLine {
    pub line_no: i64,
    pub kind: LineKind,
    /// Extended amount before tax, positive for a normal line.
    pub amount: Money,
    pub class: Option<ClassId>,
    pub item_id: Option<String>,
    /// For `Account` and `Journal` lines: the account the line names.
    pub account: Option<AccountId>,
    pub is_taxable: bool,
    /// Decimal quantity, exactly as entered. `None` for service and account lines.
    pub qty: Option<Decimal>,
    /// For inventory item lines: unit cost at the time, so the COGS leg can be
    /// computed without a second lookup.
    pub unit_cost: Option<Money>,
    pub description: Option<String>,
    /// `Journal` lines only.
    pub posting: Option<Side>,
    /// Payment/receipt lines: which contact the money belongs to (deposit splits).
    pub entity: Option<ContactRef>,
}

/// The tax the document carries in total. Line taxability is on each line.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct TaxDetail {
    pub total_tax: Money,
    pub taxable_base: Money,
    /// The rate in force on `txn_date`, as a fraction (`0.06625`).
    pub rate: Decimal,
}

/// How much of a payment, credit or bill payment applies to which document.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Application {
    pub target_document_id: String,
    pub target_kind: DocKind,
    pub amount: Money,
}

/// The document as saved. `payload_json` in `document_versions` is this, serialised.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct LedgerDocument {
    pub document_id: String,
    pub kind: DocKind,
    pub number: Option<String>,
    pub txn_date: NaiveDate,
    pub due_date: Option<NaiveDate>,
    pub contact: Option<ContactRef>,
    pub header_class: Option<ClassId>,
    pub lines: Vec<DocLine>,
    pub tax: Option<TaxDetail>,
    /// Money-in documents: where the money lands. `None` means 1150
    /// undeposited funds (W8).
    pub deposit_to: Option<AccountId>,
    /// Money-out documents: the bank or card account paying.
    pub pay_from: Option<AccountId>,
    /// Payments, credits, bill payments, vendor credits.
    pub applications: Vec<Application>,
    /// The remainder no application claims. Parks in 2300 (W8) or 2000.
    pub unapplied: Money,
    /// A voided document posts a reversal rather than vanishing (§6).
    pub is_voided: bool,
    /// The QBO id when imported or mirrored. Kept forever.
    pub source_ref: Option<String>,
    pub memo: Option<String>,
}

impl LedgerDocument {
    /// Sum of the line amounts, the pre-tax subtotal.
    pub fn subtotal(&self) -> Result<Money, ledger_core::MoneyError> {
        Money::checked_sum(self.lines.iter().map(|line| line.amount))
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct JournalLine {
    pub line_no: i64,
    pub account: AccountId,
    pub class: Option<ClassId>,
    /// Exactly one of `debit` and `credit` is non-zero (§4 CHECK constraints).
    pub debit: Money,
    pub credit: Money,
    pub memo: Option<String>,
    pub entity: Option<ContactRef>,
}

impl JournalLine {
    pub fn debit(line_no: i64, account: AccountId, amount: Money) -> Self {
        JournalLine {
            line_no,
            account,
            class: None,
            debit: amount,
            credit: Money::ZERO,
            memo: None,
            entity: None,
        }
    }

    pub fn credit(line_no: i64, account: AccountId, amount: Money) -> Self {
        JournalLine {
            line_no,
            account,
            class: None,
            debit: Money::ZERO,
            credit: amount,
            memo: None,
            entity: None,
        }
    }

    pub fn with_class(mut self, class: Option<ClassId>) -> Self {
        self.class = class;
        self
    }

    pub fn with_entity(mut self, entity: Option<ContactRef>) -> Self {
        self.entity = entity;
        self
    }
}

/// One balanced entry, derived from exactly one document version.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct JournalEntry {
    pub entry_date: NaiveDate,
    pub memo: Option<String>,
    pub source_type: DocKind,
    pub source_id: String,
    pub source_version: i64,
    /// Set on the reversing entry that undoes an earlier one.
    pub reversal_of: Option<String>,
    /// Manual and imported journal entries (§4, W18).
    pub is_flagged: bool,
    pub lines: Vec<JournalLine>,
}

impl JournalEntry {
    pub fn total_debits(&self) -> Result<Money, ledger_core::MoneyError> {
        Money::checked_sum(self.lines.iter().map(|line| line.debit))
    }

    pub fn total_credits(&self) -> Result<Money, ledger_core::MoneyError> {
        Money::checked_sum(self.lines.iter().map(|line| line.credit))
    }

    /// Debits equal credits and every line has exactly one non-zero side.
    pub fn is_balanced(&self) -> bool {
        let sides_ok = self.lines.iter().all(|line| {
            (line.debit == Money::ZERO) != (line.credit == Money::ZERO)
                && line.debit.minor() >= 0
                && line.credit.minor() >= 0
        });
        match (self.total_debits(), self.total_credits()) {
            (Ok(d), Ok(c)) => sides_ok && d == c && !self.lines.is_empty(),
            _ => false,
        }
    }

    /// The correction path (§4): same lines, sides swapped, dated `on`.
    pub fn reversed(&self, on: NaiveDate, reverses_entry_id: &str) -> JournalEntry {
        JournalEntry {
            entry_date: on,
            memo: self.memo.clone(),
            source_type: self.source_type,
            source_id: self.source_id.clone(),
            source_version: self.source_version,
            reversal_of: Some(reverses_entry_id.to_string()),
            is_flagged: self.is_flagged,
            lines: self
                .lines
                .iter()
                .map(|line| JournalLine {
                    line_no: line.line_no,
                    account: line.account.clone(),
                    class: line.class.clone(),
                    debit: line.credit,
                    credit: line.debit,
                    memo: line.memo.clone(),
                    entity: line.entity.clone(),
                })
                .collect(),
        }
    }
}

/// What the posting function needs to know about the world that is not on the
/// document: item accounts, defaults, the exemption flag. Built by the store
/// (live) or the importer (replay). Pure data, so `post` stays testable.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct PostingContext {
    pub items: std::collections::HashMap<String, ItemAccounts>,
    pub customers: std::collections::HashMap<String, CustomerPosting>,
    pub config: PostingConfig,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ItemAccounts {
    pub income: AccountId,
    /// COGS or expense account for the item.
    pub expense: AccountId,
    /// Inventory asset account for tracked items; `None` for services.
    pub asset: Option<AccountId>,
    pub default_class: Option<ClassId>,
    /// Current moving-average unit cost (W12), for the COGS leg when the line
    /// carries none.
    pub unit_cost: Option<Money>,
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct CustomerPosting {
    pub is_tax_exempt: bool,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PostingConfig {
    /// `sales_tax.nj_rate`, §9.
    pub sales_tax_rate: Decimal,
    /// Where a receipt lands when the document names no account (W8: 1150).
    pub default_deposit_account: AccountId,
    pub default_bank_account: AccountId,
    pub merchant_fee_account: AccountId,
}

impl Default for PostingConfig {
    fn default() -> Self {
        PostingConfig {
            sales_tax_rate: Decimal::new(6625, 5),
            default_deposit_account: AccountId(crate::chart::UNDEPOSITED_FUNDS.into()),
            default_bank_account: AccountId(crate::chart::CHECKING.into()),
            merchant_fee_account: AccountId(crate::chart::MERCHANT_FEES.into()),
        }
    }
}
