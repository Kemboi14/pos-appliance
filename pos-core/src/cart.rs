//! Cart, discounts and the pricing engine (discount allocation and tax).
//!
//! Pricing rules, in order:
//! 1. `gross = round(unit_price × qty / 1000)`, or the embedded amount for
//!    price-embedded barcodes.
//! 2. Line discount: percent → `round(gross × r)`, fixed → `min(fixed, gross)`.
//! 3. Basket discount on the sum of discounted lines, allocated back to lines
//!    in proportion to their amount (largest remainder, exact).
//! 4. `amount = gross − line_discount − basket_share` (never negative).
//! 5. Tax per [`TaxCalc`]: per line, or per class on the class sum and then
//!    allocated back to lines. Inclusive mode extracts `a × r/(1+r)`;
//!    exclusive adds `a × r`.
//! 6. `line_total = amount + (exclusive ? tax : 0)`, `total = Σ line_total`.
//!
//! Every division uses `Config::rounding`. Invariant (tested with proptest):
//! `Σ line_total == total` and `Σ line_tax == tax_total`.

use crate::catalog::Item;
use crate::config::{Config, TaxCalc, TaxClassId, TaxMode};
use crate::money::{MResult, Money, MoneyError, Qty, Rate, allocate};
use alloc::string::String;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Discount {
    Percent(Rate),
    Fixed(Money),
}

