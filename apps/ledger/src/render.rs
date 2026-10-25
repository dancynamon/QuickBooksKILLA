//! Customer- and vendor-facing documents from the own book (D39): invoice,
//! estimate, sales receipt, credit memo, purchase order, and a customer
//! statement of open items with aging. After cutover QBO no longer renders
//! these, so the book must.
//!
//! Everything printed comes from saved documents and master data
//! ([`crate::books`]): the newest version of the document, the contact's
//! addresses and terms, each line's item SKU and description, and the open
//! balance [`books::open_documents`] derives from payments and credits. The
//! company's own identity (name, address, logo, footer) comes from the
//! gitignored `.local/config.toml` (`[documents]`, see [`RenderConfig`]),
//! else the ledger's `companies` row — never from code.
//!
//! Three steps, each its own function so the tests can hold each one:
//!
//! 1. [`document_sheet`] / [`statement_sheet`] build a [`Sheet`]: plain data
//!    saying what goes on the page (title, meta, address blocks, rows,
//!    totals, notes, VOID).
//! 2. [`layout`] turns a sheet into pages ([`pdf::PdfDocument`]) in the
//!    house style of the `branded-invoice` skill's renderer: Letter, 0.8in
//!    margins, Helvetica, navy `#1B3A5B` accents, no heavy fills; paginated,
//!    with the table header repeated and "Page x of y" on every page.
//! 3. [`render_document`] / [`render_statement`] write the file into the
//!    output directory ([`output_dir`]), which must sit outside any git
//!    work tree (or in its ignored `target/` or `.local/`).
//!
//! **Pay link.** For an open invoice, [`render_document`] asks
//! [`link::payment_link`] for an Authorize.net hosted page when a client is
//! configured, writes the self-submitting page beside the PDF
//! (`<name>.pay.html`) and puts a PAY ONLINE box on the invoice. The token
//! lasts 15 minutes (D30), so the PDF never prints it: the box points at
//! `[documents] pay_url` (the durable mint-on-click endpoint D30 leaves as a
//! follow-up) when that is set, else at "the secure payment link sent with
//! this invoice". With no client, missing credentials, or any gateway
//! failure, the box is simply left off and the result says why.

pub mod pdf;

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use chrono::{DateTime, NaiveDate, Utc};
use rusqlite::{params, OptionalExtension};
use rust_decimal::Decimal;
use serde::Deserialize;

use ledger_core::Money;

use crate::authnet::link::{self, HostedPaymentClient, LinkError, PaymentLink};
use crate::books::{self, AgingRow, Book, BooksError, Contact};
use crate::store::{Ledger, LedgerError};
use crate::types::{ContactKind, DocKind, LedgerDocument, LineKind};

use pdf::{Color, Font, Page, PdfDocument, PAGE_HEIGHT, PAGE_WIDTH};

#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error(transparent)]
    Books(#[from] BooksError),
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error(transparent)]
    Money(#[from] ledger_core::MoneyError),
    #[error("{path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("config {path}: {reason}")]
    Config { path: String, reason: String },
    #[error("a {0} is not a customer or vendor document; render_document prints invoices, estimates, sales receipts, credit memos and purchase orders")]
    NotRenderable(&'static str),
    #[error("unknown contact {0}")]
    UnknownContact(String),
    #[error("{0} is a vendor; a statement is for a customer")]
    NotACustomer(String),
    #[error(
        "{0} is inside the git work tree at {1}; customer documents go outside the repository \
         (or in its ignored target/ or .local/)"
    )]
    InsideRepository(String, String),
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> RenderError + '_ {
    move |source| RenderError::Io {
        path: path.display().to_string(),
        source,
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Who the documents are from. Every field but the name is optional; a
/// company with no `[documents.company.<id>]` table prints the ledger's own
/// display name and nothing else.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompanyIdentity {
    pub name: String,
    /// Printed under the logo when it differs from `name`.
    #[serde(default)]
    pub legal_name: Option<String>,
    #[serde(default)]
    pub address: Vec<String>,
    #[serde(default)]
    pub phone: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub website: Option<String>,
    /// A PNG (8-bit, non-interlaced). Missing or unreadable, the header
    /// falls back to the name as a wordmark.
    #[serde(default)]
    pub logo: Option<PathBuf>,
    #[serde(default)]
    pub footer: Option<String>,
}

/// `[documents]` in `.local/config.toml`:
///
/// ```toml
/// [documents]
/// output_dir = "~/Documents/Ledger Documents"   # one subfolder per company
/// pay_url = "https://pay.example.com/i/{number}" # optional, D30 follow-up
///
/// [documents.company.aquamentor]
/// name = "Aquamentor"
/// legal_name = "Suspended Aquatic Mentor Inc"
/// address = ["Street", "Town, ST 00000"]
/// phone = "(000) 000-0000"
/// email = "orders@example.com"
/// logo = "/abs/path/logo.png"
/// footer = "Thank you for your business."
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderConfig {
    #[serde(default)]
    pub output_dir: Option<PathBuf>,
    /// A durable pay page printed on open invoices; `{number}` and
    /// `{document_id}` are filled in.
    #[serde(default)]
    pub pay_url: Option<String>,
    #[serde(default)]
    pub company: HashMap<String, CompanyIdentity>,
}

#[derive(Deserialize)]
struct ConfigFile {
    #[serde(default)]
    documents: Option<toml::Value>,
}

impl RenderConfig {
    /// `[documents]` from `path`. A missing file or table is the default
    /// (no identity beyond the ledger's company name, default output
    /// directory); the rest of the file belongs to `qbo-local` and
    /// `[authnet]` and is ignored here.
    pub fn load(path: &Path) -> Result<RenderConfig, RenderError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(RenderConfig::default())
            }
            Err(source) => return Err(io_err(path)(source)),
        };
        RenderConfig::parse(&text).map_err(|reason| RenderError::Config {
            path: path.display().to_string(),
            reason,
        })
    }

    pub fn parse(text: &str) -> Result<RenderConfig, String> {
        let file: ConfigFile = toml::from_str(text).map_err(|e| e.to_string())?;
        match file.documents {
            None => Ok(RenderConfig::default()),
            Some(table) => table.try_into().map_err(|e: toml::de::Error| e.to_string()),
        }
    }
}

