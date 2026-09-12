//! Chart mapping. `LEDGER-DESIGN.md` §2, "Mapping the QBO chart onto it".
//!
//! A QBO account with no counterpart is **created, never dropped**: it lands in
//! the x990-x999 slot of its band, named as QBO names it, `needs_mapping = 1`.
//! This module only ever produces that mapping as data; nothing here writes to
//! a store.

use std::collections::HashSet;

use qbo_local::project::ParsedAccount;

use crate::chart::{self, AccountSeed, Classification};
use crate::types::AccountId;

/// One QBO account resolved onto the §2 chart.
#[derive(Clone, Debug, PartialEq)]
pub struct MappedAccount {
    /// The QBO account id (`accounts.source_ref`, kept forever, §2).
    pub source_ref: String,
    pub ledger_number: AccountId,
    /// QBO's own name for the account — W14 (name mapping) is still open, so
    /// this mirrors the source rather than guessing a canonical rename.
    pub name: String,
    pub classification: Classification,
    pub needs_mapping: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AccountMapping {
    pub mapped: Vec<MappedAccount>,
}

impl AccountMapping {
    pub fn by_source_ref(&self, qbo_id: &str) -> Option<&MappedAccount> {
        self.mapped
            .iter()
            .find(|account| account.source_ref == qbo_id)
    }
}

/// Map every replica account onto the §2 chart. Deterministic: sorted by
/// `qbo_id` before anything else, so the same input always produces the same
/// numbering, including which unmapped account gets which x99N suffix and
/// which bank/card account gets which sequential number.
pub fn map_accounts(replica_accounts: &[ParsedAccount]) -> AccountMapping {
    let seed = chart::seed_chart();
    let mut used: HashSet<String> = HashSet::new();
    let mut mapped = Vec::with_capacity(replica_accounts.len());
    let mut bank_pending: Vec<&ParsedAccount> = Vec::new();
    let mut card_pending: Vec<&ParsedAccount> = Vec::new();

    let mut ordered: Vec<&ParsedAccount> = replica_accounts.iter().collect();
    ordered.sort_by(|a, b| a.qbo_id.cmp(&b.qbo_id));

    for account in ordered {
        if let Some(seeded) = acct_num_seed_match(account, &seed) {
            let number = next_available(seeded.number, &mut used);
            mapped.push(MappedAccount {
                source_ref: account.qbo_id.clone(),
                ledger_number: number,
                name: account.name.clone(),
                classification: seeded.classification,
                needs_mapping: false,
            });
            continue;
        }

        if matches_kind(account, "Bank") {
            bank_pending.push(account);
            continue;
        }
        if matches_kind(account, "CreditCard") {
            card_pending.push(account);
            continue;
        }

        match resolve_fixed(account) {
            Some((number, classification)) => {
                let assigned = next_available(number, &mut used);
                mapped.push(MappedAccount {
                    source_ref: account.qbo_id.clone(),
                    ledger_number: assigned,
                    name: account.name.clone(),
                    classification,
                    needs_mapping: false,
                });
            }
            None => {
                let classification = infer_classification(account);
                let assigned = next_available(classification.unmapped_slot(), &mut used);
                mapped.push(MappedAccount {
                    source_ref: account.qbo_id.clone(),
                    ledger_number: assigned,
                    name: account.name.clone(),
                    classification,
                    needs_mapping: true,
                });
            }
        }
    }

    // Bank and credit card accounts number sequentially, in acct_num order —
    // the base slot for the first of each, then +1 for every additional one.
    bank_pending.sort_by_key(|account| sort_key(account));
    for account in bank_pending {
        let assigned = next_available(chart::CHECKING, &mut used);
        mapped.push(MappedAccount {
            source_ref: account.qbo_id.clone(),
            ledger_number: assigned,
            name: account.name.clone(),
            classification: Classification::Asset,
            needs_mapping: false,
        });
    }

    card_pending.sort_by_key(|account| sort_key(account));
    for account in card_pending {
        let assigned = next_available(chart::CREDIT_CARD, &mut used);
        mapped.push(MappedAccount {
            source_ref: account.qbo_id.clone(),
            ledger_number: assigned,
            name: account.name.clone(),
            classification: Classification::Liability,
            needs_mapping: false,
        });
    }

    AccountMapping { mapped }
}

fn sort_key(account: &ParsedAccount) -> (String, String) {
    (
        account.acct_num.clone().unwrap_or_default(),
        account.qbo_id.clone(),
    )
}

/// `acct_num` is preferred over any other rule when it already names a real
/// chart number (§2: "Preferred when the QBO chart already numbers an
/// account").
fn acct_num_seed_match<'a>(
    account: &ParsedAccount,
    seed: &'a [AccountSeed],
) -> Option<&'a AccountSeed> {
    let raw = account.acct_num.as_deref()?;
    if raw.len() == 4 && raw.bytes().all(|b| b.is_ascii_digit()) {
        seed.iter().find(|a| a.number == raw)
    } else {
        None
    }
}

