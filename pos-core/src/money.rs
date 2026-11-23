//! Fixed-point money, rates, quantities and rounding. Integer only.

use core::fmt;
use serde::{Deserialize, Serialize};

/// An amount in minor currency units (e.g. cents). Arithmetic is checked;
/// overflow is reported as [`MoneyError::Overflow`], never wrapped.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub struct Money(pub i64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoneyError {
    Overflow,
    DivideByZero,
}

impl fmt::Display for MoneyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MoneyError::Overflow => f.write_str("arithmetic overflow"),
            MoneyError::DivideByZero => f.write_str("division by zero"),
        }
    }
}

pub type MResult<T> = Result<T, MoneyError>;

impl Money {
    pub const ZERO: Money = Money(0);

    pub const fn minor(v: i64) -> Self {
        Money(v)
    }
    pub fn checked_add(self, o: Money) -> MResult<Money> {
        self.0
            .checked_add(o.0)
            .map(Money)
            .ok_or(MoneyError::Overflow)
    }
    pub fn checked_sub(self, o: Money) -> MResult<Money> {
        self.0
            .checked_sub(o.0)
            .map(Money)
            .ok_or(MoneyError::Overflow)
    }
    pub fn checked_neg(self) -> MResult<Money> {
        self.0.checked_neg().map(Money).ok_or(MoneyError::Overflow)
    }
    pub fn is_negative(self) -> bool {
        self.0 < 0
    }
    pub fn is_zero(self) -> bool {
        self.0 == 0
    }
    pub fn sum<I: IntoIterator<Item = Money>>(it: I) -> MResult<Money> {
        it.into_iter().try_fold(Money::ZERO, Money::checked_add)
    }

    /// `self * num / den`, rounded with `mode`, computed in i128 so the
    /// intermediate product cannot overflow; the result must fit in i64.
    pub fn mul_div(self, num: i64, den: i64, mode: Rounding) -> MResult<Money> {
        let r = div_round(i128::from(self.0) * i128::from(num), i128::from(den), mode)?;
        i64::try_from(r)
            .map(Money)
            .map_err(|_| MoneyError::Overflow)
    }

    /// Round to a multiple of `increment` (cash rounding, e.g. 5 for 0.05).
    pub fn round_to_increment(self, increment: i64, mode: Rounding) -> MResult<Money> {
        if increment <= 0 {
            return Err(MoneyError::DivideByZero);
        }
        let q = div_round(i128::from(self.0), i128::from(increment), mode)?;
        let v = q
            .checked_mul(i128::from(increment))
            .ok_or(MoneyError::Overflow)?;
        i64::try_from(v)
            .map(Money)
            .map_err(|_| MoneyError::Overflow)
    }

    /// Format with `decimals` fractional digits, e.g. `-12.34`.
    pub fn display(self, decimals: u8) -> MoneyDisplay {
        MoneyDisplay { m: self, decimals }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MoneyDisplay {
    m: Money,
    decimals: u8,
}

impl fmt::Display for MoneyDisplay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let v = i128::from(self.m.0);
        let neg = v < 0;
        let a = v.unsigned_abs();
        let s = if neg { "-" } else { "" };
        if self.decimals == 0 {
            return f.pad(&alloc::format!("{s}{a}"));
        }
        let p = 10u128.pow(u32::from(self.decimals));
        let w = usize::from(self.decimals);
        f.pad(&alloc::format!("{s}{}.{:0w$}", a / p, a % p))
    }
}

/// Rounding policy for every division in the domain. Configured per
/// jurisdiction via [`crate::config::Config`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Rounding {
    /// Round half away from zero (commercial rounding): 0.5 → 1, -0.5 → -1.
    HalfUp,
    /// Round half to even (banker's rounding): 0.5 → 0, 1.5 → 2.
    HalfEven,
    /// Truncate toward zero.
    Down,
}

/// Divide `n / d` and round to an integer using `mode`.
pub fn div_round(n: i128, d: i128, mode: Rounding) -> MResult<i128> {
    if d == 0 {
        return Err(MoneyError::DivideByZero);
    }
    let (n, d) = if d < 0 { (-n, -d) } else { (n, d) };
    let q = n / d; // truncates toward zero
    let r = n % d; // same sign as n
    if r == 0 {
        return Ok(q);
    }
    let twice = r.unsigned_abs() * 2;
    let du = d.unsigned_abs();
    let away = if n < 0 { q - 1 } else { q + 1 };
    Ok(match mode {
        Rounding::Down => q,
        Rounding::HalfUp => {
            if twice >= du {
                away
            } else {
                q
            }
        }
        Rounding::HalfEven => {
            if twice > du || (twice == du && q % 2 != 0) {
                away
            } else {
                q
            }
        }
    })
}

/// A rate in parts per million: 16 % = `Rate(160_000)`, 8.875 % =
/// `Rate(88_750)`. Used for tax and percentage discounts.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub struct Rate(pub u32);

impl Rate {
    pub const PPM: i64 = 1_000_000;
    pub const ZERO: Rate = Rate(0);
    pub const fn percent(p: u32) -> Rate {
        Rate(p * 10_000)
    }
    pub fn is_valid_fraction(self) -> bool {
        i64::from(self.0) <= Self::PPM
    }
}

impl fmt::Display for Rate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let whole = self.0 / 10_000;
        let frac = self.0 % 10_000;
        if frac == 0 {
            write!(f, "{whole}%")
        } else {
            let mut fr = alloc::format!("{frac:04}");
            while fr.ends_with('0') {
                fr.pop();
            }
            write!(f, "{whole}.{fr}%")
        }
    }
}