/// The config file read when none is named: `LEDGER_DOCUMENTS_CONFIG`, else
/// `.local/config.toml` (the same file `[authnet]` lives in).
pub fn default_config_path() -> PathBuf {
    std::env::var_os("LEDGER_DOCUMENTS_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".local/config.toml"))
}

fn expand_home(path: &Path) -> PathBuf {
    match (path.strip_prefix("~"), std::env::var_os("HOME")) {
        (Ok(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => path.to_path_buf(),
    }
}

/// Where `company`'s documents go: `explicit` if given, else
/// `LEDGER_DOCUMENTS_DIR`, else `[documents] output_dir`, else
/// `~/Documents/Ledger Documents` — each with the company id appended
/// except `explicit`. Refused when it lands inside a git work tree other
/// than in that tree's `target/` or `.local/` (both ignored), so a customer
/// PDF can never be committed by accident. Created if missing.
pub fn output_dir(
    config: &RenderConfig,
    company: &str,
    explicit: Option<&Path>,
) -> Result<PathBuf, RenderError> {
    let dir = match explicit {
        Some(dir) => expand_home(dir),
        None => {
            let base = std::env::var_os("LEDGER_DOCUMENTS_DIR")
                .map(PathBuf::from)
                .or_else(|| config.output_dir.clone())
                .unwrap_or_else(|| PathBuf::from("~/Documents/Ledger Documents"));
            expand_home(&base).join(company)
        }
    };
    let dir = if dir.is_absolute() {
        dir
    } else {
        std::env::current_dir().map_err(io_err(&dir))?.join(dir)
    };
    check_outside_repository(&dir)?;
    std::fs::create_dir_all(&dir).map_err(io_err(&dir))?;
    Ok(dir)
}

/// Resolves `dir` through its nearest existing ancestor (so symlinks such
/// as macOS's `/tmp` are followed before anything is created) and refuses
/// it if a `.git` sits above it, unless the path below that work tree
/// starts with `target` or `.local`.
fn check_outside_repository(dir: &Path) -> Result<(), RenderError> {
    let mut existing = dir.to_path_buf();
    let mut rest = Vec::new();
    while !existing.exists() {
        match (existing.file_name(), existing.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name.to_os_string());
                existing = parent.to_path_buf();
            }
            _ => break,
        }
    }
    let mut resolved = existing.canonicalize().map_err(io_err(&existing))?;
    for name in rest.into_iter().rev() {
        resolved.push(name);
    }
    for ancestor in resolved.ancestors() {
        if ancestor.join(".git").exists() {
            let below = resolved.strip_prefix(ancestor).unwrap_or(&resolved);
            let first = below.components().next();
            let ignored = matches!(first, Some(Component::Normal(name)) if name == "target" || name == ".local");
            if !ignored {
                return Err(RenderError::InsideRepository(
                    resolved.display().to_string(),
                    ancestor.display().to_string(),
                ));
            }
            break;
        }
    }
    Ok(())
}

/// The identity printed for `company`: the config's table, else the
/// ledger's `companies.display_name`, else the id itself.
pub fn company_identity(
    ledger: &Ledger,
    company: &str,
    config: &RenderConfig,
) -> Result<CompanyIdentity, RenderError> {
    if let Some(identity) = config.company.get(company) {
        return Ok(identity.clone());
    }
    let name: Option<String> = ledger
        .conn()
        .query_row(
            "SELECT display_name FROM companies WHERE company_id = ?1",
            params![company],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| RenderError::Books(BooksError::Sqlite(e)))?;
    Ok(CompanyIdentity {
        name: name.unwrap_or_else(|| company.to_string()),
        ..CompanyIdentity::default()
    })
}

// ---------------------------------------------------------------------------
// What goes on the page
// ---------------------------------------------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Column {
    pub label: &'static str,
    /// Points. Exactly one column has width 0: it takes what is left and
    /// wraps.
    pub width: f32,
    pub align: Align,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Row {
    pub cells: Vec<String>,
    /// A grey second line under the wrapping column.
    pub detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TotalLine {
    pub label: String,
    pub value: String,
    /// Bold, navy, larger, with a rule above: TOTAL, BALANCE DUE.
    pub emphasis: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct AddressBlock {
    pub label: &'static str,
    pub lines: Vec<String>,
}

/// A PAY ONLINE box: the lines it prints and the URI it opens, if any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PayBox {
    pub lines: Vec<String>,
    pub uri: Option<String>,
}

/// Everything one document prints, before layout.
#[derive(Clone, Debug, PartialEq)]
pub struct Sheet {
    pub title: String,
    /// `(label, value)`, printed right-aligned under the title.
    pub meta: Vec<(String, String)>,
    pub left: AddressBlock,
    pub right: Option<AddressBlock>,
    pub columns: Vec<Column>,
    pub rows: Vec<Row>,
    /// Printed in place of the table body when there are no rows.
    pub empty_text: Option<String>,
    pub totals: Vec<TotalLine>,
    pub pay: Option<PayBox>,
    pub notes: Vec<String>,
    pub void: bool,
}

/// `$1,234.56`, a negative as `-$12.00`.
pub fn usd(amount: Money) -> String {
    let minor = amount.minor();
    let sign = if minor < 0 { "-" } else { "" };
    let abs = minor.unsigned_abs();
    let dollars = (abs / 100).to_string();
    let mut grouped = String::new();
    for (i, c) in dollars.chars().enumerate() {
        if i > 0 && (dollars.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(c);
    }
    format!("{sign}${grouped}.{:02}", abs % 100)
}

fn date(d: NaiveDate) -> String {
    d.format("%m/%d/%Y").to_string()
}

fn quantity(qty: Decimal) -> String {
    qty.normalize().to_string()
}

/// Amount over quantity: two places, or up to four when the price needs
/// them (a 0.125 unit rate).
fn rate(amount: Money, qty: Decimal) -> Option<String> {
    if qty.is_zero() {
        return None;
    }
    let mut value = (Decimal::new(amount.minor(), 2) / qty)
        .round_dp(4)
        .normalize();
    if value.scale() < 2 {
        value.rescale(2);
    }
    let text = value.to_string();
    Some(match text.strip_prefix('-') {
        Some(rest) => format!("-${rest}"),
        None => format!("${text}"),
    })
}

fn address_lines(name: &str, address: Option<&str>) -> Vec<String> {
    let mut lines = vec![name.to_string()];
    if let Some(address) = address {
        lines.extend(
            address
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(String::from),
        );
    }
    lines
}

fn contact_name(contact: &Contact) -> &str {
    contact
        .company_name
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(&contact.display_name)
}

const fn title_of(kind: DocKind) -> Option<&'static str> {
    Some(match kind {
        DocKind::Invoice => "INVOICE",
        DocKind::Estimate => "ESTIMATE",
        DocKind::SalesReceipt => "SALES RECEIPT",
        DocKind::CreditMemo => "CREDIT MEMO",
        DocKind::PurchaseOrder => "PURCHASE ORDER",
        _ => return None,
    })
}

const fn file_label(kind: DocKind) -> &'static str {
    match kind {
        DocKind::Invoice => "Invoice",
        DocKind::Estimate => "Estimate",
        DocKind::SalesReceipt => "Sales Receipt",
        DocKind::CreditMemo => "Credit Memo",
        DocKind::PurchaseOrder => "Purchase Order",
        _ => "Document",
    }
}

/// Later than any saved date: every payment and credit counts toward the
/// balance printed (dates are ISO text in the store, so four digits).
fn all_time() -> NaiveDate {
    NaiveDate::from_ymd_opt(9999, 12, 31).expect("valid date")
}

/// The open balance on `doc` across every saved application, signed as
/// [`books::open_documents`] signs it (negative for a credit memo). Zero
/// when nothing is open, or for a kind that opens nothing.
fn open_balance(
    ledger: &Ledger,
    company: &str,
    doc: &LedgerDocument,
) -> Result<Money, RenderError> {
    let book = match doc.kind {
        DocKind::Invoice | DocKind::CreditMemo => Book::Receivable,
        _ => return Ok(Money::ZERO),
    };
    if doc.is_voided {
        return Ok(Money::ZERO);
    }
    Ok(books::open_documents(ledger, company, book, all_time())?
        .into_iter()
        .find(|open| open.document_id == doc.document_id)
        .map(|open| open.open)
        .unwrap_or(Money::ZERO))
}

/// The document columns: SKU, DESCRIPTION (wraps), QTY, RATE, AMOUNT.
fn line_columns() -> Vec<Column> {
    vec![
        Column {
            label: "SKU",
            width: 82.0,
            align: Align::Left,
        },
        Column {
            label: "DESCRIPTION",
            width: 0.0,
            align: Align::Left,
        },
        Column {
            label: "QTY",
            width: 46.0,
            align: Align::Right,
        },
        Column {
            label: "RATE",
            width: 72.0,
            align: Align::Right,
        },
        Column {
            label: "AMOUNT",
            width: 80.0,
            align: Align::Right,
        },
    ]
}

/// What a document's numbers come to, as printed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentFigures {
    pub subtotal: Money,
    pub discount: Money,
    pub tax: Money,
    pub total: Money,
    /// Invoice and sales receipt: what has been paid. Credit memo: what has
    /// been applied.
    pub paid: Money,
    /// Invoice: what is still owed. Credit memo: the credit left. Zero for
    /// the rest and for anything voided.
    pub balance: Money,
}