impl Discount {
    fn amount_on(self, base: Money, cfg: &Config) -> MResult<Money> {
        let d = match self {
            Discount::Percent(r) => base.mul_div(i64::from(r.0), Rate::PPM, cfg.rounding)?,
            Discount::Fixed(m) => m,
        };
        Ok(Money(d.0.clamp(0, base.0.max(0))))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Line {
    pub sku: String,
    pub name: String,
    pub unit_price: Money,
    pub qty: Qty,
    pub tax_class: TaxClassId,
    pub discount: Option<Discount>,
    /// Fixed line amount from a price-embedded barcode.
    pub fixed_amount: Option<Money>,
    /// Set when a manager overrode the catalog price.
    pub price_override: bool,
}

impl Line {
    pub fn from_item(item: &Item, qty: Qty) -> Line {
        Line {
            sku: item.sku.clone(),
            name: item.name.clone(),
            unit_price: item.price,
            qty,
            tax_class: item.tax_class,
            discount: None,
            fixed_amount: None,
            price_override: false,
        }
    }

    pub fn gross(&self, cfg: &Config) -> MResult<Money> {
        match self.fixed_amount {
            Some(a) => Ok(a),
            None => self.unit_price.mul_div(self.qty.0, 1000, cfg.rounding),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CartError {
    Money(MoneyError),
    BadQty,
    NoSuchLine,
    TooManyLines,
    UnknownTaxClass,
}

impl From<MoneyError> for CartError {
    fn from(e: MoneyError) -> Self {
        CartError::Money(e)
    }
}

pub const MAX_LINES: usize = 500;
/// 1,000,000 units: anything larger is a keying error.
pub const MAX_QTY: i64 = 1_000_000_000;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cart {
    pub lines: Vec<Line>,
    pub basket_discount: Option<Discount>,
}

impl Cart {
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Add a line. Counted (non-embedded, non-weighable) items with the same
    /// SKU, price and no discount are merged by increasing the quantity.
    pub fn add(&mut self, line: Line) -> Result<usize, CartError> {
        if line.qty.0 <= 0 || line.qty.0 > MAX_QTY {
            return Err(CartError::BadQty);
        }
        if line.fixed_amount.is_none() && line.discount.is_none() && line.qty.is_whole()
            && let Some((i, l)) = self.lines.iter_mut().enumerate().rev().find(|(_, l)| {
                l.sku == line.sku
                    && l.unit_price == line.unit_price
                    && l.fixed_amount.is_none()
                    && l.discount.is_none()
                    && l.qty.is_whole()
            }) {
                let q = l.qty.0.checked_add(line.qty.0).ok_or(CartError::BadQty)?;
                if q > MAX_QTY {
                    return Err(CartError::BadQty);
                }
                l.qty = Qty(q);
                return Ok(i);
            }
        if self.lines.len() >= MAX_LINES {
            return Err(CartError::TooManyLines);
        }
        self.lines.push(line);
        Ok(self.lines.len() - 1)
    }

    pub fn set_qty(&mut self, idx: usize, qty: Qty) -> Result<(), CartError> {
        if qty.0 <= 0 || qty.0 > MAX_QTY {
            return Err(CartError::BadQty);
        }
        let l = self.lines.get_mut(idx).ok_or(CartError::NoSuchLine)?;
        if l.fixed_amount.is_some() {
            return Err(CartError::BadQty);
        }
        l.qty = qty;
        Ok(())
    }

    pub fn remove(&mut self, idx: usize) -> Result<Line, CartError> {
        if idx >= self.lines.len() {
            return Err(CartError::NoSuchLine);
        }
        Ok(self.lines.remove(idx))
    }

    pub fn price(&self, cfg: &Config) -> Result<Priced, CartError> {
        price(self, cfg)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PricedLine {
    pub gross: Money,
    pub line_discount: Money,
    pub basket_discount: Money,
    /// Gross minus discounts. Includes tax in inclusive mode.
    pub amount: Money,
    pub tax: Money,
    /// What the customer pays for this line.
    pub total: Money,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaxLine {
    pub class: TaxClassId,
    pub rate: Rate,
    /// Taxable amount (sum of line amounts in this class).
    pub base: Money,
    pub tax: Money,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Totals {
    pub subtotal: Money,
    pub discount: Money,
    pub taxes: Vec<TaxLine>,
    pub tax: Money,
    pub total: Money,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Priced {
    pub lines: Vec<PricedLine>,
    pub totals: Totals,
}

fn tax_on(amount: Money, rate: Rate, cfg: &Config) -> MResult<Money> {
    let r = i64::from(rate.0);
    match cfg.tax_mode {
        TaxMode::Exclusive => amount.mul_div(r, Rate::PPM, cfg.rounding),
        TaxMode::Inclusive => amount.mul_div(r, Rate::PPM + r, cfg.rounding),
    }
}

pub fn price(cart: &Cart, cfg: &Config) -> Result<Priced, CartError> {
    let n = cart.lines.len();
    let mut gross = Vec::with_capacity(n);
    let mut ldisc = Vec::with_capacity(n);
    let mut after = Vec::with_capacity(n);
    for l in &cart.lines {
        let g = l.gross(cfg)?;
        let d = match l.discount {
            Some(d) => d.amount_on(g, cfg)?,
            None => Money::ZERO,
        };
        gross.push(g);
        ldisc.push(d);
        after.push(g.checked_sub(d)?);
    }
    let after_sum = Money::sum(after.iter().copied())?;
    let bd_total = match cart.basket_discount {
        Some(d) if n > 0 => d.amount_on(after_sum, cfg)?,
        _ => Money::ZERO,
    };
    let weights: Vec<i64> = after.iter().map(|m| m.0).collect();
    let bshare = if n > 0 {
        allocate(bd_total, &weights)?
    } else {
        Vec::new()
    };
    let mut amount = Vec::with_capacity(n);
    for i in 0..n {
        amount.push(after[i].checked_sub(bshare[i])?);
    }

    // Tax classes in config order, only those used.
    let mut classes: Vec<(TaxClassId, Rate)> = Vec::new();
    for l in &cart.lines {
        let tc = cfg
            .tax_class(l.tax_class)
            .ok_or(CartError::UnknownTaxClass)?;
        if !classes.iter().any(|(id, _)| *id == tc.id) {
            classes.push((tc.id, tc.rate));
        }
    }
    classes.sort_by_key(|(id, _)| cfg.tax_classes.iter().position(|t| t.id == *id));

    let mut tax = alloc::vec![Money::ZERO; n];
    let mut taxes = Vec::with_capacity(classes.len());
    for &(cid, rate) in &classes {
        let idx: Vec<usize> = (0..n).filter(|&i| cart.lines[i].tax_class == cid).collect();
        let base = Money::sum(idx.iter().map(|&i| amount[i]))?;
        let class_tax = match cfg.tax_calc {
            TaxCalc::PerLine => {
                let mut s = Money::ZERO;
                for &i in &idx {
                    tax[i] = tax_on(amount[i], rate, cfg)?;
                    s = s.checked_add(tax[i])?;
                }
                s
            }
            TaxCalc::PerInvoice => {
                let t = tax_on(base, rate, cfg)?;
                let w: Vec<i64> = idx.iter().map(|&i| amount[i].0).collect();
                let parts = allocate(t, &w)?;
                for (k, &i) in idx.iter().enumerate() {
                    tax[i] = parts[k];
                }
                t
            }
        };
        taxes.push(TaxLine {
            class: cid,
            rate,
            base,
            tax: class_tax,
        });
    }

    let mut lines = Vec::with_capacity(n);
    for i in 0..n {
        let total = match cfg.tax_mode {
            TaxMode::Exclusive => amount[i].checked_add(tax[i])?,
            TaxMode::Inclusive => amount[i],
        };
        lines.push(PricedLine {
            gross: gross[i],
            line_discount: ldisc[i],
            basket_discount: bshare[i],
            amount: amount[i],
            tax: tax[i],
            total,
        });
    }
    let subtotal = Money::sum(gross.iter().copied())?;
    let discount = Money::sum(ldisc.iter().copied())?.checked_add(bd_total)?;
    let tax_total = Money::sum(taxes.iter().map(|t| t.tax))?;
    let total = Money::sum(lines.iter().map(|l| l.total))?;
    Ok(Priced {
        lines,
        totals: Totals {
            subtotal,
            discount,
            taxes,
            tax: tax_total,
            total,
        },
    })
}
