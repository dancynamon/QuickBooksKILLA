use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A signed monetary amount in minor units (cents).
///
/// Never a float, anywhere. `i64` cents covers roughly ±92 quadrillion dollars,
/// so overflow is not a practical concern at this scale — but every operation is
/// checked regardless and returns [`MoneyError`] rather than wrapping. A silent
/// wrap in money code is precisely the class of bug that corrupts a balance
/// without anyone noticing.
///
/// There is no currency tag. Both realms are USD and multi-currency is out of
/// scope. Adding one later means touching every signature, which is the correct
/// cost to pay at the point the scope actually changes.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Money(i64);

#[derive(Debug, Error, PartialEq, Eq)]
pub enum MoneyError {
    #[error("money arithmetic overflowed i64 minor units")]
    Overflow,
    #[error("decimal value {0} is outside the range representable as i64 minor units")]
    OutOfRange(String),
}

impl Money {
    pub const ZERO: Money = Money(0);

    /// Construct from minor units already known to be a whole number of cents.
    ///
    /// This is the database-hydration path: a value read back from an `INTEGER`
    /// column is already in minor units and needs no rounding decision.
    ///
    /// To convert a [`rust_decimal::Decimal`] — a rate, a quantity, an extended
    /// line amount — use [`crate::round_money`], which is the only function in
    /// this crate that accepts a `Decimal`. That is what makes it the single
    /// conversion point, rather than a naming convention that can be worked
    /// around.
    pub const fn from_minor(minor: i64) -> Self {
        Money(minor)
    }

    /// The underlying minor units, for storage and display formatting.
    pub const fn minor(self) -> i64 {
        self.0
    }

    pub fn checked_add(self, rhs: Money) -> Result<Money, MoneyError> {
        self.0.checked_add(rhs.0).map(Money).ok_or(MoneyError::Overflow)
    }

    pub fn checked_sub(self, rhs: Money) -> Result<Money, MoneyError> {
        self.0.checked_sub(rhs.0).map(Money).ok_or(MoneyError::Overflow)
    }

    pub fn checked_neg(self) -> Result<Money, MoneyError> {
        self.0.checked_neg().map(Money).ok_or(MoneyError::Overflow)
    }

    /// Sum with the same overflow guarantee as [`Money::checked_add`].
    ///
    /// Exists so that totalling document lines does not tempt anyone into
    /// `iter().map(Money::minor).sum::<i64>()`, which wraps silently in release.
    pub fn checked_sum<I>(amounts: I) -> Result<Money, MoneyError>
    where
        I: IntoIterator<Item = Money>,
    {
        amounts
            .into_iter()
            .try_fold(Money::ZERO, |acc, amount| acc.checked_add(amount))
    }

    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    pub const fn is_negative(self) -> bool {
        self.0 < 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn minor_units_round_trip() {
        assert_eq!(Money::from_minor(1234).minor(), 1234);
        assert_eq!(Money::from_minor(-1234).minor(), -1234);
        assert_eq!(Money::ZERO.minor(), 0);
    }

    #[test]
    fn addition_and_subtraction() {
        let a = Money::from_minor(1995);
        let b = Money::from_minor(2499);
        assert_eq!(a.checked_add(b).unwrap(), Money::from_minor(4494));
        assert_eq!(b.checked_sub(a).unwrap(), Money::from_minor(504));
        assert_eq!(a.checked_sub(b).unwrap(), Money::from_minor(-504));
    }

    #[test]
    fn overflow_is_an_error_not_a_wrap() {
        let max = Money::from_minor(i64::MAX);
        assert_eq!(max.checked_add(Money::from_minor(1)), Err(MoneyError::Overflow));

        let min = Money::from_minor(i64::MIN);
        assert_eq!(min.checked_sub(Money::from_minor(1)), Err(MoneyError::Overflow));
        // i64::MIN has no positive counterpart.
        assert_eq!(min.checked_neg(), Err(MoneyError::Overflow));
    }

    #[test]
    fn sum_totals_document_lines() {
        // Three lines off a real Aquamentor invoice shape: 2 x 19.95 plus
        // shipping at 24.99.
        let lines = [
            Money::from_minor(1995),
            Money::from_minor(1995),
            Money::from_minor(2499),
        ];
        assert_eq!(Money::checked_sum(lines).unwrap(), Money::from_minor(6489));
    }

    #[test]
    fn empty_sum_is_zero() {
        assert_eq!(Money::checked_sum([]).unwrap(), Money::ZERO);
    }

    #[test]
    fn sum_overflow_is_an_error() {
        let lines = [Money::from_minor(i64::MAX), Money::from_minor(1)];
        assert_eq!(Money::checked_sum(lines), Err(MoneyError::Overflow));
    }

    #[test]
    fn serialises_as_bare_minor_units() {
        // Transparent representation keeps the stored form obvious to anyone
        // reading the JSON audit log or the raw DB.
        let json = serde_json::to_string(&Money::from_minor(-4494)).unwrap();
        assert_eq!(json, "-4494");
        let back: Money = serde_json::from_str(&json).unwrap();
        assert_eq!(back, Money::from_minor(-4494));
    }

    proptest! {
        /// Addition never panics and never wraps: it either reports the exact
        /// mathematical result or refuses.
        #[test]
        fn addition_is_exact_or_refused(a: i64, b: i64) {
            let result = Money::from_minor(a).checked_add(Money::from_minor(b));
            match result {
                Ok(sum) => prop_assert_eq!(sum.minor() as i128, a as i128 + b as i128),
                Err(_) => prop_assert!((a as i128 + b as i128) > i64::MAX as i128
                    || (a as i128 + b as i128) < i64::MIN as i128),
            }
        }

        #[test]
        fn addition_is_commutative(a: i64, b: i64) {
            let (x, y) = (Money::from_minor(a), Money::from_minor(b));
            prop_assert_eq!(x.checked_add(y), y.checked_add(x));
        }

        /// Summing a list agrees with folding it one element at a time.
        #[test]
        fn sum_agrees_with_fold(amounts: Vec<i32>) {
            let monies: Vec<Money> = amounts.iter().map(|&a| Money::from_minor(a as i64)).collect();
            let folded = monies
                .iter()
                .try_fold(Money::ZERO, |acc, &m| acc.checked_add(m));
            prop_assert_eq!(Money::checked_sum(monies), folded);
        }
    }
}
