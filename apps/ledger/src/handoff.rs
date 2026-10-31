//! The accountant handoff pack. `LEDGER-DESIGN.md` §8 "How Joel reaches
//! it" (W24), `docs/ACCOUNTANT.md`.
//!
//! W24's proposed answer is a local install reading a dated, read-only copy
//! of the book. This module makes that copy and proves it is one:
//!
//! - [`write_pack`] takes a `VACUUM INTO` snapshot of the live file, turns
//!   it into a plain rollback-journal database (so opening it never needs a
//!   writable `-wal`/`-shm` beside it), opens it with
//!   [`Ledger::open_read_only`], and renders every CSV **from the snapshot**
//!   rather than from the live book — so the numbers in the folder and the
//!   numbers Joel gets when he opens the file cannot disagree. Then it
//!   writes `manifest.json` (every file's SHA-256, size and row count, the
//!   trial balance totals, as-of and generated-at) and `SHA256SUMS` (the same
//!   hashes in `shasum -a 256 -c` form), and marks every file `0444`.
//! - [`verify_pack`] is the other end: re-hash every file against the
//!   manifest, open the snapshot read-only, and check its trial balance
//!   still totals what the manifest says.
//! - [`open_book`] is how every read command opens a `--db`: a write-
//!   protected file goes through [`Ledger::open_read_only`], anything else
//!   through [`Ledger::open`] as before.
//!
//! The pack is deterministic for the same book and the same `--as-of`: every
//! byte apart from `generated_at` in the manifest (and so the manifest's own
//! line in `SHA256SUMS`) comes out the same twice.
//!
//! **Why read-only holds.** Three layers, any one of which is enough: the
//! file is mode `0444`; [`Ledger::open_read_only`] opens it
//! `SQLITE_OPEN_READ_ONLY` with `PRAGMA query_only` on top, so SQLite refuses
//! every write statement; and `crate::mcp`'s write gate refuses every write
//! tool up front with a sentence a person can read. `AccountantView` over a
//! snapshot therefore reads everything and proposes nothing — Joel's
//! proposals go to Dan, who enters them on the live book.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::accountant::{self, csv_field, csv_money, AccountantError, AccountantView, AuditRow};
use crate::books::{self, Aging, Book, BooksError};
use crate::chart::Classification;
use crate::report::{self, SalesTaxLines, TrialBalance};
use crate::store::{latest_version, Ledger, LedgerError};
use crate::types::PostingConfig;
use ledger_core::Money;

/// `manifest.json`'s `format`; bump it when the pack's shape changes.
pub const PACK_FORMAT: &str = "ledger-accountant-pack/1";
pub const SNAPSHOT_FILE: &str = "book.sqlite";
pub const MANIFEST_FILE: &str = "manifest.json";
pub const SUMS_FILE: &str = "SHA256SUMS";

#[derive(Debug, Error)]
pub enum HandoffError {
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error(transparent)]
    Accountant(#[from] AccountantError),
    #[error(transparent)]
    Books(#[from] BooksError),
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("manifest: {0}")]
    Json(#[from] serde_json::Error),
    #[error(
        "{0} already exists; a pack is never written over — move it aside or pick another --out"
    )]
    Exists(PathBuf),
    #[error("snapshot trial balance ({snapshot}) differs from the live book's ({live}); was something posted while the pack was being made?")]
    SnapshotDiffers { snapshot: String, live: String },
    #[error("snapshot integrity check failed: {0}")]
    Integrity(String),
    #[error("{path}: {problem}")]
    Tampered { path: String, problem: String },
    #[error("manifest format {0:?} is not {PACK_FORMAT:?}")]
    Format(String),
}

