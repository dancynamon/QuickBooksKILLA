use rust_decimal::prelude::ToPrimitive;
use rust_decimal::{Decimal, RoundingStrategy};

use crate::money::{Money, MoneyError};

/// Where a rounding decision is being made.
///
/// Named rather than scattered, so that changing how tax rounds is a one-line
/// change in one place rather than an audit of every call site.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum RoundingPolicy {
    /// Extending a line: quantity × rate → amount. Half-up, away from zero.
    LineExtension,
    /// Calculating tax: taxable subtotal × rate → amount. Half-up, away from
    /// zero — the rule QBO follows.
    ///
    /// This was banker's rounding (half-to-even) until W17 checked it against
    /// the replica (`DECISIONS.md` D46, `ledger::tax_audit`): on every one of
    /// the 25 documents whose tax landed exactly on a half cent, QBO charged
    /// the half-up amount, and on none the half-to-even one. The ledger
    /// computes tax itself and diffs 2200 against QBO nightly, so matching
    /// QBO's rule is what keeps that diff free of one-cent noise. It stays a
    /// separate policy from [`RoundingPolicy::LineExtension`] even though the
    /// strategy is now the same, so a jurisdiction that rounds differently
    /// is still a one-line change here.
    TaxCalculation,
    /// Converting an amount that arrived already rounded by the book of record.
    ///
    /// This is a change of representation, not a rounding decision: `qbo-local`
    /// mirrors the amounts QBO computed rather than recomputing them, and its
    /// caller rejects anything carrying more than two decimal places before it
    /// gets here. A strategy is still required by the signature, so the
    /// line-extension one stands in — it is unreachable by construction, and if
    /// it ever does fire, the caller's precision check is the bug.
    MirroredAmount,
}

impl RoundingPolicy {
    const fn strategy(self) -> RoundingStrategy {
        match self {
            RoundingPolicy::LineExtension => RoundingStrategy::MidpointAwayFromZero,
            RoundingPolicy::TaxCalculation => RoundingStrategy::MidpointAwayFromZero,
            RoundingPolicy::MirroredAmount => RoundingStrategy::MidpointAwayFromZero,
        }
    }
}

