//! Shifts, running counters and X/Z reports.

use crate::auth::UserId;
use crate::config::TaxClassId;
use crate::money::{MResult, Money, Rate};
use crate::sale::{Refund, Sale};
use crate::tender::TenderKind;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaxTotals {
    pub rate: Rate,
    pub base: Money,
    pub tax: Money,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counters {
    pub sales: u32,
    pub refunds: u32,
    /// Σ line gross before discounts.
    pub gross: Money,
    pub discounts: Money,
    /// Σ sale totals (what customers owed, before cash rounding).
    pub sales_total: Money,
    pub refunds_total: Money,
    pub cash_rounding: Money,
    pub change_given: Money,
    pub taxes: BTreeMap<TaxClassId, TaxTotals>,
    pub refund_tax: Money,
    /// Tendered amounts by kind (cash is gross of change).
    pub tenders: BTreeMap<TenderKind, Money>,
    pub payouts: BTreeMap<TenderKind, Money>,
    pub line_voids: u32,
    pub sale_voids: u32,
    pub no_sales: u32,
    pub reprints: u32,
    pub overrides: u32,
    pub first_receipt: Option<u64>,
    pub last_receipt: Option<u64>,
}

fn bump(m: &mut BTreeMap<TenderKind, Money>, k: TenderKind, v: Money) -> MResult<()> {
    let e = m.entry(k).or_insert(Money::ZERO);
    *e = e.checked_add(v)?;
    Ok(())
}

impl Counters {
    fn receipt(&mut self, no: u64) {
        if self.first_receipt.is_none() {
            self.first_receipt = Some(no);
        }
        self.last_receipt = Some(no);
    }

    pub fn add_sale(&mut self, s: &Sale) -> MResult<()> {
        self.sales = self.sales.saturating_add(1);
        self.gross = self.gross.checked_add(s.totals.subtotal)?;
        self.discounts = self.discounts.checked_add(s.totals.discount)?;
        self.sales_total = self.sales_total.checked_add(s.totals.total)?;
        self.cash_rounding = self.cash_rounding.checked_add(s.rounding)?;
        self.change_given = self.change_given.checked_add(s.change)?;
        for t in &s.totals.taxes {
            let e = self.taxes.entry(t.class).or_insert_with(|| TaxTotals {
                rate: t.rate,
                ..TaxTotals::default()
            });
            e.base = e.base.checked_add(t.base)?;
            e.tax = e.tax.checked_add(t.tax)?;
        }
        for t in &s.tenders {
            bump(&mut self.tenders, t.kind, t.amount)?;
        }
        self.receipt(s.receipt_no);
        Ok(())
    }

    pub fn add_refund(&mut self, r: &Refund) -> MResult<()> {
        self.refunds = self.refunds.saturating_add(1);
        self.refunds_total = self.refunds_total.checked_add(r.total)?;
        self.refund_tax = self.refund_tax.checked_add(r.tax)?;
        for t in &r.payout {
            bump(&mut self.payouts, t.kind, t.amount)?;
        }
        self.receipt(r.receipt_no);
        Ok(())
    }

    pub fn tender(&self, k: TenderKind) -> Money {
        self.tenders.get(&k).copied().unwrap_or(Money::ZERO)
    }
    pub fn payout(&self, k: TenderKind) -> Money {
        self.payouts.get(&k).copied().unwrap_or(Money::ZERO)
    }
    /// Net sales after refunds.
    pub fn net(&self) -> MResult<Money> {
        self.sales_total.checked_sub(self.refunds_total)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shift {
    pub id: u32,
    pub opened_by: UserId,
    pub opened_at: i64,
    pub float: Money,
    pub counters: Counters,
}

impl Shift {
    /// Cash that should be in the drawer: float + cash tendered − change −
    /// cash refunds.
    pub fn expected_cash(&self) -> MResult<Money> {
        let c = &self.counters;
        self.float
            .checked_add(c.tender(TenderKind::Cash))?
            .checked_sub(c.change_given)?
            .checked_sub(c.payout(TenderKind::Cash))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReportKind {
    X,
    Z,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub kind: ReportKind,
    /// Z number (monotonic, never reused). `None` for X reports.
    pub z_no: Option<u32>,
    pub at: i64,
    pub by: UserId,
    pub shift: Shift,
    pub expected_cash: Money,
    /// Declared (counted) cash, if the manager entered it.
    pub counted_cash: Option<Money>,
    /// Lifetime grand total of net sales; never reset (fiscal requirement in
    /// many jurisdictions).
    pub grand_total: Money,
    pub extra: Vec<alloc::string::String>,
}