/// The sheet for one saved document, from the document's newest version
/// and the master data it names. `pay` is the box an open invoice gets when
/// a pay link was minted ([`render_document`] decides).
pub fn document_sheet(
    ledger: &Ledger,
    company: &str,
    doc: &LedgerDocument,
    identity: &CompanyIdentity,
    pay: Option<PayBox>,
) -> Result<(Sheet, DocumentFigures), RenderError> {
    let title = title_of(doc.kind).ok_or(RenderError::NotRenderable(doc.kind.as_str()))?;
    let contact = match &doc.contact {
        Some(c) => books::contact(ledger, company, &c.id)?,
        None => None,
    };

    // Lines and the totals they make.
    let mut rows = Vec::new();
    let mut subtotal = Money::ZERO;
    let mut discount = Money::ZERO;
    for line in &doc.lines {
        let item = match &line.item_id {
            Some(id) => books::item(ledger, company, id)?,
            None => None,
        };
        let description = line
            .description
            .clone()
            .filter(|d| !d.trim().is_empty())
            .or_else(|| item.as_ref().and_then(|i| i.description.clone()))
            .or_else(|| item.as_ref().map(|i| i.name.clone()))
            .unwrap_or_default();
        let sku = item
            .as_ref()
            .map(|i| i.sku.clone().unwrap_or_else(|| i.name.clone()))
            .unwrap_or_default();
        match line.kind {
            LineKind::Item | LineKind::Shipping | LineKind::Account => {
                subtotal = subtotal.checked_add(line.amount)?;
                let description = if description.is_empty() && line.kind == LineKind::Shipping {
                    "Shipping".to_string()
                } else {
                    description
                };
                rows.push(Row {
                    cells: vec![
                        sku,
                        description,
                        line.qty.map(quantity).unwrap_or_default(),
                        line.qty
                            .and_then(|q| rate(line.amount, q))
                            .unwrap_or_default(),
                        usd(line.amount),
                    ],
                    detail: None,
                });
            }
            LineKind::Description => rows.push(Row {
                cells: vec![
                    sku,
                    description,
                    String::new(),
                    String::new(),
                    String::new(),
                ],
                detail: None,
            }),
            LineKind::Discount => discount = discount.checked_add(line.amount)?,
            LineKind::Journal => {}
        }
    }
    let tax = doc.tax.as_ref().map(|t| t.total_tax).unwrap_or(Money::ZERO);
    let total = books::document_total(doc)?;

    let open = open_balance(ledger, company, doc)?;
    let (paid, balance) = match doc.kind {
        _ if doc.is_voided => (Money::ZERO, Money::ZERO),
        DocKind::Invoice => (total.checked_sub(open)?, open),
        DocKind::SalesReceipt => (total, Money::ZERO),
        DocKind::CreditMemo => {
            let left = open.checked_neg()?;
            (total.checked_sub(left)?, left)
        }
        _ => (Money::ZERO, Money::ZERO),
    };

    let mut totals = vec![TotalLine {
        label: "Subtotal".into(),
        value: usd(subtotal),
        emphasis: false,
    }];
    if !discount.is_zero() {
        totals.push(TotalLine {
            label: "Discount".into(),
            value: usd(discount.checked_neg()?),
            emphasis: false,
        });
    }
    if let Some(detail) = &doc.tax {
        let label = if detail.rate.is_zero() {
            "Sales Tax".to_string()
        } else {
            format!(
                "Sales Tax ({}%)",
                (detail.rate * Decimal::from(100)).normalize()
            )
        };
        totals.push(TotalLine {
            label,
            value: usd(detail.total_tax),
            emphasis: false,
        });
    }
    let total_label = match doc.kind {
        DocKind::CreditMemo => "TOTAL CREDIT",
        _ => "TOTAL",
    };
    totals.push(TotalLine {
        label: total_label.into(),
        value: usd(total),
        emphasis: true,
    });
    match doc.kind {
        DocKind::Invoice => {
            totals.push(TotalLine {
                label: "Amount Paid".into(),
                value: usd(paid),
                emphasis: false,
            });
            totals.push(TotalLine {
                label: "BALANCE DUE".into(),
                value: usd(balance),
                emphasis: true,
            });
        }
        DocKind::SalesReceipt => {
            totals.push(TotalLine {
                label: "Amount Paid".into(),
                value: usd(paid),
                emphasis: false,
            });
            totals.push(TotalLine {
                label: "BALANCE DUE".into(),
                value: usd(balance),
                emphasis: false,
            });
        }
        DocKind::CreditMemo => {
            totals.push(TotalLine {
                label: "Applied".into(),
                value: usd(paid),
                emphasis: false,
            });
            totals.push(TotalLine {
                label: "REMAINING CREDIT".into(),
                value: usd(balance),
                emphasis: true,
            });
        }
        _ => {}
    }

    // Header.
    let number = doc.number.clone().unwrap_or_else(|| "—".to_string());
    let number_label = match doc.kind {
        DocKind::Invoice => "Invoice #",
        DocKind::Estimate => "Estimate #",
        DocKind::SalesReceipt => "Receipt #",
        DocKind::CreditMemo => "Credit Memo #",
        _ => "P.O. #",
    };
    let mut meta = vec![
        (number_label.to_string(), number),
        ("Date".to_string(), date(doc.txn_date)),
    ];
    // A contact on day-of-month terms (D41) shows them by name when they
    // gave this due date; otherwise the terms are read off the due date.
    let day_of_month = contact
        .as_ref()
        .map(|c| c.terms())
        .filter(|t| matches!(t, books::Terms::DayOfMonth { .. }))
        .filter(|t| {
            doc.due_date
                .is_none_or(|due| due == t.due_date(doc.txn_date))
        });
    let terms_days = doc
        .due_date
        .map(|due| (due - doc.txn_date).num_days())
        .or_else(|| contact.as_ref().and_then(|c| c.terms_days));
    match doc.kind {
        DocKind::Invoice => {
            if let Some(terms) = day_of_month {
                meta.push(("Terms".into(), terms.label()));
            } else if let Some(days) = terms_days {
                meta.push(("Terms".into(), books::Terms::Net { days }.label()));
            }
            meta.push((
                "Due Date".into(),
                date(doc.due_date.unwrap_or(doc.txn_date)),
            ));
        }
        DocKind::Estimate | DocKind::PurchaseOrder => {
            if let Some(due) = doc.due_date {
                let label = if doc.kind == DocKind::Estimate {
                    "Expires"
                } else {
                    "Expected"
                };
                meta.push((label.into(), date(due)));
            }
        }
        _ => {}
    }
    if doc.is_voided {
        meta.push(("Status".into(), "VOID".into()));
    }

    let (name, bill, ship) = match &contact {
        Some(c) => (
            contact_name(c).to_string(),
            c.bill_address.clone(),
            c.ship_address.clone(),
        ),
        None => (
            doc.contact
                .as_ref()
                .map(|c| c.id.clone())
                .unwrap_or_default(),
            None,
            None,
        ),
    };
    let (left, right) = if doc.kind == DocKind::PurchaseOrder {
        let mut ours = vec![identity.name.clone()];
        ours.extend(identity.address.iter().cloned());
        (
            AddressBlock {
                label: "VENDOR",
                lines: address_lines(&name, bill.as_deref()),
            },
            Some(AddressBlock {
                label: "SHIP TO",
                lines: ours,
            }),
        )
    } else {
        (
            AddressBlock {
                label: "BILL TO",
                lines: address_lines(&name, bill.as_deref()),
            },
            ship.filter(|s| !s.trim().is_empty())
                .map(|ship| AddressBlock {
                    label: "SHIP TO",
                    lines: address_lines(&name, Some(&ship)),
                }),
        )
    };

    let notes = doc
        .memo
        .iter()
        .filter(|m| !m.trim().is_empty())
        .cloned()
        .collect();

    let figures = DocumentFigures {
        subtotal,
        discount,
        tax,
        total,
        paid,
        balance,
    };
    let sheet = Sheet {
        title: title.to_string(),
        meta,
        left,
        right,
        columns: line_columns(),
        rows,
        empty_text: None,
        totals,
        pay,
        notes,
        void: doc.is_voided,
    };
    Ok((sheet, figures))
}

