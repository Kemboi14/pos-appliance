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

/// Figures per product for [`ShiftStats`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemStats {
    pub name: alloc::string::String,
    pub qty: i64,
    pub total: Money,
}

/// Running figures for the Reports app: sales by hour of day, by product
/// and by cashier for the current (or last) shift. Kept in the state so
/// they survive a reboot; not part of X/Z reports.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShiftStats {
    pub shift_id: u32,
    /// Hour of day (0-23, till clock) → (sales, sales total).
    pub hours: BTreeMap<u8, (u32, Money)>,
    /// SKU → figures (net of refunds).
    pub items: BTreeMap<alloc::string::String, ItemStats>,
    /// Cashier → (sales, sales total).
    pub cashiers: BTreeMap<UserId, (u32, Money)>,
}

impl ShiftStats {
    pub fn new(shift_id: u32) -> Self {
        ShiftStats {
            shift_id,
            ..Self::default()
        }
    }

    pub fn hour_of(at: i64) -> u8 {
        (at.rem_euclid(86_400) / 3600) as u8
    }

    /// Saturating: statistics never block a sale.
    pub fn add_sale(&mut self, s: &Sale) {
        let add = |m: Money, v: Money| Money(m.0.saturating_add(v.0));
        let h = self.hours.entry(Self::hour_of(s.at)).or_default();
        h.0 = h.0.saturating_add(1);
        h.1 = add(h.1, s.totals.total);
        let c = self.cashiers.entry(s.cashier).or_default();
        c.0 = c.0.saturating_add(1);
        c.1 = add(c.1, s.totals.total);
        for l in &s.lines {
            let e = self.items.entry(l.line.sku.clone()).or_default();
            e.name.clone_from(&l.line.name);
            e.qty = e.qty.saturating_add(l.line.qty.0);
            e.total = add(e.total, l.priced.total);
        }
    }

    pub fn add_refund(&mut self, r: &Refund) {
        for l in &r.lines {
            if let Some(e) = self.items.get_mut(&l.sku) {
                e.qty = e.qty.saturating_sub(l.qty.0);
                e.total = Money(e.total.0.saturating_sub(l.total.0));
            }
        }
    }

    /// Best sellers by takings, most first.
    pub fn top_items(&self, n: usize) -> Vec<(&str, &ItemStats)> {
        let mut v: Vec<_> = self
            .items
            .iter()
            .filter(|(_, i)| i.total.0 > 0)
            .map(|(k, i)| (k.as_str(), i))
            .collect();
        v.sort_by(|a, b| b.1.total.cmp(&a.1.total).then(a.0.cmp(b.0)));
        v.truncate(n);
        v
    }
}