/// Case- and punctuation-insensitive equality against `account_type` or
/// `account_subtype` — QBO spells the same concept "Accounts Receivable" in
/// one field and "AccountsReceivable" in the other.
fn matches_kind(account: &ParsedAccount, needle: &str) -> bool {
    let needle = normalize(needle);
    let hit = |field: &Option<String>| {
        field.as_deref().map(normalize).as_deref() == Some(needle.as_str())
    };
    hit(&account.account_type) || hit(&account.account_subtype)
}

fn name_contains(account: &ParsedAccount, needle: &str) -> bool {
    account.name.to_lowercase().contains(&needle.to_lowercase())
}

fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

/// The single-slot rules from the §2 mapping table, checked in an order that
/// resolves the ambiguous cases first (shipping income before generic income,
/// sales tax before generic liability).
fn resolve_fixed(account: &ParsedAccount) -> Option<(&'static str, Classification)> {
    use Classification::*;

    if matches_kind(account, "AccountsReceivable") {
        return Some((chart::ACCOUNTS_RECEIVABLE, Asset));
    }
    if matches_kind(account, "UndepositedFunds") {
        return Some((chart::UNDEPOSITED_FUNDS, Asset));
    }
    if matches_kind(account, "AccountsPayable") {
        return Some((chart::ACCOUNTS_PAYABLE, Liability));
    }
    if matches_kind(account, "SalesTaxPayable") || name_contains(account, "sales tax") {
        return Some((chart::SALES_TAX_PAYABLE, Liability));
    }
    if matches_kind(account, "Inventory") {
        return Some(if name_contains(account, "raw") {
            (chart::INVENTORY_RAW, Asset)
        } else {
            (chart::INVENTORY_FINISHED, Asset)
        });
    }
    if matches_kind(account, "CostOfGoodsSold") {
        return Some((chart::COGS, Cogs));
    }
    if matches_kind(account, "ShippingIncome") || name_contains(account, "shipping") {
        return Some((chart::SHIPPING_INCOME, Income));
    }
    if matches_kind(account, "Income") || matches_kind(account, "SalesOfProductIncome") {
        return Some((chart::SALES_INCOME, Income));
    }
    if matches_kind(account, "DiscountsRefundsGiven") {
        return Some((chart::DISCOUNTS_GIVEN, Income));
    }
    if matches_kind(account, "OtherIncome") {
        return Some((chart::OTHER_INCOME, Income));
    }
    if matches_kind(account, "OpeningBalanceEquity") {
        return Some((chart::OPENING_BALANCE_EQUITY, Equity));
    }
    if matches_kind(account, "RetainedEarnings") {
        return Some((chart::RETAINED_EARNINGS, Equity));
    }
    if matches_kind(account, "OwnersEquity") || matches_kind(account, "Equity") {
        return Some((chart::OWNER_CAPITAL, Equity));
    }
    if let Some(number) = expense_keyword_match(account) {
        return Some((number, Expense));
    }
    None
}

