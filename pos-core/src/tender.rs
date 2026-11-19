//! Tenders, split payment, change and cash rounding.
//!
//! Card tenders are *record only*: the appliance has no network and never
//! processes cards. The cashier runs the card on a standalone terminal and
//! records the amount, the last four digits and the approval code. Full card
//! numbers are rejected so the appliance never holds PCI cardholder data.

use crate::config::Config;
use crate::money::{MResult, Money};
use alloc::string::String;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum TenderKind {
    Cash,
    Card,
    Voucher,
}

impl TenderKind {
    pub fn label(self) -> &'static str {
        match self {
            TenderKind::Cash => "CASH",
            TenderKind::Card => "CARD",
            TenderKind::Voucher => "VOUCHER",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tender {
    pub kind: TenderKind,
    /// Amount handed over (cash may exceed what is due; change is separate).
    pub amount: Money,
    /// Card: "last4/approval"; voucher: voucher code; cash: empty.
    pub reference: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TenderError {
    NotPositive,
    /// Non-cash tenders cannot exceed the remaining balance.
    Overtender,
    AlreadyPaid,
    BadCardReference,
    BadVoucherCode,
    Overflow,
}

impl From<crate::money::MoneyError> for TenderError {
    fn from(_: crate::money::MoneyError) -> Self {
        TenderError::Overflow
    }
}

/// Payment in progress for a sale (or a refund payout).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Payment {
    /// Sale total before cash rounding.
    pub due: Money,
    pub tenders: Vec<Tender>,
    /// Cash rounding adjustment (rounded − exact); applied when a cash tender
    /// settles the balance.
    pub rounding: Money,
    pub change: Money,
}

impl Payment {
    pub fn new(due: Money) -> Payment {
        Payment {
            due,
            tenders: Vec::new(),
            rounding: Money::ZERO,
            change: Money::ZERO,
        }
    }

    pub fn tendered(&self) -> MResult<Money> {
        Money::sum(self.tenders.iter().map(|t| t.amount))
    }

    /// Balance still owed (≥ 0 once complete is impossible to undercut).
    pub fn remaining(&self) -> MResult<Money> {
        let owed = self.due.checked_add(self.rounding)?;
        let paid = self.tendered()?.checked_sub(self.change)?;
        Ok(Money(owed.checked_sub(paid)?.0.max(0)))
    }

    pub fn is_complete(&self) -> bool {
        // A zero-total sale (e.g. 100 % discount) completes with no tender.
        matches!(self.remaining(), Ok(m) if m.is_zero())
    }

    /// What a cash tender must cover to settle the balance, after rounding.
    pub fn cash_due(&self, cfg: &Config) -> MResult<Money> {
        let rem = self.remaining()?;
        rem.round_to_increment(cfg.cash_increment, cfg.cash_rounding)
    }

    pub fn add(&mut self, t: Tender, cfg: &Config) -> Result<(), TenderError> {
        if t.amount.0 <= 0 {
            return Err(TenderError::NotPositive);
        }
        let rem = self.remaining()?;
        if rem.is_zero() {
            return Err(TenderError::AlreadyPaid);
        }
        match t.kind {
            TenderKind::Card => {
                validate_card_ref(&t.reference)?;
                if t.amount > rem {
                    return Err(TenderError::Overtender);
                }
            }
            TenderKind::Voucher => {
                let r = t.reference.trim();
                if r.is_empty()
                    || r.len() > 24
                    || !r.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
                {
                    return Err(TenderError::BadVoucherCode);
                }
                if t.amount > rem {
                    return Err(TenderError::Overtender);
                }
            }
            TenderKind::Cash => {
                let rounded = rem.round_to_increment(cfg.cash_increment, cfg.cash_rounding)?;
                if t.amount >= rounded {
                    // Cash settles the sale: apply rounding and give change.
                    self.rounding = self.rounding.checked_add(rounded.checked_sub(rem)?)?;
                    self.change = t.amount.checked_sub(rounded)?;
                }
            }
        }
        self.tenders.push(t);
        Ok(())
    }
}

/// `NNNN/APPROVAL`: exactly four digits (last four of the card), then an
/// approval code of 1–12 alphanumerics from the standalone terminal.
pub fn validate_card_ref(r: &str) -> Result<(), TenderError> {
    let (last4, appr) = r.split_once('/').ok_or(TenderError::BadCardReference)?;
    if last4.len() != 4 || !last4.bytes().all(|c| c.is_ascii_digit()) {
        return Err(TenderError::BadCardReference);
    }
    if appr.is_empty() || appr.len() > 12 || !appr.bytes().all(|c| c.is_ascii_alphanumeric()) {
        return Err(TenderError::BadCardReference);
    }
    Ok(())
}
