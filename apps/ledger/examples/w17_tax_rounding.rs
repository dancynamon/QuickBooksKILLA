//! W17 evidence: does QBO round sales tax half up or half to even?
//!
//! ```text
//! sqlite3 -readonly .local/replica.db ".backup '$S/replica.db'"
//! cargo run -p ledger --example w17_tax_rounding -- --replica $S/replica.db
//! ```
//!
//! Opens the replica read-only, recomputes every taxed sale document's
//! `TotalTax` both ways ([`ledger::tax_audit`]), and prints which rule
//! reproduces QBO's amount on the half-cent documents — the only ones where
//! the two rules differ. Never calls QBO.

use ledger::tax_audit::{audit_tax_rounding, TaxLineCheck};
use qbo_local::store::Store;

fn sample(label: &str, lines: &[TaxLineCheck], limit: usize) {
    println!("  {label}: {}", lines.len());
    for line in lines.iter().take(limit) {
        println!(
            "    {} {:?} {:>6}  {} × {}% = {}  QBO {}.{:02}",
            line.txn_date,
            line.entity_type,
            line.qbo_id,
            line.taxable.normalize(),
            line.percent.normalize(),
            line.exact.normalize(),
            line.qbo_amount.minor() / 100,
            line.qbo_amount.minor() % 100,
        );
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mut replica = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--replica" => replica = args.next(),
            other => return Err(format!("unknown flag {other}").into()),
        }
    }
    let replica = Store::open_read_only(replica.ok_or("--replica PATH is required")?)?;
    for realm in replica.list_realms()? {
        let report = audit_tax_rounding(&replica, &realm.realm_id)?;
        println!("{} ({})", realm.display_name, realm.realm_id.as_str());
        println!("  documents recomputed: {}", report.documents);
        println!("  skipped (mixed bases): {}", report.skipped);
        println!("  no tax charged (exempt): {}", report.zero_tax);
        println!(
            "  not on a half cent, both rules match: {}",
            report.both_match
        );
        println!(
            "  on a half cent, both rules match: {}",
            report.midpoint_agree
        );
        sample(
            "on a half cent, only half up matches",
            &report.half_up_only,
            usize::MAX,
        );
        sample(
            "on a half cent, only half to even matches",
            &report.half_even_only,
            usize::MAX,
        );
        sample(
            "neither rule matches (old rate or override)",
            &report.neither,
            0,
        );
        let mut by_year = std::collections::BTreeMap::<&str, usize>::new();
        for line in &report.neither {
            *by_year
                .entry(line.txn_date.get(..4).unwrap_or("?"))
                .or_default() += 1;
        }
        println!("    by year: {by_year:?}");
        println!("  verdict: {}\n", report.verdict());
    }
    Ok(())
}