/// Expense subtypes to the 6xxx band that fits, by keyword. Only fires for an
/// account QBO already calls `Expense` or `Other Expense` — a keyword match
/// against an account of a different type would be a coincidence, not a rule.
fn expense_keyword_match(account: &ParsedAccount) -> Option<&'static str> {
    if !(matches_kind(account, "Expense") || matches_kind(account, "OtherExpense")) {
        return None;
    }
    let haystack = format!(
        "{} {}",
        account.account_subtype.clone().unwrap_or_default(),
        account.name
    )
    .to_lowercase();

    const TABLE: &[(&str, &str)] = &[
        ("advertis", chart::ADVERTISING_CHANNEL_FEES),
        ("rent", chart::RENT_UTILITIES),
        ("utilit", chart::RENT_UTILITIES),
        ("insur", chart::INSURANCE),
        ("legal", chart::PROFESSIONAL_FEES),
        ("profession", chart::PROFESSIONAL_FEES),
        ("merchant", chart::MERCHANT_FEES),
        ("bankcharg", chart::MERCHANT_FEES),
        ("creditcardcharg", chart::MERCHANT_FEES),
        ("baddebt", chart::BAD_DEBT),
        ("vehicle", chart::VEHICLE_FUEL),
        ("auto", chart::VEHICLE_FUEL),
        ("fuel", chart::VEHICLE_FUEL),
        ("suppl", chart::SHOP_SUPPLIES),
        ("offic", chart::SHOP_SUPPLIES),
        ("packag", chart::SHOP_SUPPLIES),
    ];
    TABLE
        .iter()
        .find(|(keyword, _)| haystack.contains(keyword))
        .map(|(_, number)| *number)
}

/// Best-effort band for an account no specific rule matched, so the x990 slot
/// it lands in is at least in the right classification.
fn infer_classification(account: &ParsedAccount) -> Classification {
    if matches_kind(account, "CostOfGoodsSold") {
        return Classification::Cogs;
    }
    if matches_kind(account, "Expense") || matches_kind(account, "OtherExpense") {
        return Classification::Expense;
    }
    if matches_kind(account, "Income") || matches_kind(account, "OtherIncome") {
        return Classification::Income;
    }
    if matches_kind(account, "Equity") {
        return Classification::Equity;
    }
    if matches_kind(account, "AccountsPayable")
        || matches_kind(account, "CreditCard")
        || matches_kind(account, "OtherCurrentLiability")
        || matches_kind(account, "LongTermLiability")
    {
        return Classification::Liability;
    }
    if matches_kind(account, "Bank")
        || matches_kind(account, "AccountsReceivable")
        || matches_kind(account, "OtherCurrentAsset")
        || matches_kind(account, "FixedAsset")
        || matches_kind(account, "OtherAsset")
    {
        return Classification::Asset;
    }

    match account.classification.as_deref().map(normalize).as_deref() {
        Some("asset") => Classification::Asset,
        Some("liability") => Classification::Liability,
        Some("equity") => Classification::Equity,
        Some("revenue") | Some("income") => Classification::Income,
        // No readable classification at all: Expense is the least damaging
        // guess, and `needs_mapping = true` means it is never trusted silently.
        _ => Classification::Expense,
    }
}