/// One customer's open items and their aging as of a date — the same rows
/// and buckets [`books::aging`] gives that customer, so the statement ties
/// to the AR aging report.
pub fn statement_sheet(
    ledger: &Ledger,
    company: &str,
    contact_id: &str,
    as_of: NaiveDate,
) -> Result<(Sheet, AgingRow), RenderError> {
    let contact = books::contact(ledger, company, contact_id)?
        .ok_or_else(|| RenderError::UnknownContact(contact_id.to_string()))?;
    if contact.kind != ContactKind::Customer {
        return Err(RenderError::NotACustomer(contact.display_name));
    }
    let open: Vec<books::OpenDocument> =
        books::open_documents(ledger, company, Book::Receivable, as_of)?
            .into_iter()
            .filter(|doc| doc.contact_id == contact_id)
            .collect();

    let mut aging = AgingRow {
        contact_id: contact.contact_id.clone(),
        contact_name: contact.display_name.clone(),
        ..AgingRow::default()
    };
    let mut rows = Vec::new();
    for doc in &open {
        let days = (as_of - doc.due_date).num_days();
        aging.add(days, doc.open)?;
        let label = match doc.kind {
            DocKind::CreditMemo => "Credit Memo",
            DocKind::Invoice => "Invoice",
            other => file_label(other),
        };
        let description = match &doc.number {
            Some(number) => format!("{label} #{number}"),
            None => label.to_string(),
        };
        let detail = if doc.kind == DocKind::CreditMemo {
            "Credit available".to_string()
        } else if days > 0 {
            format!("{days} day{} past due", if days == 1 { "" } else { "s" })
        } else {
            "Current".to_string()
        };
        rows.push(Row {
            cells: vec![
                date(doc.txn_date),
                description,
                date(doc.due_date),
                usd(doc.total),
                usd(doc.open),
            ],
            detail: Some(detail),
        });
    }

    let past_due = Money::checked_sum([
        aging.days_1_30,
        aging.days_31_60,
        aging.days_61_90,
        aging.over_90,
    ])?;
    let bucket = |label: &str, value: Money| TotalLine {
        label: label.to_string(),
        value: usd(value),
        emphasis: false,
    };
    let totals = vec![
        bucket("Current", aging.current),
        bucket("1-30 Days Past Due", aging.days_1_30),
        bucket("31-60 Days Past Due", aging.days_31_60),
        bucket("61-90 Days Past Due", aging.days_61_90),
        bucket("Over 90 Days Past Due", aging.over_90),
        TotalLine {
            label: "TOTAL DUE".into(),
            value: usd(aging.total),
            emphasis: true,
        },
    ];
    let meta = vec![
        ("Statement Date".to_string(), date(as_of)),
        ("Customer".to_string(), contact.display_name.clone()),
        ("Total Due".to_string(), usd(aging.total)),
        ("Past Due".to_string(), usd(past_due)),
    ];
    let columns = vec![
        Column {
            label: "DATE",
            width: 70.0,
            align: Align::Left,
        },
        Column {
            label: "DESCRIPTION",
            width: 0.0,
            align: Align::Left,
        },
        Column {
            label: "DUE DATE",
            width: 70.0,
            align: Align::Left,
        },
        Column {
            label: "AMOUNT",
            width: 84.0,
            align: Align::Right,
        },
        Column {
            label: "OPEN BALANCE",
            width: 90.0,
            align: Align::Right,
        },
    ];
    let sheet = Sheet {
        title: "STATEMENT".into(),
        meta,
        left: AddressBlock {
            label: "BILL TO",
            lines: address_lines(contact_name(&contact), contact.bill_address.as_deref()),
        },
        right: None,
        columns,
        rows,
        empty_text: Some(format!("No open items as of {}.", date(as_of))),
        totals,
        pay: None,
        notes: Vec::new(),
        void: false,
    };
    Ok((sheet, aging))
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

const NAVY: Color = Color::hex(0x1B3A5B);
const GREY: Color = Color::hex(0x888888);
const DARK: Color = Color::hex(0x444444);
const RULE: Color = Color::hex(0xCCCCCC);
const VOID_RED: Color = Color::hex(0xF2C4C4);
const MARGIN: f32 = 57.6; // 0.8in
const BOTTOM: f32 = MARGIN + 22.0; // content stops above the footer
const PAD: f32 = 6.0;
const BODY: f32 = 9.5;
const LEADING: f32 = 12.0;

struct Writer<'a> {
    sheet: &'a Sheet,
    identity: &'a CompanyIdentity,
    logo: Option<(usize, u32, u32)>,
    doc: PdfDocument,
    page: Page,
    y: f32,
}

