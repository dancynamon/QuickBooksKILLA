//! `ledger nightly` — the §E parallel run, made mechanical.
//! `LEDGER-DESIGN.md` §7, `ROADMAP.md` §E.
//!
//! Every night, for each entity: import the replica's whole history through
//! the same posting function everything else uses
//! ([`crate::pipeline::import_replica`]), diff the resulting trial balance
//! against a QBO trial balance CSV (the Mac's cron produces that CSV with
//! `qbo-local report --live`), and write both the fixed-width text and the
//! CSV the design doc calls "kept forever" (§7). A must-tier disagreement or
//! an import rejection fails the run; anything else is printed and forgotten
//! about until tomorrow night.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDate, Utc};
use thiserror::Error;

use qbo_local::domain::RealmId;
use qbo_local::store::Store;

use crate::import::ImportOptions;
use crate::pipeline::{self, PipelineError, PipelineReport};
use crate::report::{self, QboTbRow, TbDiff, TierRules};
use crate::store::{Ledger, LedgerError};

#[derive(Debug, Error)]
pub enum NightlyError {
    #[error("import: {0}")]
    Pipeline(#[from] PipelineError),
    #[error("store: {0}")]
    Store(#[from] LedgerError),
    #[error("io error writing {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// What one `ledger nightly` run produced.
pub struct NightlyOutcome {
    pub import: PipelineReport,
    pub diff: TbDiff,
    pub text_path: PathBuf,
    pub csv_path: PathBuf,
}

impl NightlyOutcome {
    /// §7: "the run exits non-zero" on any must-tier failure; the same is
    /// true of an import rejection, since a document the pipeline could not
    /// post is a books discrepancy the diff below has no way to see.
    pub fn passed(&self) -> bool {
        self.diff.must_failures.is_empty() && self.import.rejected.is_empty()
    }

    /// A short line naming what failed, for the CLI to print alongside the
    /// full report — the design's "the failure count lands in the app chrome
    /// next to the sync state" restated as one line of terminal output.
    pub fn summary_line(&self) -> String {
        if self.passed() {
            return "nightly: green".to_string();
        }
        format!(
            "nightly: RED — {} must-tier failure(s), {} rejected document(s)",
            self.diff.must_failures.len(),
            self.import.rejected.len(),
        )
    }
}

/// The §E run over an already-open `ledger` and `replica`: import every
/// document (unbounded — the parallel run posts the whole book every night,
/// not a window of it), diff the resulting trial balance as of `as_of`
/// against `qbo_rows`, and write `<out_dir>/tbdiff-<as_of>.txt` and `.csv`.
#[allow(clippy::too_many_arguments)]
pub fn run_nightly(
    ledger: &Ledger,
    replica: &Store,
    realm: &RealmId,
    company: &str,
    qbo_rows: &[QboTbRow],
    as_of: NaiveDate,
    out_dir: &Path,
    now: DateTime<Utc>,
) -> Result<NightlyOutcome, NightlyError> {
    let import = pipeline::import_replica(
        ledger,
        replica,
        realm,
        company,
        &ImportOptions::default(),
        now,
    )?;

    let tb = report::trial_balance(ledger, company, as_of)?;
    let diff = report::tb_diff(&tb, qbo_rows, &TierRules::default())?;

    fs::create_dir_all(out_dir).map_err(|source| NightlyError::Io {
        path: out_dir.to_path_buf(),
        source,
    })?;
    let text_path = out_dir.join(format!("tbdiff-{as_of}.txt"));
    let csv_path = out_dir.join(format!("tbdiff-{as_of}.csv"));
    fs::write(&text_path, diff.render_text(company, as_of)).map_err(|source| NightlyError::Io {
        path: text_path.clone(),
        source,
    })?;
    fs::write(&csv_path, diff.render_csv()).map_err(|source| NightlyError::Io {
        path: csv_path.clone(),
        source,
    })?;

    Ok(NightlyOutcome {
        import,
        diff,
        text_path,
        csv_path,
    })
}

// ---------------------------------------------------------------------------
// launchd
// ---------------------------------------------------------------------------

fn xml_escape(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The `launchd` plist `ledger nightly --plist` prints: one job, run daily at
/// 02:00 local, that shells out to `qbo-local report --live` for yesterday's
/// QBO trial balance CSV and then to `ledger nightly` against it. One
/// `/bin/sh -c "A && B"` job rather than two launchd jobs — launchd has no
/// built-in "run B only if A succeeded", and a shell `&&` says the same thing
/// in a form anyone reading this file can verify by eye.
pub fn render_launchd_plist(
    label: &str,
    realm: &RealmId,
    company: &str,
    db: &Path,
    replica: &Path,
    qbo_csv: &Path,
    out_dir: &Path,
) -> String {
    // Yesterday, per §7 ("as of the previous day"), computed at run time
    // rather than baked into the plist — BSD `date` (macOS's), matching the
    // Mac this runs on (ROADMAP.md §A).
    const YESTERDAY: &str = "$(date -v-1d +%Y-%m-%d)";

    let report_cmd = format!(
        "qbo-local report --realm {realm} --name trial-balance --as-of {YESTERDAY} --out {} --live",
        qbo_csv.display(),
    );
    let nightly_cmd = format!(
        "ledger nightly --db {} --company {company} --replica {} --realm {realm} \
         --qbo-csv {} --out {} --as-of {YESTERDAY}",
        db.display(),
        replica.display(),
        qbo_csv.display(),
        out_dir.display(),
    );
    let script = xml_escape(&format!("{report_cmd} && {nightly_cmd}"));

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>/bin/sh</string>
        <string>-c</string>
        <string>{script}</string>
    </array>
    <key>StartCalendarInterval</key>
    <dict>
        <key>Hour</key>
        <integer>2</integer>
        <key>Minute</key>
        <integer>0</integer>
    </dict>
    <key>StandardOutPath</key>
    <string>{out}/nightly.log</string>
    <key>StandardErrorPath</key>
    <string>{out}/nightly.err.log</string>
    <key>RunAtLoad</key>
    <false/>
</dict>
</plist>
"#,
        out = out_dir.display(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chart;
    use crate::store::CommandMeta;
    use crate::types::{
        AccountId, Application, ClassId, ContactKind, ContactRef, DocKind, DocLine, JournalEntry,
        JournalLine, LedgerDocument, LineKind,
    };
    use ledger_core::Money;

    fn realm() -> RealmId {
        RealmId::parse("1234567890123456").unwrap()
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-12T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn ymd(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    /// A ledger with one posted invoice (Dr 1200 AR, Cr 4100 sales, both
    /// $100.00) and a matching in-memory replica registered for `realm` —
    /// `import_replica` needs a real `Store`, even an empty one, since it
    /// reads the chart/class masters from it before walking any documents.
    fn seeded(company: &str) -> (Ledger, Store) {
        let ledger = Ledger::open_in_memory().unwrap();
        ledger
            .create_company(company, "Aquamentor LLC", Some(realm().as_str()), now())
            .unwrap();

        let doc = LedgerDocument {
            document_id: "doc-1".to_string(),
            kind: DocKind::Invoice,
            number: Some("INV-9001".to_string()),
            txn_date: ymd(2026, 6, 15),
            due_date: None,
            contact: Some(ContactRef {
                kind: ContactKind::Customer,
                id: "cust-1".to_string(),
            }),
            header_class: None,
            lines: vec![DocLine {
                line_no: 1,
                kind: LineKind::Item,
                amount: Money::from_minor(10000),
                class: Some(ClassId("foam".to_string())),
                item_id: None,
                account: None,
                is_taxable: false,
                qty: None,
                unit_cost: None,
                description: None,
                posting: None,
                entity: None,
            }],
            tax: None,
            deposit_to: None,
            pay_from: None,
            applications: Vec::<Application>::new(),
            unapplied: Money::ZERO,
            is_voided: false,
            source_ref: None,
            memo: None,
        };
        let version = ledger
            .save_document(
                company,
                &doc,
                CommandMeta {
                    actor_id: "dan".to_string(),
                    kind: "save_invoice".to_string(),
                    hlc: "hlc-1".to_string(),
                },
                now(),
            )
            .unwrap();
        let entry = JournalEntry {
            entry_date: ymd(2026, 6, 15),
            memo: Some("INV-9001".to_string()),
            source_type: DocKind::Invoice,
            source_id: version.document_id,
            source_version: version.version,
            reversal_of: None,
            is_flagged: false,
            lines: vec![
                JournalLine::debit(
                    1,
                    AccountId(chart::ACCOUNTS_RECEIVABLE.to_string()),
                    Money::from_minor(10000),
                ),
                JournalLine::credit(
                    2,
                    AccountId(chart::SALES_INCOME.to_string()),
                    Money::from_minor(10000),
                )
                .with_class(Some(ClassId("foam".to_string()))),
            ],
        };
        ledger.post_entry(company, &entry, now()).unwrap();

        let store = Store::open_in_memory().unwrap();
        store.register_realm(&realm(), "Aquamentor", now()).unwrap();

        (ledger, store)
    }

    /// An empty ledger and an empty, registered replica — `import_replica`
    /// over nothing at all is a legitimate, cheap way to exercise the nightly
    /// plumbing itself without a document to translate.
    fn empty(company: &str) -> (Ledger, Store) {
        let ledger = Ledger::open_in_memory().unwrap();
        ledger
            .create_company(company, "Aquamentor LLC", Some(realm().as_str()), now())
            .unwrap();
        let store = Store::open_in_memory().unwrap();
        store.register_realm(&realm(), "Aquamentor", now()).unwrap();
        (ledger, store)
    }

    #[test]
    fn a_matching_csv_passes_and_writes_both_files() {
        let (ledger, store) = empty("aquamentor");
        let directory = tempfile::tempdir().unwrap();
        let as_of = ymd(2026, 9, 10);

        // Every account starts at zero on an empty book, so an all-zero QBO
        // side agrees on every must-tier group by construction.
        let qbo_rows = vec![QboTbRow {
            qbo_account_id: "no-such-account".to_string(),
            name: "Nothing".to_string(),
            balance: Money::ZERO,
        }];

        let outcome = run_nightly(
            &ledger,
            &store,
            &realm(),
            "aquamentor",
            &qbo_rows,
            as_of,
            directory.path(),
            now(),
        )
        .unwrap();

        assert!(
            outcome.passed(),
            "{}",
            outcome.diff.render_text("aquamentor", as_of)
        );
        assert_eq!(outcome.summary_line(), "nightly: green");
        assert!(outcome.text_path.exists());
        assert!(outcome.csv_path.exists());
        assert_eq!(
            outcome.text_path,
            directory.path().join("tbdiff-2026-09-10.txt")
        );
        let csv = fs::read_to_string(&outcome.csv_path).unwrap();
        assert!(csv.starts_with("account,tier,ledger,qbo,delta\n"));
    }

    #[test]
    fn a_1100_mismatch_fails_and_the_text_names_1100() {
        let (ledger, store) = seeded("aquamentor");
        let directory = tempfile::tempdir().unwrap();
        let as_of = ymd(2026, 9, 10);

        // The seed chart's 1100 has no `source_ref` until something maps one
        // onto it (§7's diff joins on `accounts.source_ref`); set it
        // directly so the diff actually lands on this row rather than
        // skipping it as unmapped.
        ledger
            .set_account_source_ref("aquamentor", chart::CHECKING, "checking-qbo-id")
            .unwrap();
        // The ledger's own Checking (1100) balance is zero (the seeded
        // invoice never touched it); a QBO side that puts money there is a
        // deliberate must-tier mismatch.
        let qbo_rows = vec![QboTbRow {
            qbo_account_id: "checking-qbo-id".to_string(),
            name: "Checking".to_string(),
            balance: Money::from_minor(50_000),
        }];

        let outcome = run_nightly(
            &ledger,
            &store,
            &realm(),
            "aquamentor",
            &qbo_rows,
            as_of,
            directory.path(),
            now(),
        )
        .unwrap();

        assert!(!outcome.passed());
        assert!(outcome.summary_line().starts_with("nightly: RED"));
        let text = fs::read_to_string(&outcome.text_path).unwrap();
        assert!(
            text.contains("1100"),
            "expected the report to name 1100:\n{text}"
        );
        assert!(text.contains("MUST-MATCH FAILURES: "));
        assert!(!text.contains("MUST-MATCH FAILURES: 0"));
    }

    #[test]
    fn render_launchd_plist_chains_report_then_nightly_at_2am() {
        let plist = render_launchd_plist(
            "com.aquamentor.ledger.nightly",
            &realm(),
            "aquamentor",
            Path::new("/Users/dan/.local/ledger.db"),
            Path::new("/Users/dan/.local/replica.db"),
            Path::new("/Users/dan/.local/qbo-tb.csv"),
            Path::new("/Users/dan/.local/nightly"),
        );
        assert!(plist.contains("<key>Label</key>"));
        assert!(plist.contains("com.aquamentor.ledger.nightly"));
        assert!(plist.contains("qbo-local report"));
        assert!(plist.contains("ledger nightly"));
        assert!(
            plist.contains("&amp;&amp;"),
            "the shell && must be XML-escaped"
        );
        assert!(plist.contains("<integer>2</integer>"));
        assert!(plist.contains("<integer>0</integer>"));
        assert!(plist.contains("--live"));
    }
}