/// The first unused four-digit number at or after `base`. Threading one
/// `used` set through every assignment — fixed slots, bank/card sequences and
/// unmapped x990 slots alike — is what keeps every ledger number in the
/// mapping unique without three separate collision rules.
fn next_available(base: &str, used: &mut HashSet<String>) -> AccountId {
    let mut n: u32 = base.parse().unwrap_or(0);
    loop {
        let candidate = n.to_string();
        if !used.contains(&candidate) {
            used.insert(candidate.clone());
            return AccountId(candidate);
        }
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(id: &str, name: &str) -> ParsedAccount {
        ParsedAccount {
            qbo_id: id.to_string(),
            name: name.to_string(),
            acct_num: None,
            account_type: None,
            account_subtype: None,
            classification: None,
            balance: None,
            is_active: true,
            is_deleted: false,
        }
    }

    #[test]
    fn accounts_receivable_maps_to_1200() {
        let mut a = account("79", "Accounts Receivable (A/R)");
        a.account_type = Some("Accounts Receivable".into());
        let mapping = map_accounts(&[a]);
        let mapped = mapping.by_source_ref("79").unwrap();
        assert_eq!(mapped.ledger_number, AccountId("1200".into()));
        assert_eq!(mapped.classification, Classification::Asset);
        assert!(!mapped.needs_mapping);
    }

    #[test]
    fn undeposited_funds_maps_to_1150() {
        let mut a = account("80", "Undeposited Funds");
        a.account_type = Some("Other Current Asset".into());
        a.account_subtype = Some("UndepositedFunds".into());
        let mapping = map_accounts(&[a]);
        let mapped = mapping.by_source_ref("80").unwrap();
        assert_eq!(mapped.ledger_number, AccountId("1150".into()));
    }

    #[test]
    fn a_second_bank_account_gets_the_next_number_in_acct_num_order() {
        let mut checking = account("1", "Checking");
        checking.account_type = Some("Bank".into());
        checking.acct_num = Some("9999".into()); // not a seed number, so no override
        let mut savings = account("2", "Savings");
        savings.account_type = Some("Bank".into());
        savings.acct_num = Some("1000".into());

        // "2" (Savings, acct_num 1000) sorts before "1" (Checking, acct_num
        // 9999) by acct_num, so it should claim 1100 and Checking gets 1101.
        let mapping = map_accounts(&[checking, savings]);
        assert_eq!(
            mapping.by_source_ref("2").unwrap().ledger_number,
            AccountId("1100".into())
        );
        assert_eq!(
            mapping.by_source_ref("1").unwrap().ledger_number,
            AccountId("1101".into())
        );
    }

    #[test]
    fn acct_num_is_preferred_when_it_names_a_real_chart_number() {
        let mut a = account("55", "Merchant Fees");
        a.account_type = Some("Expense".into());
        a.acct_num = Some("6700".into());
        let mapping = map_accounts(&[a]);
        assert_eq!(
            mapping.by_source_ref("55").unwrap().ledger_number,
            AccountId("6700".into())
        );
        assert_eq!(
            mapping.by_source_ref("55").unwrap().classification,
            Classification::Expense
        );
    }

    #[test]
    fn an_unmatched_account_lands_in_the_x990_slot_and_needs_mapping() {
        let mut a = account("101", "Ask My Accountant");
        a.account_type = Some("Other Current Asset".into());
        let mapping = map_accounts(&[a]);
        let mapped = mapping.by_source_ref("101").unwrap();
        assert_eq!(mapped.ledger_number, AccountId("1990".into()));
        assert!(mapped.needs_mapping);
        assert_eq!(mapped.name, "Ask My Accountant");
    }

    #[test]
    fn a_second_unmatched_account_in_the_same_band_gets_the_next_suffix() {
        let mut a = account("101", "Ask My Accountant");
        a.account_type = Some("Other Current Asset".into());
        let mut b = account("102", "Uncategorized Asset");
        b.account_type = Some("Other Current Asset".into());
        let mapping = map_accounts(&[a, b]);
        assert_eq!(
            mapping.by_source_ref("101").unwrap().ledger_number,
            AccountId("1990".into())
        );
        assert_eq!(
            mapping.by_source_ref("102").unwrap().ledger_number,
            AccountId("1991".into())
        );
    }

    #[test]
    fn mapping_is_deterministic() {
        let mut a = account("101", "Ask My Accountant");
        a.account_type = Some("Other Current Asset".into());
        let mut b = account("55", "Merchant Fees");
        b.account_type = Some("Expense".into());
        b.acct_num = Some("6700".into());
        let mut bank = account("2", "Savings");
        bank.account_type = Some("Bank".into());

        let accounts = vec![a, b, bank];
        let first = map_accounts(&accounts);
        let second = map_accounts(&accounts);
        assert_eq!(first, second);
    }

    #[test]
    fn cost_of_goods_sold_maps_to_5000_in_the_cogs_band() {
        let mut a = account("40", "Cost of Goods Sold");
        a.account_type = Some("Cost of Goods Sold".into());
        let mapping = map_accounts(&[a]);
        let mapped = mapping.by_source_ref("40").unwrap();
        assert_eq!(mapped.ledger_number, AccountId("5000".into()));
        assert_eq!(mapped.classification, Classification::Cogs);
    }

    #[test]
    fn shipping_income_is_distinguished_from_generic_income() {
        let mut shipping = account("21", "Shipping Income");
        shipping.account_type = Some("Income".into());
        shipping.account_subtype = Some("ShippingIncome".into());
        let mut sales = account("22", "Sales");
        sales.account_type = Some("Income".into());
        sales.account_subtype = Some("SalesOfProductIncome".into());

        let mapping = map_accounts(&[shipping, sales]);
        assert_eq!(
            mapping.by_source_ref("21").unwrap().ledger_number,
            AccountId("4300".into())
        );
        assert_eq!(
            mapping.by_source_ref("22").unwrap().ledger_number,
            AccountId("4100".into())
        );
    }
}