impl<'a> Writer<'a> {
    fn content_width() -> f32 {
        PAGE_WIDTH - 2.0 * MARGIN
    }

    /// Starts a page: the watermark first, so everything prints over it.
    fn fresh_page(&mut self) {
        let mut page = Page::new();
        if self.sheet.void {
            page.text_rotated_center(
                (PAGE_WIDTH / 2.0, PAGE_HEIGHT / 2.0),
                35.0,
                Font::Bold,
                170.0,
                VOID_RED,
                "VOID",
            );
        }
        self.page = page;
    }

    fn finish_page(&mut self) {
        let page = std::mem::take(&mut self.page);
        self.doc.pages.push(page);
    }

    fn full_header(&mut self) {
        let top = PAGE_HEIGHT - MARGIN;
        let mut left_y = match self.logo {
            Some((index, w, h)) => {
                let max_w = 180.0;
                let max_h = 64.0;
                let scale = (max_w / w as f32).min(max_h / h as f32);
                let (lw, lh) = (w as f32 * scale, h as f32 * scale);
                self.page.image(index, MARGIN, top - lh + 4.0, lw, lh);
                top - lh - 10.0
            }
            None => {
                self.page.text(
                    MARGIN,
                    top - 18.0,
                    Font::Bold,
                    22.0,
                    NAVY,
                    &self.identity.name,
                );
                top - 34.0
            }
        };
        let mut company_lines = Vec::new();
        if let Some(legal) = &self.identity.legal_name {
            if legal != &self.identity.name || self.logo.is_some() {
                company_lines.push(legal.clone());
            }
        }
        company_lines.extend(self.identity.address.iter().cloned());
        let contact: Vec<&str> = [
            self.identity.phone.as_deref(),
            self.identity.email.as_deref(),
            self.identity.website.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect();
        if !contact.is_empty() {
            company_lines.push(contact.join("  •  "));
        }
        for line in &company_lines {
            self.page
                .text(MARGIN, left_y, Font::Regular, 9.0, DARK, line);
            left_y -= 12.5;
        }

        let right = PAGE_WIDTH - MARGIN;
        self.page
            .text_right(right, top - 18.0, Font::Bold, 24.0, NAVY, &self.sheet.title);
        let mut meta_y = top - 38.0;
        for (label, value) in &self.sheet.meta {
            // Label first, so the file reads "Invoice #:" then "1001".
            let value_w = pdf::text_width(Font::Regular, 9.0, value);
            self.page.text_right(
                right - value_w - 4.0,
                meta_y,
                Font::Bold,
                9.0,
                NAVY,
                &format!("{label}:"),
            );
            self.page
                .text_right(right, meta_y, Font::Regular, 9.0, Color::BLACK, value);
            meta_y -= 13.0;
        }

        let mut y = left_y.min(meta_y) - 4.0;
        self.page.line(MARGIN, y, right, y, 1.5, NAVY);
        y -= 22.0;

        // Address blocks.
        let col_x = PAGE_WIDTH / 2.0 + 10.0;
        let col_w = PAGE_WIDTH / 2.0 - MARGIN - 20.0;
        let mut blocks = vec![(MARGIN, &self.sheet.left)];
        if let Some(right_block) = &self.sheet.right {
            blocks.push((col_x, right_block));
        }
        let mut lowest = y;
        for (x, block) in blocks {
            self.page.text(x, y, Font::Bold, 9.5, NAVY, block.label);
            let mut by = y - 15.0;
            for (i, line) in block.lines.iter().enumerate() {
                let font = if i == 0 { Font::Bold } else { Font::Regular };
                for piece in pdf::wrap(font, 10.0, line, col_w) {
                    self.page.text(x, by, font, 10.0, Color::BLACK, &piece);
                    by -= 13.5;
                }
            }
            lowest = lowest.min(by);
        }
        self.y = lowest - 14.0;
    }

    /// The header on every page after the first: who, what, which number.
    fn short_header(&mut self) {
        let top = PAGE_HEIGHT - MARGIN;
        let right = PAGE_WIDTH - MARGIN;
        self.page.text(
            MARGIN,
            top - 12.0,
            Font::Bold,
            12.0,
            NAVY,
            &self.identity.name,
        );
        self.page
            .text_right(right, top - 12.0, Font::Bold, 14.0, NAVY, &self.sheet.title);
        if let Some((label, value)) = self.sheet.meta.first() {
            self.page.text_right(
                right,
                top - 28.0,
                Font::Regular,
                9.0,
                Color::BLACK,
                &format!("{label}: {value} (continued)"),
            );
        }
        let y = top - 38.0;
        self.page.line(MARGIN, y, right, y, 1.0, NAVY);
        self.y = y - 24.0;
    }

    fn new_page(&mut self) {
        self.finish_page();
        self.fresh_page();
        self.short_header();
    }

    /// Column x positions: (left edge, right edge) of each.
    fn column_edges(&self) -> Vec<(f32, f32)> {
        let fixed: f32 = self.sheet.columns.iter().map(|c| c.width).sum();
        let flex = Self::content_width() - fixed;
        let mut x = MARGIN;
        self.sheet
            .columns
            .iter()
            .map(|c| {
                let w = if c.width == 0.0 { flex } else { c.width };
                let edges = (x, x + w);
                x += w;
                edges
            })
            .collect()
    }

    fn table_header(&mut self) {
        let right = PAGE_WIDTH - MARGIN;
        let y = self.y;
        self.page.line(MARGIN, y + 13.0, right, y + 13.0, 1.2, NAVY);
        self.page.line(MARGIN, y - 6.0, right, y - 6.0, 1.2, NAVY);
        for (column, (x0, x1)) in self.sheet.columns.iter().zip(self.column_edges()) {
            match column.align {
                Align::Left => self
                    .page
                    .text(x0 + PAD, y, Font::Bold, 8.5, NAVY, column.label),
                Align::Right => {
                    self.page
                        .text_right(x1 - PAD, y, Font::Bold, 8.5, NAVY, column.label)
                }
            }
        }
        self.y = y - 20.0;
    }

    fn rows(&mut self) {
        let edges = self.column_edges();
        if self.sheet.rows.is_empty() {
            if let Some(text) = &self.sheet.empty_text {
                self.page
                    .text(MARGIN + PAD, self.y, Font::Oblique, BODY, GREY, text);
                self.y -= LEADING + 8.0;
            }
        }
        for row in &self.sheet.rows {
            // Each cell wraps to its column; the row is as tall as its
            // tallest cell, plus the detail line under the flexible column.
            let wrapped: Vec<Vec<String>> = row
                .cells
                .iter()
                .zip(&edges)
                .map(|(cell, (x0, x1))| pdf::wrap(Font::Regular, BODY, cell, x1 - x0 - 2.0 * PAD))
                .collect();
            let flex = self
                .sheet
                .columns
                .iter()
                .position(|c| c.width == 0.0)
                .unwrap_or(0);
            let detail_lines = row
                .detail
                .as_ref()
                .map(|d| {
                    let (x0, x1) = edges[flex];
                    pdf::wrap(Font::Oblique, 8.0, d, x1 - x0 - 2.0 * PAD)
                })
                .unwrap_or_default();
            let tallest = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);
            let flex_lines = wrapped.get(flex).map(Vec::len).unwrap_or(1);
            let lines = tallest.max(flex_lines + detail_lines.len());
            let depth = (lines - 1) as f32 * LEADING + 6.0;
            if self.y - depth < BOTTOM {
                self.new_page();
                self.table_header();
            }
            let top = self.y;
            for ((cell, column), (x0, x1)) in wrapped.iter().zip(&self.sheet.columns).zip(&edges) {
                let mut y = top;
                for piece in cell {
                    if piece.is_empty() {
                        continue;
                    }
                    match column.align {
                        Align::Left => {
                            self.page
                                .text(x0 + PAD, y, Font::Regular, BODY, Color::BLACK, piece)
                        }
                        Align::Right => self.page.text_right(
                            x1 - PAD,
                            y,
                            Font::Regular,
                            BODY,
                            Color::BLACK,
                            piece,
                        ),
                    }
                    y -= LEADING;
                }
            }
            let mut y = top - flex_lines as f32 * LEADING;
            for piece in &detail_lines {
                self.page
                    .text(edges[flex].0 + PAD, y, Font::Oblique, 8.0, GREY, piece);
                y -= LEADING;
            }
            let rule = top - depth;
            self.page
                .line(MARGIN, rule, PAGE_WIDTH - MARGIN, rule, 0.5, RULE);
            self.y = rule - LEADING - 1.0;
        }
    }