// ---------------------------------------------------------------------------
// The manifest
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub format: String,
    pub company: String,
    pub as_of: NaiveDate,
    /// The one field that differs between two packs of the same book.
    pub generated_at: DateTime<Utc>,
    pub schema_version: i64,
    pub snapshot: String,
    pub sales_tax_rate: String,
    pub trial_balance: TbTotals,
    /// Every row count in the snapshot, by table.
    pub snapshot_tables: BTreeMap<String, u64>,
    /// Every file in the pack except `manifest.json` and `SHA256SUMS`, by path.
    pub files: Vec<ManifestFile>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TbTotals {
    pub total_debits: String,
    pub total_credits: String,
    pub balanced: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestFile {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
    /// CSV lines after the header (totals and summary lines included);
    /// `None` for the snapshot, whose rows are in `snapshot_tables`.
    pub rows: Option<u64>,
}

impl TbTotals {
    fn of(tb: &TrialBalance) -> Self {
        TbTotals {
            total_debits: csv_money(tb.total_debits),
            total_credits: csv_money(tb.total_credits),
            balanced: tb.total_debits == tb.total_credits,
        }
    }
}

/// What [`write_pack`] made.
#[derive(Clone, Debug)]
pub struct Pack {
    pub dir: PathBuf,
    pub manifest: Manifest,
}

// ---------------------------------------------------------------------------
// Opening a book
// ---------------------------------------------------------------------------

/// Opens `path` the way the read commands should: read-only when the file is
/// write-protected (a handoff snapshot, `0444`), the ordinary read-write
/// [`Ledger::open`] otherwise.
pub fn open_book(path: &Path) -> Result<Ledger, LedgerError> {
    let protected = std::fs::metadata(path)
        .map(|m| m.permissions().readonly())
        .unwrap_or(false);
    if protected {
        Ledger::open_read_only(path)
    } else {
        Ledger::open(path)
    }
}

/// The snapshot inside a pack directory, opened read-only.
pub fn open_snapshot(pack_dir: &Path) -> Result<Ledger, LedgerError> {
    Ledger::open_read_only(pack_dir.join(SNAPSHOT_FILE))
}

// ---------------------------------------------------------------------------
// Writing a pack
// ---------------------------------------------------------------------------

/// The folder [`write_pack`] creates under `out`: `<company>-<as_of>`.
pub fn pack_dir(out: &Path, company: &str, as_of: NaiveDate) -> PathBuf {
    out.join(format!("{company}-{as_of}"))
}

/// Writes the handoff pack for `company` as of `as_of` into
/// `<out>/<company>-<as_of>/`, which must not already exist. See the module
/// doc for what goes in it and why it is built from the snapshot.
pub fn write_pack(
    live: &Ledger,
    company: &str,
    as_of: NaiveDate,
    out: &Path,
    generated_at: DateTime<Utc>,
) -> Result<Pack, HandoffError> {
    let dir = pack_dir(out, company, as_of);
    if dir.exists() {
        return Err(HandoffError::Exists(dir));
    }
    std::fs::create_dir_all(&dir)?;

    // 1. The snapshot: a compacted, consistent copy, then out of WAL mode so
    // a reader never needs to create -wal/-shm files next to it.
    let snapshot_path = dir.join(SNAPSHOT_FILE);
    live.conn()
        .execute("VACUUM INTO ?1", [snapshot_path.to_string_lossy().as_ref()])?;
    {
        let conn = rusqlite::Connection::open(&snapshot_path)?;
        let _: String = conn.query_row("PRAGMA journal_mode = DELETE", [], |row| row.get(0))?;
    }
    set_read_only(&snapshot_path)?;

    // 2. Open it the way Joel will, and check it is the book we meant.
    let snapshot = Ledger::open_read_only(&snapshot_path)?;
    let integrity: String = snapshot
        .conn()
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(HandoffError::Integrity(integrity));
    }
    let tb = report::trial_balance(&snapshot, company, as_of)?;
    let live_tb = report::trial_balance(live, company, as_of)?;
    if tb != live_tb {
        return Err(HandoffError::SnapshotDiffers {
            snapshot: totals_text(&tb),
            live: totals_text(&live_tb),
        });
    }

    // 3. Every CSV, from the snapshot, through the accountant's own view.
    let view = AccountantView::new(&snapshot, company);
    let fy = as_of.year();
    let year_start =
        NaiveDate::from_ymd_opt(fy, 1, 1).ok_or_else(|| LedgerError::BadDate(as_of.to_string()))?;
    let rate = PostingConfig::default().sales_tax_rate;

    accountant::export_pack(&snapshot, company, as_of, &dir)?;
    write_file(
        &dir,
        &format!("pnl-by-class-{fy}.csv"),
        &pnl_by_class_csv(&view, &tb, year_start, as_of)?,
    )?;
    write_file(
        &dir,
        "audit-trail.csv",
        &audit_trail_csv(&view.audit_trail(None)?),
    )?;
    write_file(
        &dir,
        "adjustments.csv",
        &adjustments_csv(&view.list_adjustments(None)?),
    )?;
    for (book, name) in [(Book::Receivable, "ar"), (Book::Payable, "ap")] {
        let aging = books::aging(&snapshot, company, book, as_of)?;
        write_file(
            &dir,
            &format!("{name}-aging-{as_of}.csv"),
            &aging_csv(&aging),
        )?;
    }
    let mut quarters = Vec::new();
    for q in 1..=4u32 {
        // A quarter still running on `as_of` would pick up postings after
        // it, so only quarters that have ended go in.
        if quarter_end(fy, q) <= as_of {
            quarters.push((q, view.sales_tax_lines((fy, q), rate)?));
        }
    }
    write_file(
        &dir,
        &format!("sales-tax-{fy}.csv"),
        &sales_tax_csv(fy, &quarters),
    )?;

    // 4. The manifest, then the sums, then lock everything.
    let mut files = Vec::new();
    let mut names: Vec<String> = std::fs::read_dir(&dir)?
        .map(|entry| entry.map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect::<Result<_, _>>()?;
    names.sort();
    for name in names {
        let bytes = std::fs::read(dir.join(&name))?;
        let rows = name.ends_with(".csv").then(|| csv_rows(&bytes));
        files.push(ManifestFile {
            sha256: sha256_hex(&bytes),
            bytes: bytes.len() as u64,
            path: name,
            rows,
        });
    }
    let manifest = Manifest {
        format: PACK_FORMAT.to_string(),
        company: company.to_string(),
        as_of,
        generated_at,
        schema_version: latest_version(),
        snapshot: SNAPSHOT_FILE.to_string(),
        sales_tax_rate: rate.to_string(),
        trial_balance: TbTotals::of(&tb),
        snapshot_tables: table_counts(&snapshot)?,
        files,
    };
    drop(snapshot);

    let manifest_json = serde_json::to_string_pretty(&manifest)? + "\n";
    write_file(&dir, MANIFEST_FILE, &manifest_json)?;
    let mut sums = String::new();
    let mut lines: Vec<(String, String)> = manifest
        .files
        .iter()
        .map(|f| (f.path.clone(), f.sha256.clone()))
        .collect();
    lines.push((
        MANIFEST_FILE.to_string(),
        sha256_hex(manifest_json.as_bytes()),
    ));
    lines.sort();
    for (path, hash) in lines {
        let _ = writeln!(sums, "{hash}  {path}");
    }
    write_file(&dir, SUMS_FILE, &sums)?;

    for entry in std::fs::read_dir(&dir)? {
        set_read_only(&entry?.path())?;
    }

    Ok(Pack { dir, manifest })
}

// ---------------------------------------------------------------------------
// Verifying a pack (Joel's end)
// ---------------------------------------------------------------------------

/// Re-hashes every file in `dir` against its manifest, opens the snapshot
/// read-only, and checks the trial balance as of the manifest's date still
/// totals what the manifest recorded. Returns the manifest and the open,
/// read-only snapshot.
pub fn verify_pack(dir: &Path) -> Result<(Manifest, Ledger), HandoffError> {
    let manifest: Manifest = serde_json::from_slice(&std::fs::read(dir.join(MANIFEST_FILE))?)?;
    if manifest.format != PACK_FORMAT {
        return Err(HandoffError::Format(manifest.format));
    }
    for file in &manifest.files {
        let bytes = std::fs::read(dir.join(&file.path)).map_err(|e| HandoffError::Tampered {
            path: file.path.clone(),
            problem: e.to_string(),
        })?;
        let hash = sha256_hex(&bytes);
        if hash != file.sha256 {
            return Err(HandoffError::Tampered {
                path: file.path.clone(),
                problem: format!("sha256 {hash}, manifest says {}", file.sha256),
            });
        }
    }
    let snapshot = open_snapshot(dir)?;
    let tb = report::trial_balance(&snapshot, &manifest.company, manifest.as_of)?;
    if TbTotals::of(&tb) != manifest.trial_balance {
        return Err(HandoffError::Tampered {
            path: manifest.snapshot.clone(),
            problem: format!(
                "trial balance totals {}, manifest says {} / {}",
                totals_text(&tb),
                manifest.trial_balance.total_debits,
                manifest.trial_balance.total_credits
            ),
        });
    }
    Ok((manifest, snapshot))
}

// ---------------------------------------------------------------------------
// The CSVs this module adds to `accountant::export_pack`'s five
// ---------------------------------------------------------------------------

/// Income, COGS and expense for the year to date by account and class, on
/// each section's normal side (the same signs as `report::profit_and_loss`),
/// from the GL detail so every line carries its own class.
fn pnl_by_class_csv(
    view: &AccountantView<'_>,
    tb: &TrialBalance,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<String, HandoffError> {
    let classification: BTreeMap<&str, Classification> = tb
        .rows
        .iter()
        .map(|row| (row.account_id.0.as_str(), row.classification))
        .collect();
    let gl = view.general_ledger(from, to, None)?;
    // (section order, account number, class) -> (section, name, amount)
    let mut cells: BTreeMap<(u8, String, String), (&'static str, String, Money)> = BTreeMap::new();
    for section in &gl.sections {
        let (order, label, credit_normal) = match classification.get(section.account_id.0.as_str())
        {
            Some(Classification::Income) => (0, "income", true),
            Some(Classification::Cogs) => (1, "cogs", false),
            Some(Classification::Expense) => (2, "expense", false),
            _ => continue,
        };
        for line in &section.lines {
            let amount = if credit_normal {
                line.credit.checked_sub(line.debit)
            } else {
                line.debit.checked_sub(line.credit)
            }
            .map_err(LedgerError::from)?;
            let class = line.class.as_ref().map(|c| c.0.clone()).unwrap_or_default();
            let cell = cells
                .entry((order, section.number.clone(), class))
                .or_insert((label, section.name.clone(), Money::ZERO));
            cell.2 = cell.2.checked_add(amount).map_err(LedgerError::from)?;
        }
    }
    let mut out = String::from("section,account_number,account_name,class,amount\n");
    for ((_, number, class), (label, name, amount)) in cells {
        let _ = writeln!(
            out,
            "{label},{},{},{},{}",
            csv_field(&number),
            csv_field(&name),
            csv_field(&class),
            csv_money(amount)
        );
    }
    Ok(out)
}

fn audit_trail_csv(rows: &[AuditRow]) -> String {
    let mut out = String::from(
        "entry_id,entry_date,source_type,source_id,source_version,memo,is_flagged,\
         reversal_of,actor,command_kind,at\n",
    );
    for row in rows {
        let _ = writeln!(
            out,
            "{},{},{},{},{},{},{},{},{},{},{}",
            row.entry_id,
            row.entry_date,
            row.source_type.as_str(),
            csv_field(&row.source_id),
            row.source_version,
            csv_field(row.memo.as_deref().unwrap_or("")),
            row.is_flagged,
            row.reversal_of.as_deref().unwrap_or(""),
            csv_field(&row.actor),
            csv_field(&row.command_kind),
            row.at.to_rfc3339(),
        );
    }
    out
}

/// One line per proposed journal line, so a request's lines read as the
/// entry they would post.
fn adjustments_csv(requests: &[accountant::AdjustmentRequest]) -> String {
    let mut out = String::from(
        "request_id,state,requested_by,requested_at,description,period_closed,decided_by,\
         decided_at,decision_note,posted_entry_id,line_no,account,class,debit,credit,memo\n",
    );
    for request in requests {
        for line in &request.lines {
            let _ = writeln!(
                out,
                "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
                request.request_id,
                request.state.as_str(),
                csv_field(&request.requested_by),
                request.requested_at.to_rfc3339(),
                csv_field(&request.description),
                request.period_closed,
                csv_field(request.decided_by.as_deref().unwrap_or("")),
                request
                    .decided_at
                    .map(|at| at.to_rfc3339())
                    .unwrap_or_default(),
                csv_field(request.decision_note.as_deref().unwrap_or("")),
                request.posted_entry_id.as_deref().unwrap_or(""),
                line.line_no,
                csv_field(&line.account.0),
                line.class.as_ref().map(|c| c.0.as_str()).unwrap_or(""),
                csv_money(line.debit),
                csv_money(line.credit),
                csv_field(line.memo.as_deref().unwrap_or("")),
            );
        }
    }
    out
}

/// By contact, QBO's buckets, then a `TOTAL` line. The total ties to 1200
/// (AR) or to 2000 (AP, as a positive number) on the trial balance.
fn aging_csv(aging: &Aging) -> String {
    let mut out = String::from(
        "contact_id,contact_name,current,days_1_30,days_31_60,days_61_90,over_90,total\n",
    );
    for row in aging.rows.iter().chain(std::iter::once(&aging.totals)) {
        let id = if std::ptr::eq(row, &aging.totals) {
            "TOTAL"
        } else {
            row.contact_id.as_str()
        };
        let _ = writeln!(
            out,
            "{},{},{},{},{},{},{},{}",
            csv_field(id),
            csv_field(&row.contact_name),
            csv_money(row.current),
            csv_money(row.days_1_30),
            csv_money(row.days_31_60),
            csv_money(row.days_61_90),
            csv_money(row.over_90),
            csv_money(row.total),
        );
    }
    out
}

/// §9's lines per quarter: A total income, B tax collected, C taxable sales
/// (B over the rate), D non-taxable (A minus C), and the line-level check E
/// with its variance from C.
fn sales_tax_csv(year: i32, quarters: &[(u32, SalesTaxLines)]) -> String {
    let mut out = String::from(
        "quarter,a_total_income,b_tax_collected,c_taxable_sales,d_nontaxable_sales,\
         e_line_level_taxable,variance_e_minus_c\n",
    );
    for (q, lines) in quarters {
        let _ = writeln!(
            out,
            "{year}-Q{q},{},{},{},{},{},{}",
            csv_money(lines.a_total_income),
            csv_money(lines.b_tax_collected),
            csv_money(lines.c_taxable_sales),
            csv_money(lines.d_nontaxable_sales),
            lines
                .e_line_level_taxable
                .map(csv_money)
                .unwrap_or_default(),
            lines.variance.map(csv_money).unwrap_or_default(),
        );
    }
    out
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn quarter_end(year: i32, q: u32) -> NaiveDate {
    let (y, m) = if q == 4 {
        (year + 1, 1)
    } else {
        (year, q * 3 + 1)
    };
    NaiveDate::from_ymd_opt(y, m, 1)
        .and_then(|d| d.pred_opt())
        .expect("a quarter end is a valid date")
}

fn totals_text(tb: &TrialBalance) -> String {
    format!(
        "{} / {}",
        csv_money(tb.total_debits),
        csv_money(tb.total_credits)
    )
}

fn write_file(dir: &Path, name: &str, content: &str) -> Result<(), HandoffError> {
    std::fs::write(dir.join(name), content)?;
    Ok(())
}

fn set_read_only(path: &Path) -> Result<(), HandoffError> {
    let mut permissions = std::fs::metadata(path)?.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o444);
    }
    #[cfg(not(unix))]
    permissions.set_readonly(true);
    std::fs::set_permissions(path, permissions)?;
    Ok(())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

fn csv_rows(bytes: &[u8]) -> u64 {
    let lines = bytes.iter().filter(|&&b| b == b'\n').count() as u64;
    lines.saturating_sub(1)
}

fn table_counts(ledger: &Ledger) -> Result<BTreeMap<String, u64>, HandoffError> {
    let conn = ledger.conn();
    let mut stmt = conn.prepare(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
         ORDER BY name",
    )?;
    let names: Vec<String> = stmt
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    let mut counts = BTreeMap::new();
    for name in names {
        let count: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM \"{}\"", name.replace('"', "\"\"")),
            [],
            |row| row.get(0),
        )?;
        counts.insert(name, count as u64);
    }
    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chart;
    use crate::mcp;
    use chrono::TimeZone;
    use serde_json::{json, Value};

    const COMPANY: &str = "testco";

    fn date(raw: &str) -> NaiveDate {
        NaiveDate::parse_from_str(raw, "%Y-%m-%d").unwrap()
    }

    fn call_tool(server: &mut mcp::Server<'_>, name: &str, arguments: Value) -> Value {
        let line = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": name, "arguments": arguments },
        })
        .to_string();
        serde_json::from_str(&server.handle_line(&line).expect("a response")).unwrap()
    }

    fn ok(server: &mut mcp::Server<'_>, name: &str, arguments: Value) -> Value {
        let response = call_tool(server, name, arguments);
        assert_ne!(
            response["result"]["isError"],
            json!(true),
            "{name}: {response}"
        );
        response["result"]["structuredContent"].clone()
    }

    /// A file-backed book for 2026: two customers with open invoices of
    /// different ages (one part paid), a vendor with two open bills (one part
    /// paid), a close through Q3, and one pending adjustment request.
    fn synthetic_book(dir: &Path) -> Ledger {
        let ledger = Ledger::open(dir.join("live.sqlite")).unwrap();
        ledger
            .create_company(COMPANY, "Test Co", None, Utc::now())
            .unwrap();
        {
            let mut server = mcp::new_server(&ledger, COMPANY.to_string());
            let s = &mut server;
            for contact in [
                json!({ "contact_id": "c-pool", "kind": "Customer", "display_name": "Pool Co",
                        "terms_days": 30, "default_class": "foam" }),
                json!({ "contact_id": "c-swim", "kind": "Customer", "display_name": "Swim, Club",
                        "terms_days": 30, "default_class": "foam" }),
                json!({ "contact_id": "v-foam", "kind": "Vendor", "display_name": "Foam Supplier" }),
            ] {
                ok(s, "upsert_contact", json!({ "contact": contact }));
            }
            ok(
                s,
                "upsert_item",
                json!({ "item": {
                    "item_id": "mat", "sku": "MAT-1", "name": "Swim mat",
                    "item_type": "non_inventory", "income_account": "4100",
                    "expense_account": "6100", "default_class": "foam", "sales_price": "25.00"
                }}),
            );
            let invoice =
                |s: &mut mcp::Server<'_>, customer: &str, on: &str, amount: &str, taxable: bool| {
                    ok(
                        s,
                        "create_document",
                        json!({ "actor": "dan", "document": {
                            "kind": "Invoice", "txn_date": on,
                            "contact": { "kind": "Customer", "id": customer },
                            "lines": [{ "kind": "Item", "item_id": "mat", "amount": amount,
                                        "is_taxable": taxable }]
                        }}),
                    )["document_id"]
                        .as_str()
                        .unwrap()
                        .to_string()
                };
            let first = invoice(s, "c-pool", "2026-08-01", "100.00", true);
            invoice(s, "c-swim", "2026-11-20", "200.00", true);
            invoice(s, "c-pool", "2026-12-15", "50.00", false);
            ok(
                s,
                "create_document",
                json!({ "actor": "dan", "document": {
                    "kind": "Payment", "txn_date": "2026-09-05",
                    "contact": { "kind": "Customer", "id": "c-pool" },
                    "applications": [{ "target_document_id": first,
                                       "target_kind": "Invoice", "amount": "50.00" }]
                }}),
            );
            let bill = |s: &mut mcp::Server<'_>, number: &str, on: &str, amount: &str| {
                ok(
                    s,
                    "create_document",
                    json!({ "actor": "dan", "document": {
                        "kind": "Bill", "txn_date": on, "number": number,
                        "contact": { "kind": "Vendor", "id": "v-foam" },
                        "lines": [{ "kind": "Account", "account": "6100", "amount": amount,
                                    "class": "foam" }]
                    }}),
                )["document_id"]
                    .as_str()
                    .unwrap()
                    .to_string()
            };
            let first_bill = bill(s, "FS-1", "2026-07-10", "400.00");
            bill(s, "FS-2", "2026-12-10", "120.00");
            ok(
                s,
                "create_document",
                json!({ "actor": "dan", "document": {
                    "kind": "BillPayment", "txn_date": "2026-08-20",
                    "contact": { "kind": "Vendor", "id": "v-foam" },
                    "pay_from": chart::CHECKING,
                    "applications": [{ "target_document_id": first_bill,
                                       "target_kind": "Bill", "amount": "150.00" }]
                }}),
            );
            ok(
                s,
                "close_period",
                json!({ "period_end": "2026-09-30", "actor": "dan", "note": "Q3" }),
            );
            ok(
                s,
                "propose_adjustment",
                json!({
                    "requested_by": "joel",
                    "description": "move supplies to foam",
                    "lines": [
                        { "account": chart::SHOP_SUPPLIES, "class": "foam", "debit": "25.00" },
                        { "account": chart::CHECKING, "credit": "25.00" },
                    ],
                }),
            );
        }
        ledger
    }

    fn generated(n: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2027, 1, 15, 9, n, 0).unwrap()
    }

    fn csv_value(dir: &Path, file: &str, key_col: usize, key: &str, col: usize) -> String {
        let text = std::fs::read_to_string(dir.join(file)).unwrap();
        text.lines()
            .find(|line| line.split(',').nth(key_col) == Some(key))
            .unwrap_or_else(|| panic!("{file} has no row {key:?}:\n{text}"))
            .split(',')
            .nth(col)
            .unwrap()
            .to_string()
    }

    #[test]
    fn manifest_hashes_match_and_the_pack_is_deterministic() {
        let tmp = tempfile::tempdir().unwrap();
        let live = synthetic_book(tmp.path());
        let as_of = date("2026-12-31");

        let a = write_pack(&live, COMPANY, as_of, &tmp.path().join("a"), generated(0)).unwrap();
        let b = write_pack(&live, COMPANY, as_of, &tmp.path().join("b"), generated(5)).unwrap();
        assert_eq!(a.dir.file_name().unwrap(), "testco-2026-12-31");

        // Every file's hash and size match what the manifest on disk says.
        let on_disk: Manifest =
            serde_json::from_slice(&std::fs::read(a.dir.join(MANIFEST_FILE)).unwrap()).unwrap();
        assert_eq!(on_disk, a.manifest);
        for file in &on_disk.files {
            let bytes = std::fs::read(a.dir.join(&file.path)).unwrap();
            assert_eq!(sha256_hex(&bytes), file.sha256, "{}", file.path);
            assert_eq!(bytes.len() as u64, file.bytes, "{}", file.path);
        }
        // SHA256SUMS covers every file, the manifest included.
        let sums = std::fs::read_to_string(a.dir.join(SUMS_FILE)).unwrap();
        assert_eq!(sums.lines().count(), on_disk.files.len() + 1);
        for line in sums.lines() {
            let (hash, path) = line.split_once("  ").unwrap();
            assert_eq!(sha256_hex(&std::fs::read(a.dir.join(path)).unwrap()), hash);
        }

        let names: Vec<&str> = on_disk.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            names,
            [
                "adjustments.csv",
                "ap-aging-2026-12-31.csv",
                "ar-aging-2026-12-31.csv",
                "audit-trail.csv",
                "book.sqlite",
                "bs-2026-12-31.csv",
                "close-history.csv",
                "gl-2026.csv",
                "pnl-2026.csv",
                "pnl-by-class-2026.csv",
                "sales-tax-2026.csv",
                "tb-2026-12-31.csv",
            ]
        );
        assert_eq!(
            std::fs::read_to_string(a.dir.join("sales-tax-2026.csv"))
                .unwrap()
                .lines()
                .count(),
            5,
            "a header and four quarters"
        );
        assert!(on_disk.trial_balance.balanced);
        assert!(on_disk.snapshot_tables["journal_entries"] > 0);

        // Same book, same date: only generated_at differs.
        let mut b_manifest = b.manifest.clone();
        b_manifest.generated_at = a.manifest.generated_at;
        assert_eq!(b_manifest, a.manifest);
        assert_ne!(b.manifest.generated_at, a.manifest.generated_at);

        // Every file is write-protected, and the pack verifies.
        #[cfg(unix)]
        for entry in std::fs::read_dir(&a.dir).unwrap() {
            use std::os::unix::fs::PermissionsExt;
            let mode = entry.unwrap().metadata().unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o444);
        }
        let (verified, snapshot) = verify_pack(&a.dir).unwrap();
        assert_eq!(verified, a.manifest);
        assert!(snapshot.is_read_only());

        // A pack is never written over.
        assert!(matches!(
            write_pack(&live, COMPANY, as_of, &tmp.path().join("a"), generated(9)),
            Err(HandoffError::Exists(_))
        ));
    }

    #[test]
    fn aging_ties_to_1200_and_2000_on_the_trial_balance() {
        let tmp = tempfile::tempdir().unwrap();
        let live = synthetic_book(tmp.path());
        let as_of = date("2026-12-31");
        let pack = write_pack(&live, COMPANY, as_of, tmp.path(), generated(0)).unwrap();
        let dir = &pack.dir;

        let tb = "tb-2026-12-31.csv";
        let ar_tb = csv_value(dir, tb, 0, chart::ACCOUNTS_RECEIVABLE, 5);
        let ap_tb = csv_value(dir, tb, 0, chart::ACCOUNTS_PAYABLE, 5);
        let ar_aging = csv_value(dir, "ar-aging-2026-12-31.csv", 0, "TOTAL", 7);
        let ap_aging = csv_value(dir, "ap-aging-2026-12-31.csv", 0, "TOTAL", 7);

        // 100 + 6.63 tax (half up, W17) - 50 paid, 200 + 13.25 tax, 50 untaxed.
        assert_eq!(ar_tb, "319.88");
        assert_eq!(ar_aging, ar_tb);
        // 400 - 150 paid, 120; AP is a credit balance on the TB.
        assert_eq!(ap_tb, "-370.00");
        assert_eq!(format!("-{ap_aging}"), ap_tb);

        // Buckets: the August invoice is 122 days past due as of year end.
        assert_eq!(
            csv_value(dir, "ar-aging-2026-12-31.csv", 0, "c-pool", 6),
            "56.63"
        );
        // A contact name with a comma is quoted, not split.
        assert!(std::fs::read_to_string(dir.join("ar-aging-2026-12-31.csv"))
            .unwrap()
            .contains("c-swim,\"Swim, Club\","));
    }

    #[test]
    fn pnl_by_class_sums_to_the_pnl() {
        let tmp = tempfile::tempdir().unwrap();
        let live = synthetic_book(tmp.path());
        let pack =
            write_pack(&live, COMPANY, date("2026-12-31"), tmp.path(), generated(0)).unwrap();
        let text = std::fs::read_to_string(pack.dir.join("pnl-by-class-2026.csv")).unwrap();
        let mut income = 0i64;
        let mut expense = 0i64;
        for line in text.lines().skip(1) {
            let cols: Vec<&str> = line.split(',').collect();
            let cents = (cols[4].parse::<f64>().unwrap() * 100.0).round() as i64;
            match cols[0] {
                "income" => income += cents,
                "expense" => expense += cents,
                _ => {}
            }
            assert_eq!(cols[3], "foam", "{line}");
        }
        let pnl = report::profit_and_loss(&live, COMPANY, date("2026-01-01"), date("2026-12-31"))
            .unwrap();
        assert_eq!(income, pnl.income.total.minor());
        assert_eq!(expense, pnl.expense.total.minor());
    }

    #[test]
    fn every_write_is_refused_against_a_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let live = synthetic_book(tmp.path());
        let as_of = date("2026-12-31");
        let pack = write_pack(&live, COMPANY, as_of, tmp.path(), generated(0)).unwrap();
        let snapshot = open_snapshot(&pack.dir).unwrap();
        assert!(snapshot.is_read_only());
        // open_book takes the read-only path for a write-protected file too.
        assert!(open_book(&pack.dir.join(SNAPSHOT_FILE))
            .unwrap()
            .is_read_only());
        assert!(!live.is_read_only());

        let reads = [
            "trial_balance",
            "profit_and_loss",
            "balance_sheet",
            "sales_tax_lines",
            "general_ledger",
            "audit_trail",
            "entries_for_document",
            "list_adjustments",
            "bank_status",
            "locked_through",
            "list_contacts",
            "list_items",
            "open_documents",
            "open_sales_orders",
            "aging",
            "get_document",
            "open_sales_orders",
            // Write PDFs to disk, never to the book.
            "render_document",
            "render_statement",
        ];
        let writes: Vec<String> = mcp::tools()
            .into_iter()
            .map(|t| t.name.to_string())
            .filter(|name| !reads.contains(&name.as_str()))
            .collect();
        // If a tool is added, it must be classified here deliberately.
        assert_eq!(writes.len(), 16, "{writes:?}");

        let before = std::fs::read(pack.dir.join(SNAPSHOT_FILE)).unwrap();
        let mut server = mcp::new_server(&snapshot, COMPANY.to_string());
        for name in &writes {
            let response = call_tool(&mut server, name, json!({}));
            assert_eq!(
                response["result"]["isError"],
                json!(true),
                "{name}: {response}"
            );
            let text = response["result"]["content"][0]["text"].as_str().unwrap();
            assert!(
                text.contains("read-only accountant snapshot"),
                "{name}: {text}"
            );
        }
        // Reads still work, through the same server.
        let tb = ok(
            &mut server,
            "trial_balance",
            json!({ "as_of": "2026-12-31" }),
        );
        assert!(!tb["rows"].as_array().unwrap().is_empty());
        drop(server);

        // Below the MCP gate, SQLite itself refuses.
        let view = AccountantView::new(&snapshot, COMPANY);
        let lines = view.list_adjustments(None).unwrap()[0].lines.clone();
        assert!(view
            .propose_adjustment("joel", "again", lines.clone(), Utc::now())
            .is_err());
        let entry = crate::types::JournalEntry {
            entry_date: as_of,
            memo: None,
            source_type: crate::types::DocKind::JournalEntry,
            source_id: "x".into(),
            source_version: 1,
            reversal_of: None,
            is_flagged: true,
            lines,
        };
        assert!(snapshot.post_entry(COMPANY, &entry, Utc::now()).is_err());
        assert!(snapshot
            .close_period(COMPANY, as_of, "joel", "", Utc::now())
            .is_err());
        assert!(snapshot
            .create_company("other", "Other", None, Utc::now())
            .is_err());
        assert!(snapshot
            .conn()
            .execute_batch("CREATE TEMP TABLE t (x)")
            .is_err());

        drop(snapshot);
        assert_eq!(std::fs::read(pack.dir.join(SNAPSHOT_FILE)).unwrap(), before);
        verify_pack(&pack.dir).unwrap();
    }

    #[test]
    fn verify_refuses_an_edited_file() {
        let tmp = tempfile::tempdir().unwrap();
        let live = synthetic_book(tmp.path());
        let pack =
            write_pack(&live, COMPANY, date("2026-12-31"), tmp.path(), generated(0)).unwrap();
        let path = pack.dir.join("tb-2026-12-31.csv");
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        std::fs::set_permissions(&path, permissions).unwrap();
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("9999,plug,Asset,1.00,0.00,1.00\n");
        std::fs::write(&path, text).unwrap();
        match verify_pack(&pack.dir) {
            Err(HandoffError::Tampered { path, .. }) => assert_eq!(path, "tb-2026-12-31.csv"),
            Err(other) => panic!("expected Tampered, got {other:?}"),
            Ok(_) => panic!("an edited pack verified"),
        }
    }
}
