//! W17: does QBO round sales tax half up or half to even?
//!
//! `RoundingPolicy::TaxCalculation` once rounded half to even, and the claim
//! that QBO does the same was never checked (`LEDGER-DESIGN.md` W17). This
//! module checks it against the replica. A sale document's
//! `TxnTaxDetail.TaxLine`s state the taxable base QBO used
//! (`NetAmountTaxable`) and each component rate (`TaxPercent`), and
//! `TotalTax` is what it charged. Recomputing `base × combined rate` both
//! ways and comparing against `TotalTax` says which rule QBO follows — but
//! only on documents whose exact product lands on a half cent, because
//! everywhere else the two rules agree. Those documents are the evidence;
//! the rest only show the recomputation is sound.
//!
//! The check is per document, not per tax line, because that is how QBO
//! rounds and how [`crate::books`] computes tax: one rounding of the base
//! times the combined rate. QBO then splits that total across the
//! components so they sum to it, which leaves an individual component
//! rounded the "wrong" way (e.g. 19,875.00 at 1% + 0.7% + 4.3%: 0.7% is
//! 139.125 and lands on 139.12 so the three sum to 1,192.50). A document
//! whose tax lines carry different bases is skipped and counted.
//!
//! `TaxPercent` is the rate's current value, so a document taxed before a
//! rate change (NJ was 7% until 2017, 6.875% in 2017) recomputes at the
//! wrong rate and lands in [`TaxRoundingReport::neither`]. Those carry their
//! date so they can be told apart from an override.
//!
//! Reads the replica only. Never calls QBO.

use std::str::FromStr;

use ledger_core::{round_money, Money, MoneyError, RoundingPolicy};
use qbo_local::domain::{EntityType, RealmId};
use qbo_local::store::{Store, StoreError};
use rust_decimal::{Decimal, RoundingStrategy};
use serde_json::Value;

/// The sale documents that carry a `TxnTaxDetail` with per-rate tax lines.
const TAXED_TYPES: [EntityType; 5] = [
    EntityType::Invoice,
    EntityType::SalesReceipt,
    EntityType::CreditMemo,
    EntityType::RefundReceipt,
    EntityType::Estimate,
];

/// One recomputed document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaxLineCheck {
    pub entity_type: EntityType,
    pub qbo_id: String,
    pub txn_date: String,
    pub taxable: Decimal,
    /// The combined rate: the sum of the components' `TaxPercent`.
    pub percent: Decimal,
    /// `taxable × percent / 100`, unrounded.
    pub exact: Decimal,
    pub qbo_amount: Money,
}

/// What [`audit_tax_rounding`] found across one realm.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TaxRoundingReport {
    /// Documents with a nonzero base that were recomputed.
    pub documents: usize,
    /// Documents skipped because their tax lines carry different bases or
    /// a component that is not percent based.
    pub skipped: usize,
    /// Documents with no tax charged on a nonzero recomputed tax: an exempt
    /// customer or a zeroed rate. Not evidence either way.
    pub zero_tax: usize,
    /// Documents where both rules give QBO's amount and the product is not
    /// on a half cent.
    pub both_match: usize,
    /// Half-cent documents both rules reproduce (the even neighbour is also the
    /// upper one, e.g. 0.515 → 0.52).
    pub midpoint_agree: usize,
    /// Half-cent documents only half up reproduces.
    pub half_up_only: Vec<TaxLineCheck>,
    /// Half-cent documents only half to even reproduces.
    pub half_even_only: Vec<TaxLineCheck>,
    /// Documents neither rule reproduces: an overridden tax amount, or a
    /// rate that has changed since (see the module docs). Not evidence
    /// either way.
    pub neither: Vec<TaxLineCheck>,
}

