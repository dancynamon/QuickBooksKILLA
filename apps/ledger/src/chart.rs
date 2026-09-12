//! The chart of accounts. `LEDGER-DESIGN.md` §2.
//!
//! Numbers are four digits in classification bands with room between them. The
//! constants are the numbers the posting rules (§1) name; the seed is what a
//! new company starts with. A QBO account with no counterpart is created in
//! the x990 to x999 slot of its band with `needs_mapping` set, never dropped.

use serde::{Deserialize, Serialize};

pub const CHECKING: &str = "1100";
pub const UNDEPOSITED_FUNDS: &str = "1150";
pub const AUTHNET_CLEARING: &str = "1160";
pub const ACCOUNTS_RECEIVABLE: &str = "1200";
pub const ALLOWANCE_DOUBTFUL: &str = "1250";
pub const INVENTORY_RAW: &str = "1300";
pub const INVENTORY_FINISHED: &str = "1310";
pub const WORK_IN_PROGRESS: &str = "1320";
pub const PREPAID: &str = "1400";
pub const MACHINERY: &str = "1500";
pub const ACCUM_DEPRECIATION: &str = "1590";
pub const ACCOUNTS_PAYABLE: &str = "2000";
pub const INVENTORY_RECEIVED_NOT_BILLED: &str = "2050";
pub const CREDIT_CARD: &str = "2100";
pub const SALES_TAX_PAYABLE: &str = "2200";
pub const CUSTOMER_DEPOSITS: &str = "2300";
pub const PAYROLL_LIABILITIES: &str = "2400";
pub const NOTES_PAYABLE: &str = "2900";
pub const OWNER_CAPITAL: &str = "3000";
pub const OWNER_DRAWS: &str = "3100";
pub const RETAINED_EARNINGS: &str = "3900";
pub const OPENING_BALANCE_EQUITY: &str = "3950";
pub const SALES_INCOME: &str = "4100";
pub const SHIPPING_INCOME: &str = "4300";
pub const DISCOUNTS_GIVEN: &str = "4900";
pub const RETURNS_ALLOWANCES: &str = "4950";
pub const OTHER_INCOME: &str = "4990";
pub const COGS: &str = "5000";
pub const FREIGHT_IN: &str = "5050";
pub const MANUFACTURING_VARIANCE: &str = "5100";
pub const INVENTORY_ADJUSTMENT: &str = "5150";
pub const PARTS_LABOUR_APPLIED: &str = "5300";
pub const DIRECT_LABOUR: &str = "5400";
pub const SHOP_SUPPLIES: &str = "6100";
pub const VEHICLE_FUEL: &str = "6200";
pub const RENT_UTILITIES: &str = "6300";
pub const INSURANCE: &str = "6400";
pub const PROFESSIONAL_FEES: &str = "6500";
pub const ADVERTISING_CHANNEL_FEES: &str = "6600";
pub const MERCHANT_FEES: &str = "6700";
pub const BAD_DEBT: &str = "6800";
pub const OTHER_OPERATING: &str = "6900";
pub const INTEREST_EXPENSE: &str = "7100";

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum Classification {
    Asset,
    Liability,
    Equity,
    Income,
    Cogs,
    Expense,
}