/// Convert a `Decimal` to [`Money`] under an explicit rounding policy.
///
/// This is the **only** function in this crate that accepts a `Decimal`, which is
/// what makes it the single conversion point between the two representations.
/// Rates, quantities and tax percentages stay in `Decimal`; they become `Money`
/// here and nowhere else.
///
/// Returns [`MoneyError::OutOfRange`] rather than panicking when the value cannot
/// be represented as `i64` minor units. The previous draft of this function ended
/// in `.to_i64().unwrap()`, which put a panic in the one path all money
/// conversion flows through.
pub fn round_money(amount: Decimal, policy: RoundingPolicy) -> Result<Money, MoneyError> {
    let minor = amount
        .round_dp_with_strategy(2, policy.strategy())
        .checked_mul(Decimal::ONE_HUNDRED)
        .ok_or_else(|| MoneyError::OutOfRange(amount.to_string()))?;

    minor
        .to_i64()
        .map(Money::from_minor)
        .ok_or_else(|| MoneyError::OutOfRange(amount.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use rust_decimal_macros::dec;

    fn line(d: Decimal) -> Money {
        round_money(d, RoundingPolicy::LineExtension).unwrap()
    }

    fn tax(d: Decimal) -> Money {
        round_money(d, RoundingPolicy::TaxCalculation).unwrap()
    }

    #[test]
    fn line_extension_rounds_half_away_from_zero() {
        assert_eq!(line(dec!(0.005)), Money::from_minor(1));
        assert_eq!(line(dec!(0.015)), Money::from_minor(2));
        assert_eq!(line(dec!(0.025)), Money::from_minor(3));
        // Symmetric about zero — a credit memo rounds the same distance as the
        // invoice it reverses, so the pair nets to zero.
        assert_eq!(line(dec!(-0.005)), Money::from_minor(-1));
        assert_eq!(line(dec!(-0.025)), Money::from_minor(-3));
    }

    #[test]
    fn tax_rounds_half_away_from_zero() {
        assert_eq!(tax(dec!(0.005)), Money::from_minor(1));
        assert_eq!(tax(dec!(0.015)), Money::from_minor(2));
        assert_eq!(tax(dec!(0.025)), Money::from_minor(3));
        assert_eq!(tax(dec!(-0.025)), Money::from_minor(-3));
    }

    #[test]
    fn tax_matches_what_qbo_charged_on_a_half_cent() {
        // W17 evidence from the replica, where half to even would be a cent
        // low: estimate 91522 (1,908.00 × NJ 6.625% = 126.405, QBO 126.41)
        // and invoice 67577 (500.00 × 6.625% = 33.125, QBO 33.13).
        assert_eq!(tax(dec!(1908.00) * dec!(0.06625)), Money::from_minor(12641));
        assert_eq!(tax(dec!(500.00) * dec!(0.06625)), Money::from_minor(3313));
    }

    #[test]
    fn non_midpoint_values_round_identically_under_both_policies() {
        for value in [dec!(1.234), dec!(1.236), dec!(-99.991), dec!(0.001)] {
            assert_eq!(line(value), tax(value), "diverged on {value}");
        }
    }

    #[test]
    fn real_invoice_line_extension() {
        // 2 x RING BUOY HOLDER at 19.95 — from Aquamentor invoice #9665.
        assert_eq!(line(dec!(2) * dec!(19.95)), Money::from_minor(3990));
    }

    #[test]
    fn new_jersey_sales_tax_on_a_taxable_subtotal() {
        // NJ 6.625% on 59.85 = 3.9650625, which is above the midpoint and rounds
        // up under either policy.
        let subtotal = dec!(59.85);
        let rate = dec!(0.06625);
        assert_eq!(tax(subtotal * rate), Money::from_minor(397));
    }

    #[test]
    fn sub_cent_unit_price_extends_correctly() {
        // Foam is costed by board-foot at sub-cent rates; the rate keeps full
        // precision and only the extended amount is rounded.
        let board_feet = dec!(1440);
        let rate_per_bf = dec!(0.0834);
        assert_eq!(line(board_feet * rate_per_bf), Money::from_minor(12010)); // 120.096
    }

    #[test]
    fn out_of_range_is_an_error_not_a_panic() {
        // Decimal's range vastly exceeds i64 cents; this must refuse, not unwrap.
        let huge = Decimal::MAX;
        assert!(matches!(
            round_money(huge, RoundingPolicy::LineExtension),
            Err(MoneyError::OutOfRange(_))
        ));
    }

    #[test]
    fn zero_is_zero_under_both_policies() {
        assert_eq!(line(Decimal::ZERO), Money::ZERO);
        assert_eq!(tax(Decimal::ZERO), Money::ZERO);
    }

    proptest! {
        /// Never panics, for any decimal and either policy — the property that
        /// matters most, since this sits in every money path in the system.
        #[test]
        fn never_panics(
            mantissa: i64,
            scale in 0u32..12,
            use_tax: bool,
        ) {
            let value = Decimal::new(mantissa, scale);
            let policy = if use_tax {
                RoundingPolicy::TaxCalculation
            } else {
                RoundingPolicy::LineExtension
            };
            let _ = round_money(value, policy);
        }

        /// Rounding lands within half a cent of the input. Catches an off-by-a-
        /// factor-of-ten in the minor-unit conversion, which unit tests on
        /// hand-picked values can miss.
        #[test]
        fn result_is_within_half_a_cent(mantissa in -10_000_000_000i64..10_000_000_000, scale in 0u32..6) {
            let value = Decimal::new(mantissa, scale);
            let rounded = round_money(value, RoundingPolicy::LineExtension).unwrap();
            let difference = (value * Decimal::ONE_HUNDRED) - Decimal::from(rounded.minor());
            prop_assert!(difference.abs() <= dec!(0.5), "{value} -> {rounded:?}");
        }

        /// Negating the input negates the result. Line extension is symmetric
        /// about zero, so a reversing entry cancels the original exactly.
        #[test]
        fn line_extension_is_sign_symmetric(mantissa in -1_000_000_000i64..1_000_000_000, scale in 0u32..6) {
            let value = Decimal::new(mantissa, scale);
            let positive = round_money(value, RoundingPolicy::LineExtension).unwrap();
            let negative = round_money(-value, RoundingPolicy::LineExtension).unwrap();
            prop_assert_eq!(positive.checked_neg().unwrap(), negative);
        }
    }
}