    fn totals(&mut self) {
        let height: f32 = self
            .sheet
            .totals
            .iter()
            .map(|t| if t.emphasis { 26.0 } else { 16.0 })
            .sum::<f32>()
            + 10.0;
        if self.y - height < BOTTOM {
            self.new_page();
        }
        let right = PAGE_WIDTH - MARGIN;
        let label_x = right - 118.0;
        let mut y = self.y - 6.0;
        for line in &self.sheet.totals {
            if line.emphasis {
                y -= 6.0;
                self.page
                    .line(right - 240.0, y + 14.0, right, y + 14.0, 1.0, NAVY);
                self.page
                    .text_right(label_x, y, Font::Bold, 12.0, NAVY, &line.label);
                self.page
                    .text_right(right - PAD, y, Font::Bold, 12.0, NAVY, &line.value);
                y -= 20.0;
            } else {
                self.page
                    .text_right(label_x, y, Font::Regular, BODY, Color::BLACK, &line.label);
                self.page.text_right(
                    right - PAD,
                    y,
                    Font::Regular,
                    BODY,
                    Color::BLACK,
                    &line.value,
                );
                y -= 16.0;
            }
        }
        self.y = y - 6.0;
    }

    fn pay_box(&mut self) {
        let Some(pay) = &self.sheet.pay else {
            return;
        };
        let width = 260.0;
        let inner = width - 20.0;
        let lines: Vec<String> = pay
            .lines
            .iter()
            .flat_map(|l| pdf::wrap(Font::Regular, 9.0, l, inner))
            .collect();
        let height = 24.0 + lines.len() as f32 * 12.0 + 6.0;
        if self.y - height < BOTTOM {
            self.new_page();
        }
        let x = PAGE_WIDTH - MARGIN - width;
        let top = self.y;
        self.page
            .rect_stroke(x, top - height, width, height, 1.0, NAVY);
        self.page
            .text(x + 10.0, top - 16.0, Font::Bold, 10.0, NAVY, "PAY ONLINE");
        let mut y = top - 30.0;
        for line in &lines {
            self.page
                .text(x + 10.0, y, Font::Regular, 9.0, Color::BLACK, line);
            y -= 12.0;
        }
        if let Some(uri) = &pay.uri {
            self.page.link(x, top - height, width, height, uri);
        }
        self.y = top - height - 16.0;
    }