impl Classification {
    pub const fn as_str(self) -> &'static str {
        match self {
            Classification::Asset => "Asset",
            Classification::Liability => "Liability",
            Classification::Equity => "Equity",
            Classification::Income => "Income",
            Classification::Cogs => "COGS",
            Classification::Expense => "Expense",
        }
    }

    pub fn parse(raw: &str) -> Option<Classification> {
        match raw {
            "Asset" => Some(Classification::Asset),
            "Liability" => Some(Classification::Liability),
            "Equity" => Some(Classification::Equity),
            "Income" => Some(Classification::Income),
            "COGS" => Some(Classification::Cogs),
            "Expense" => Some(Classification::Expense),
            _ => None,
        }
    }

    /// The side a non-contra account of this classification normally carries.
    pub const fn normal_balance(self) -> crate::types::Side {
        match self {
            Classification::Asset | Classification::Cogs | Classification::Expense => {
                crate::types::Side::Debit
            }
            Classification::Liability | Classification::Equity | Classification::Income => {
                crate::types::Side::Credit
            }
        }
    }

    /// The x990 slot for an unmapped QBO account of this band (§2).
    pub const fn unmapped_slot(self) -> &'static str {
        match self {
            Classification::Asset => "1990",
            Classification::Liability => "2990",
            Classification::Equity => "3990",
            Classification::Income => "4990",
            Classification::Cogs => "5990",
            Classification::Expense => "6990",
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AccountSeed {
    pub number: &'static str,
    pub name: &'static str,
    pub classification: Classification,
    pub is_contra: bool,
}

const fn seed(
    number: &'static str,
    name: &'static str,
    classification: Classification,
    is_contra: bool,
) -> AccountSeed {
    AccountSeed {
        number,
        name,
        classification,
        is_contra,
    }
}

/// The §2 table, in order.
pub fn seed_chart() -> Vec<AccountSeed> {
    use Classification::*;
    vec![
        seed(CHECKING, "Checking", Asset, false),
        seed(UNDEPOSITED_FUNDS, "Undeposited funds", Asset, false),
        seed(AUTHNET_CLEARING, "Authorize.net clearing", Asset, false),
        seed(ACCOUNTS_RECEIVABLE, "Accounts receivable", Asset, false),
        seed(
            ALLOWANCE_DOUBTFUL,
            "Allowance for doubtful accounts",
            Asset,
            true,
        ),
        seed(INVENTORY_RAW, "Inventory, raw materials", Asset, false),
        seed(
            INVENTORY_FINISHED,
            "Inventory, finished goods",
            Asset,
            false,
        ),
        seed(WORK_IN_PROGRESS, "Work in progress", Asset, false),
        seed(PREPAID, "Prepaid expenses", Asset, false),
        seed(MACHINERY, "Machinery and equipment", Asset, false),
        seed(ACCUM_DEPRECIATION, "Accumulated depreciation", Asset, true),
        seed(ACCOUNTS_PAYABLE, "Accounts payable", Liability, false),
        seed(
            INVENTORY_RECEIVED_NOT_BILLED,
            "Inventory received not billed",
            Liability,
            false,
        ),
        seed(CREDIT_CARD, "Credit card", Liability, false),
        seed(SALES_TAX_PAYABLE, "Sales tax payable", Liability, false),
        seed(
            CUSTOMER_DEPOSITS,
            "Customer deposits and unapplied payments",
            Liability,
            false,
        ),
        seed(PAYROLL_LIABILITIES, "Payroll liabilities", Liability, false),
        seed(NOTES_PAYABLE, "Notes and loans payable", Liability, false),
        seed(OWNER_CAPITAL, "Owner capital", Equity, false),
        seed(OWNER_DRAWS, "Owner draws", Equity, true),
        seed(RETAINED_EARNINGS, "Retained earnings", Equity, false),
        seed(
            OPENING_BALANCE_EQUITY,
            "Opening balance equity",
            Equity,
            false,
        ),
        seed(SALES_INCOME, "Sales income", Income, false),
        seed(SHIPPING_INCOME, "Shipping income", Income, false),
        seed(DISCOUNTS_GIVEN, "Discounts given", Income, true),
        seed(RETURNS_ALLOWANCES, "Returns and allowances", Income, true),
        seed(OTHER_INCOME, "Other income", Income, false),
        seed(COGS, "Cost of goods sold", Cogs, false),
        seed(FREIGHT_IN, "Freight in", Cogs, false),
        seed(
            MANUFACTURING_VARIANCE,
            "Manufacturing variance",
            Cogs,
            false,
        ),
        seed(
            INVENTORY_ADJUSTMENT,
            "Inventory adjustment and shrinkage",
            Cogs,
            false,
        ),
        seed(PARTS_LABOUR_APPLIED, "Parts and labour applied", Cogs, true),
        seed(DIRECT_LABOUR, "Direct labour", Cogs, false),
        seed(SHOP_SUPPLIES, "Shop supplies and packaging", Expense, false),
        seed(VEHICLE_FUEL, "Vehicle and fuel", Expense, false),
        seed(RENT_UTILITIES, "Rent and utilities", Expense, false),
        seed(INSURANCE, "Insurance", Expense, false),
        seed(PROFESSIONAL_FEES, "Professional fees", Expense, false),
        seed(
            ADVERTISING_CHANNEL_FEES,
            "Advertising and channel fees",
            Expense,
            false,
        ),
        seed(MERCHANT_FEES, "Merchant and bank fees", Expense, false),
        seed(BAD_DEBT, "Bad debt", Expense, false),
        seed(OTHER_OPERATING, "Other operating expense", Expense, false),
        seed(INTEREST_EXPENSE, "Interest expense", Expense, false),
    ]
}

/// The §3 class list for Aquamentor. WaterLine carries `cnc` and `uv` only.
pub const AQUAMENTOR_CLASSES: &[(&str, &str)] = &[
    ("foam", "Foam products"),
    ("sign", "Signs"),
    ("chair", "Lifeguard chairs"),
    ("drop", "Dropship and resale"),
    ("cnc", "CNC cutting"),
    ("uv", "UV printing"),
];

pub const WATERLINE_CLASSES: &[(&str, &str)] = &[("cnc", "CNC cutting"), ("uv", "UV printing")];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_numbers_are_unique_and_banded() {
        let chart = seed_chart();
        let mut numbers: Vec<&str> = chart.iter().map(|a| a.number).collect();
        numbers.sort_unstable();
        numbers.dedup();
        assert_eq!(numbers.len(), chart.len());
        for account in &chart {
            let band = &account.number[..1];
            let expected = match account.classification {
                Classification::Asset => "1",
                Classification::Liability => "2",
                Classification::Equity => "3",
                Classification::Income => "4",
                Classification::Cogs => "5",
                Classification::Expense => "6",
            };
            assert!(
                band == expected
                    || (account.classification == Classification::Expense && band == "7"),
                "{} is in the wrong band for {:?}",
                account.number,
                account.classification
            );
        }
    }

    #[test]
    fn the_accounts_the_posting_rules_name_exist() {
        let chart = seed_chart();
        for needed in [
            ACCOUNTS_RECEIVABLE,
            SALES_TAX_PAYABLE,
            UNDEPOSITED_FUNDS,
            INVENTORY_RAW,
            INVENTORY_RECEIVED_NOT_BILLED,
            MANUFACTURING_VARIANCE,
            OPENING_BALANCE_EQUITY,
        ] {
            assert!(chart.iter().any(|a| a.number == needed), "{needed} missing");
        }
    }
}
