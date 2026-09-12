//! `ledger` binary. Subcommands arrive with the store; until then this reports
//! that the crate exists and what it contains.

fn main() {
    println!(
        "ledger: {} seed accounts, posting rules for {} document kinds",
        ledger::chart::seed_chart().len(),
        ledger::types::DocKind::ALL.len()
    );
}