    fn notes(&mut self) {
        if self.sheet.notes.is_empty() {
            return;
        }
        if self.y - 30.0 < BOTTOM {
            self.new_page();
        }
        self.page
            .text(MARGIN, self.y, Font::Bold, 9.0, NAVY, "NOTES");
        self.y -= 14.0;
        for note in &self.sheet.notes {
            for piece in pdf::wrap(Font::Regular, 9.0, note, Self::content_width() - 8.0) {
                if self.y < BOTTOM {
                    self.new_page();
                }
                self.page.text(
                    MARGIN + 8.0,
                    self.y,
                    Font::Regular,
                    9.0,
                    Color::BLACK,
                    &piece,
                );
                self.y -= 13.0;
            }
        }
    }

    /// Footer and page numbers, once the page count is known.
    fn footers(&mut self) {
        let count = self.doc.pages.len();
        let footer = self.identity.footer.clone();
        for (i, page) in self.doc.pages.iter_mut().enumerate() {
            let y = MARGIN - 14.0;
            if let Some(text) = &footer {
                page.text_center(PAGE_WIDTH / 2.0, y, Font::Oblique, 8.0, GREY, text);
            }
            page.text_right(
                PAGE_WIDTH - MARGIN,
                y,
                Font::Regular,
                8.0,
                GREY,
                &format!("Page {} of {count}", i + 1),
            );
        }
    }
}

/// Lays `sheet` out on as many pages as it needs. `logo` is the decoded
/// company logo, if one loaded.
pub fn layout(sheet: &Sheet, identity: &CompanyIdentity, logo: Option<pdf::Image>) -> PdfDocument {
    let title = format!("{} — {}", sheet.title, identity.name);
    let mut doc = PdfDocument::new(title);
    let logo = logo.map(|image| {
        let (w, h) = (image.width, image.height);
        (doc.add_image(image), w, h)
    });
    let mut writer = Writer {
        sheet,
        identity,
        logo,
        doc,
        page: Page::new(),
        y: 0.0,
    };
    writer.fresh_page();
    writer.full_header();
    writer.table_header();
    writer.rows();
    writer.totals();
    writer.pay_box();
    writer.notes();
    writer.finish_page();
    writer.footers();
    writer.doc
}