/// Quantity in thousandths of a unit (grams for kg-priced items; 1000 = one
/// piece for counted items).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub struct Qty(pub i64);

impl Qty {
    pub const ONE: Qty = Qty(1000);
    pub const fn units(n: i64) -> Qty {
        Qty(n * 1000)
    }
    pub fn is_whole(self) -> bool {
        self.0 % 1000 == 0
    }
}

impl fmt::Display for Qty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_whole() {
            write!(f, "{}", self.0 / 1000)
        } else {
            let neg = if self.0 < 0 { "-" } else { "" };
            let a = self.0.unsigned_abs();
            write!(f, "{neg}{}.{:03}", a / 1000, a % 1000)
        }
    }
}

/// Split `total` into parts proportional to `weights` such that the parts sum
/// to exactly `total` (largest-remainder method; ties go to the earliest
/// index, so the result is deterministic). All weights must be ≥ 0; if all are
/// zero the whole amount goes to the first slot.
pub fn allocate(total: Money, weights: &[i64]) -> MResult<alloc::vec::Vec<Money>> {
    use alloc::vec::Vec;
    if weights.is_empty() {
        return if total.is_zero() {
            Ok(Vec::new())
        } else {
            Err(MoneyError::DivideByZero)
        };
    }
    let wsum: i128 = weights.iter().map(|&w| i128::from(w.max(0))).sum();
    let mut out: Vec<Money> = alloc::vec![Money::ZERO; weights.len()];
    if wsum == 0 {
        out[0] = total;
        return Ok(out);
    }
    let t = i128::from(total.0);
    let mut rems: Vec<(i128, usize)> = Vec::with_capacity(weights.len());
    let mut assigned: i128 = 0;
    for (i, &w) in weights.iter().enumerate() {
        let p = t * i128::from(w.max(0));
        let q = p / wsum; // toward zero
        let r = (p % wsum).abs();
        out[i] = Money(i64::try_from(q).map_err(|_| MoneyError::Overflow)?);
        assigned += q;
        rems.push((r, i));
    }
    let mut left = t - assigned; // |left| < number of weights
    rems.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let step: i64 = if left >= 0 { 1 } else { -1 };
    let mut k = 0;
    while left != 0 {
        let idx = rems[k % rems.len()].1;
        out[idx] = out[idx].checked_add(Money(step))?;
        left -= i128::from(step);
        k += 1;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    #[test]
    fn rounding_modes() {
        use Rounding::*;
        let cases: &[(i128, i128, Rounding, i128)] = &[
            (5, 2, HalfUp, 3),
            (-5, 2, HalfUp, -3),
            (5, 2, HalfEven, 2),
            (7, 2, HalfEven, 4),
            (-5, 2, HalfEven, -2),
            (-7, 2, HalfEven, -4),
            (7, 3, HalfUp, 2),
            (8, 3, HalfUp, 3),
            (8, 3, Down, 2),
            (-8, 3, Down, -2),
            (10, -4, HalfUp, -3),
        ];
        for &(n, d, m, want) in cases {
            assert_eq!(div_round(n, d, m).unwrap(), want, "{n}/{d} {m:?}");
        }
        assert_eq!(div_round(1, 0, HalfUp), Err(MoneyError::DivideByZero));
    }

    #[test]
    fn overflow_is_error() {
        assert_eq!(
            Money(i64::MAX).checked_add(Money(1)),
            Err(MoneyError::Overflow)
        );
        assert_eq!(Money(i64::MIN).checked_neg(), Err(MoneyError::Overflow));
        assert_eq!(
            Money(i64::MAX).mul_div(2, 1, Rounding::HalfUp),
            Err(MoneyError::Overflow)
        );
        // Large intermediate products are fine thanks to i128.
        assert_eq!(
            Money(i64::MAX).mul_div(1_000_000, 1_000_000, Rounding::HalfUp),
            Ok(Money(i64::MAX))
        );
    }

    #[test]
    fn cash_rounding() {
        let m = |v| Money(v).round_to_increment(5, Rounding::HalfUp).unwrap().0;
        assert_eq!(m(1001), 1000);
        assert_eq!(m(1002), 1000);
        assert_eq!(m(1003), 1005);
        assert_eq!(m(1007), 1005);
        assert_eq!(m(1008), 1010);
    }

    #[test]
    fn display() {
        assert_eq!(Money(-1234).display(2).to_string(), "-12.34");
        assert_eq!(Money(5).display(2).to_string(), "0.05");
        assert_eq!(Money(5).display(0).to_string(), "5");
        assert_eq!(
            Money(i64::MIN).display(2).to_string(),
            "-92233720368547758.08"
        );
        assert_eq!(Rate(88_750).to_string(), "8.875%");
        assert_eq!(Rate::percent(16).to_string(), "16%");
        assert_eq!(Qty(1250).to_string(), "1.250");
        assert_eq!(Qty::units(3).to_string(), "3");
    }

    #[test]
    fn allocate_exact() {
        let parts = allocate(Money(100), &[1, 1, 1]).unwrap();
        assert_eq!(parts, alloc::vec![Money(34), Money(33), Money(33)]);
        let parts = allocate(Money(-100), &[1, 1, 1]).unwrap();
        assert_eq!(parts, alloc::vec![Money(-34), Money(-33), Money(-33)]);
        let parts = allocate(Money(10), &[0, 0]).unwrap();
        assert_eq!(parts, alloc::vec![Money(10), Money(0)]);
    }
}