impl TaxRoundingReport {
    /// Which rule the evidence supports, if the discriminating documents agree.
    pub fn verdict(&self) -> &'static str {
        match (self.half_up_only.len(), self.half_even_only.len()) {
            (0, 0) => "inconclusive: no document tells the rules apart",
            (_, 0) => "half up (away from zero)",
            (0, _) => "half to even",
            _ => "mixed: some half cents round up, some to even",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TaxAuditError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Money(#[from] MoneyError),
}

/// A JSON number as QBO wrote it, without a trip through `f64` arithmetic.
fn decimal(value: &Value) -> Option<Decimal> {
    match value {
        Value::Number(number) => {
            let text = number.to_string();
            Decimal::from_str(&text)
                .or_else(|_| Decimal::from_scientific(&text))
                .ok()
        }
        Value::String(text) => Decimal::from_str(text).ok(),
        _ => None,
    }
}

/// Recompute every taxed sale document in `realm` both ways.
pub fn audit_tax_rounding(
    replica: &Store,
    realm: &RealmId,
) -> Result<TaxRoundingReport, TaxAuditError> {
    let mut report = TaxRoundingReport::default();
    let mut failure = None;
    replica.for_each_entity(realm, |entity| {
        if failure.is_some() || entity.is_deleted || !TAXED_TYPES.contains(&entity.entity_type) {
            return Ok(());
        }
        let detail = &entity.raw_json["TxnTaxDetail"];
        let Some(lines) = detail["TaxLine"]
            .as_array()
            .filter(|lines| !lines.is_empty())
        else {
            return Ok(());
        };
        let mut base = None;
        let mut percent = Decimal::ZERO;
        for line in lines {
            let component = &line["TaxLineDetail"];
            let (Some(taxable), Some(rate), true) = (
                decimal(&component["NetAmountTaxable"]),
                decimal(&component["TaxPercent"]),
                component["PercentBased"].as_bool() != Some(false),
            ) else {
                report.skipped += 1;
                return Ok(());
            };
            if base.is_some_and(|base| base != taxable) {
                report.skipped += 1;
                return Ok(());
            }
            base = Some(taxable);
            percent += rate;
        }
        let (Some(taxable), Some(total)) = (base, decimal(&detail["TotalTax"])) else {
            report.skipped += 1;
            return Ok(());
        };
        if taxable.is_zero() {
            return Ok(());
        }
        let check = TaxLineCheck {
            entity_type: entity.entity_type,
            qbo_id: entity.qbo_id.clone(),
            txn_date: entity.raw_json["TxnDate"]
                .as_str()
                .unwrap_or("")
                .to_string(),
            taxable,
            percent,
            exact: taxable * percent / Decimal::ONE_HUNDRED,
            qbo_amount: Money::ZERO,
        };
        if let Err(error) = classify(&mut report, check, total) {
            failure = Some(error);
        }
        Ok(())
    })?;
    match failure {
        Some(error) => Err(error),
        None => Ok(report),
    }
}

fn classify(
    report: &mut TaxRoundingReport,
    mut check: TaxLineCheck,
    amount: Decimal,
) -> Result<(), TaxAuditError> {
    // The two rules themselves, not `RoundingPolicy`s: the policy is what
    // this check decides, so it cannot be what the check compares.
    let half_up = check
        .exact
        .round_dp_with_strategy(2, RoundingStrategy::MidpointAwayFromZero);
    let half_even = check
        .exact
        .round_dp_with_strategy(2, RoundingStrategy::MidpointNearestEven);
    check.qbo_amount = round_money(amount, RoundingPolicy::MirroredAmount)?;
    let on_half_cent = (check.exact * Decimal::ONE_HUNDRED).fract().abs() == Decimal::new(5, 1);
    report.documents += 1;
    if amount.is_zero() && !half_up.is_zero() {
        report.zero_tax += 1;
        return Ok(());
    }
    match (half_up == amount, half_even == amount) {
        (true, true) if on_half_cent => report.midpoint_agree += 1,
        (true, true) => report.both_match += 1,
        (true, false) => report.half_up_only.push(check),
        (false, true) => report.half_even_only.push(check),
        (false, false) => report.neither.push(check),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use qbo_local::store::MirroredEntity;
    use serde_json::json;

    fn realm() -> RealmId {
        RealmId::parse("1234567890123456").unwrap()
    }

    fn seed(store: &Store, id: &str, taxable: f64, percent: f64, amount: f64) {
        seed_components(store, id, taxable, &[(percent, amount)], amount);
    }

    fn seed_components(store: &Store, id: &str, taxable: f64, parts: &[(f64, f64)], total: f64) {
        let now = Utc.with_ymd_and_hms(2026, 10, 31, 12, 0, 0).unwrap();
        let entity = MirroredEntity {
            entity_type: EntityType::Invoice,
            qbo_id: id.into(),
            sync_token: "0".into(),
            last_updated_utc: now,
            is_deleted: false,
            raw_json: json!({
                "Id": id,
                "TxnDate": "2026-10-01",
                "TxnTaxDetail": {
                    "TotalTax": total,
                    "TaxLine": parts.iter().map(|(percent, amount)| json!({
                        "Amount": amount,
                        "DetailType": "TaxLineDetail",
                        "TaxLineDetail": {
                            "NetAmountTaxable": taxable,
                            "PercentBased": true,
                            "TaxPercent": percent
                        }
                    })).collect::<Vec<_>>()
                }
            }),
        };
        store.upsert_entity(&realm(), &entity, now).unwrap();
    }

    #[test]
    fn half_cent_lines_are_sorted_by_the_rule_that_reproduces_them() {
        let store = Store::open_in_memory().unwrap();
        let now = Utc.with_ymd_and_hms(2026, 10, 31, 12, 0, 0).unwrap();
        store.register_realm(&realm(), "Test", now).unwrap();
        // 49.50 × 6.625% = 3.279375: no half cent, both rules give 3.28.
        seed(&store, "1", 49.5, 6.625, 3.28);
        // 10.50 × 5% = 0.525: half up 0.53, half even 0.52.
        seed(&store, "2", 10.5, 5.0, 0.53);
        // 10.30 × 5% = 0.515: both give 0.52.
        seed(&store, "3", 10.3, 5.0, 0.52);
        // 11.70 × 5% = 0.585: half up 0.59, half even 0.58.
        seed(&store, "4", 11.7, 5.0, 0.58);
        // An overridden amount neither rule reproduces.
        seed(&store, "5", 100.0, 5.0, 4.99);
        // An exempt customer: a base and a rate, but no tax charged.
        seed(&store, "6", 100.0, 5.0, 0.0);

        let report = audit_tax_rounding(&store, &realm()).unwrap();
        assert_eq!(report.documents, 6);
        assert_eq!(report.zero_tax, 1);
        assert_eq!(report.both_match, 1);
        assert_eq!(report.midpoint_agree, 1);
        assert_eq!(report.half_up_only.len(), 1);
        assert_eq!(report.half_up_only[0].qbo_id, "2");
        assert_eq!(report.half_even_only.len(), 1);
        assert_eq!(report.half_even_only[0].qbo_id, "4");
        assert_eq!(report.neither.len(), 1);
        assert_eq!(
            report.verdict(),
            "mixed: some half cents round up, some to even"
        );
    }

    #[test]
    fn a_new_jersey_half_cent_is_caught() {
        // 6.625% is 53/800, so a base of whole dollars ≡ 4 (mod 8) lands on a
        // half cent: 20.00 × 6.625% = 1.325 (half up 1.33, half even 1.32).
        let store = Store::open_in_memory().unwrap();
        let now = Utc.with_ymd_and_hms(2026, 10, 31, 12, 0, 0).unwrap();
        store.register_realm(&realm(), "Test", now).unwrap();
        seed(&store, "1", 20.0, 6.625, 1.33);
        let report = audit_tax_rounding(&store, &realm()).unwrap();
        assert_eq!(report.half_up_only.len(), 1);
        assert_eq!(report.verdict(), "half up (away from zero)");
    }

    #[test]
    fn components_are_judged_on_their_combined_total() {
        // The replica's own invoice 84395: 19,875.00 at 1% + 0.7% + 4.3%.
        // The 0.7% component alone looks half-to-even (139.125 → 139.12),
        // but it is QBO splitting a 1,192.50 total, which is no half cent.
        let store = Store::open_in_memory().unwrap();
        let now = Utc.with_ymd_and_hms(2026, 10, 31, 12, 0, 0).unwrap();
        store.register_realm(&realm(), "Test", now).unwrap();
        seed_components(
            &store,
            "1",
            19875.0,
            &[(1.0, 198.75), (0.7, 139.12), (4.3, 854.63)],
            1192.5,
        );
        let report = audit_tax_rounding(&store, &realm()).unwrap();
        assert_eq!(report.both_match, 1);
        assert!(report.half_even_only.is_empty());
    }
}