/// The configured logo, decoded and downsampled; `None` (and a warning)
/// when it is missing or not a PNG this module reads.
fn load_logo(identity: &CompanyIdentity, warnings: &mut Vec<String>) -> Option<pdf::Image> {
    let path = expand_home(identity.logo.as_ref()?);
    match std::fs::read(&path) {
        Ok(bytes) => match pdf::decode_png(&bytes) {
            Ok(image) => Some(image.downsampled(600)),
            Err(reason) => {
                warnings.push(format!(
                    "logo {}: {reason}; printed the name instead",
                    path.display()
                ));
                None
            }
        },
        Err(err) => {
            warnings.push(format!(
                "logo {}: {err}; printed the name instead",
                path.display()
            ));
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering to a file
// ---------------------------------------------------------------------------

/// What happened about the pay link on this render.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PayLinkOutcome {
    /// Not an open, non-voided invoice.
    NotApplicable,
    /// The caller asked for none.
    Skipped,
    /// No Authorize.net client, or no credentials or config for it.
    NotConfigured(String),
    /// The gateway or the network refused; the invoice rendered without it.
    Failed(String),
    /// A link was minted: the self-submitting page is beside the PDF.
    Attached { link: PaymentLink, page: PathBuf },
}

/// One rendered file.
#[derive(Clone, Debug)]
pub struct Rendered {
    pub path: PathBuf,
    pub page_count: usize,
    pub title: String,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct RenderedDocument {
    pub rendered: Rendered,
    pub document: LedgerDocument,
    pub figures: DocumentFigures,
    pub pay_link: PayLinkOutcome,
}

#[derive(Clone, Debug)]
pub struct RenderedStatement {
    pub rendered: Rendered,
    pub aging: AgingRow,
    pub open_items: usize,
}

/// What [`render_document`] does about the pay link.
pub enum PayLinkMode<'a> {
    /// Never ask for one.
    Off,
    /// No client is configured; an open invoice says so.
    Unconfigured,
    /// Ask this client for one when the invoice is open.
    Client(&'a mut dyn HostedPaymentClient),
}

/// A name safe on every filesystem the files might be copied to.
fn file_name(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').to_string();
    if trimmed.is_empty() {
        "document".to_string()
    } else {
        trimmed
    }
}

fn write_pdf(path: &Path, doc: &PdfDocument) -> Result<usize, RenderError> {
    std::fs::write(path, doc.to_bytes()).map_err(io_err(path))?;
    Ok(doc.pages.len())
}

/// Is this refusal "Authorize.net is not set up" rather than a failure?
fn not_configured(err: &LinkError) -> bool {
    matches!(
        err,
        LinkError::MissingCredentials { .. }
            | LinkError::Config { .. }
            | LinkError::ProductionDisabled
            | LinkError::Keychain(_)
    )
}

/// Renders one saved document to `<dir>/<Kind> <number> - <contact>.pdf`
/// (with ` (VOID)` when voided), overwriting an earlier render of it. An
/// open invoice gets a pay link per `pay` (see the module docs).
pub fn render_document(
    ledger: &Ledger,
    company: &str,
    document_id: &str,
    config: &RenderConfig,
    dir: &Path,
    pay: PayLinkMode<'_>,
    now: DateTime<Utc>,
) -> Result<RenderedDocument, RenderError> {
    let doc = books::latest_document(ledger, company, document_id)?;
    let identity = company_identity(ledger, company, config)?;
    // Built once without the box to learn the balance, then again with it.
    let (_, figures) = document_sheet(ledger, company, &doc, &identity, None)?;

    let contact = match &doc.contact {
        Some(c) => books::contact(ledger, company, &c.id)?,
        None => None,
    };
    let who = contact
        .as_ref()
        .map(|c| c.display_name.clone())
        .or_else(|| doc.contact.as_ref().map(|c| c.id.clone()))
        .unwrap_or_default();
    let number = doc
        .number
        .clone()
        .unwrap_or_else(|| doc.document_id.clone());
    let mut stem = format!("{} {number}", file_label(doc.kind));
    if !who.is_empty() {
        stem = format!("{stem} - {who}");
    }
    if doc.is_voided {
        stem.push_str(" (VOID)");
    }
    let stem = file_name(&stem);
    let path = dir.join(format!("{stem}.pdf"));

    let open_invoice =
        doc.kind == DocKind::Invoice && !doc.is_voided && figures.balance.minor() > 0;
    let pay_link = if !open_invoice {
        PayLinkOutcome::NotApplicable
    } else {
        match pay {
            PayLinkMode::Off => PayLinkOutcome::Skipped,
            PayLinkMode::Unconfigured => PayLinkOutcome::NotConfigured(
                "no Authorize.net client is configured (docs/AUTHNET.md, \"Payment link\")".into(),
            ),
            PayLinkMode::Client(client) => {
                match link::payment_link(ledger, company, document_id, client, now) {
                    Ok(minted) => {
                        let page = dir.join(format!("{stem}.pay.html"));
                        std::fs::write(&page, minted.html()).map_err(io_err(&page))?;
                        PayLinkOutcome::Attached { link: minted, page }
                    }
                    Err(err) if not_configured(&err) => {
                        PayLinkOutcome::NotConfigured(err.to_string())
                    }
                    Err(err) => PayLinkOutcome::Failed(err.to_string()),
                }
            }
        }
    };
    let pay_box = match &pay_link {
        PayLinkOutcome::Attached { link, .. } => {
            let durable = config.pay_url.as_ref().map(|template| {
                template
                    .replace("{number}", &link.invoice_number)
                    .replace("{document_id}", &link.document_id)
            });
            let mut lines = vec![format!(
                "Pay the {} balance by card on Authorize.net's secure page.",
                usd(link.amount)
            )];
            match &durable {
                Some(url) => lines.push(url.clone()),
                None => lines.push("Use the secure payment link sent with this invoice.".into()),
            }
            Some(PayBox {
                lines,
                uri: durable,
            })
        }
        _ => None,
    };

    let (sheet, figures) = document_sheet(ledger, company, &doc, &identity, pay_box)?;
    let mut warnings = Vec::new();
    let logo = load_logo(&identity, &mut warnings);
    let pdf = layout(&sheet, &identity, logo);
    let page_count = write_pdf(&path, &pdf)?;
    if let PayLinkOutcome::Failed(reason) = &pay_link {
        warnings.push(format!("pay link left off: {reason}"));
    }
    Ok(RenderedDocument {
        rendered: Rendered {
            path,
            page_count,
            title: stem,
            warnings,
        },
        document: doc,
        figures,
        pay_link,
    })
}

/// Renders a customer statement as of `as_of` to
/// `<dir>/Statement - <customer> - <as_of>.pdf`.
pub fn render_statement(
    ledger: &Ledger,
    company: &str,
    contact_id: &str,
    as_of: NaiveDate,
    config: &RenderConfig,
    dir: &Path,
) -> Result<RenderedStatement, RenderError> {
    let identity = company_identity(ledger, company, config)?;
    let (sheet, aging) = statement_sheet(ledger, company, contact_id, as_of)?;
    let stem = file_name(&format!("Statement - {} - {as_of}", aging.contact_name));
    let path = dir.join(format!("{stem}.pdf"));
    let mut warnings = Vec::new();
    let logo = load_logo(&identity, &mut warnings);
    let pdf = layout(&sheet, &identity, logo);
    let page_count = write_pdf(&path, &pdf)?;
    Ok(RenderedStatement {
        rendered: Rendered {
            path,
            page_count,
            title: stem,
            warnings,
        },
        open_items: sheet.rows.len(),
        aging,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usd_groups_thousands_and_signs() {
        assert_eq!(usd(Money::from_minor(0)), "$0.00");
        assert_eq!(usd(Money::from_minor(123_456_789)), "$1,234,567.89");
        assert_eq!(usd(Money::from_minor(-1_200)), "-$12.00");
        assert_eq!(usd(Money::from_minor(100_000)), "$1,000.00");
    }

    #[test]
    fn rates_keep_the_places_they_need() {
        assert_eq!(
            rate(Money::from_minor(15_750), Decimal::from(10)).unwrap(),
            "$15.75"
        );
        assert_eq!(
            rate(Money::from_minor(100), Decimal::from(8)).unwrap(),
            "$0.125"
        );
        assert_eq!(
            rate(Money::from_minor(5_000), Decimal::from(1)).unwrap(),
            "$50.00"
        );
        assert!(rate(Money::from_minor(5_000), Decimal::ZERO).is_none());
    }

    #[test]
    fn config_reads_documents_and_ignores_the_rest() {
        let config = RenderConfig::parse(
            "[intuit]\nclient_id = \"x\"\n[authnet]\nmerchant_name = \"A\"\n\
             [documents]\noutput_dir = \"/tmp/docs\"\n\
             [documents.company.aquamentor]\nname = \"Aquamentor\"\naddress = [\"1 Main St\"]\n",
        )
        .unwrap();
        assert_eq!(config.output_dir, Some(PathBuf::from("/tmp/docs")));
        assert_eq!(config.company["aquamentor"].address, vec!["1 Main St"]);
        assert!(RenderConfig::parse("[documents]\nsurprise = 1\n").is_err());
        assert_eq!(RenderConfig::parse("").unwrap(), RenderConfig::default());
    }

    #[test]
    fn file_names_lose_path_characters() {
        assert_eq!(file_name("Invoice 1/2 - A:B"), "Invoice 1-2 - A-B");
        assert_eq!(file_name("..."), "document");
    }

    #[test]
    fn output_inside_a_work_tree_is_refused_except_target_and_local() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        let config = RenderConfig::default();
        let err = output_dir(&config, "co", Some(&root.path().join("docs"))).unwrap_err();
        assert!(matches!(err, RenderError::InsideRepository(..)), "{err}");
        assert!(
            !root.path().join("docs").exists(),
            "refused before creating"
        );
        assert!(output_dir(&config, "co", Some(&root.path().join("target/pdfs"))).is_ok());
        assert!(output_dir(&config, "co", Some(&root.path().join(".local/pdfs"))).is_ok());
        let outside = tempfile::tempdir().unwrap();
        let dir = output_dir(&config, "co", Some(outside.path())).unwrap();
        assert!(dir.exists());
    }
}
