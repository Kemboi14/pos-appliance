//! Stock levels per SKU.
//!
//! An item is *tracked* once stock has been received or counted for it.
//! Sales and refunds move tracked stock automatically; untracked items are
//! sold without stock bookkeeping (e.g. services, bags). Stock may go
//! negative: a till never refuses a sale because the books disagree with
//! the shelf, it only flags the item for a count.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

use crate::money::{Money, Qty};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StockLevel {
    pub on_hand: Qty,
    /// Flag the item as low at or below this level (0 = no alert).
    pub reorder_at: Qty,
    /// Unit cost of the last delivery, if one was given.
    pub last_cost: Option<Money>,
    pub last_received: i64,
    pub last_counted: i64,
    /// Sold since the last count or delivery (for the "moving" view).
    pub sold_since: Qty,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StockStatus {
    Ok,
    Low,
    Out,
    Untracked,
}

impl StockStatus {
    pub fn label(self) -> &'static str {
        match self {
            StockStatus::Ok => "In stock",
            StockStatus::Low => "Low",
            StockStatus::Out => "Out",
            StockStatus::Untracked => "Not tracked",
        }
    }
}

/// Why stock left the shelf other than a sale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Adjust {
    Damaged,
    Expired,
    Theft,
    Returned,
    Other,
}

impl Adjust {
    pub const ALL: [Adjust; 5] = [
        Adjust::Damaged,
        Adjust::Expired,
        Adjust::Theft,
        Adjust::Returned,
        Adjust::Other,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Adjust::Damaged => "Damaged",
            Adjust::Expired => "Expired",
            Adjust::Theft => "Theft / loss",
            Adjust::Returned => "Returned to supplier",
            Adjust::Other => "Other",
        }
    }
}

/// One line of a delivery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Received {
    pub sku: String,
    pub qty: Qty,
    pub unit_cost: Option<Money>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inventory {
    levels: BTreeMap<String, StockLevel>,
}

impl Inventory {
    pub fn get(&self, sku: &str) -> Option<&StockLevel> {
        self.levels.get(sku)
    }
    pub fn tracked(&self) -> usize {
        self.levels.len()
    }
    pub fn levels(&self) -> impl Iterator<Item = (&String, &StockLevel)> {
        self.levels.iter()
    }

    pub fn status(&self, sku: &str) -> StockStatus {
        match self.levels.get(sku) {
            None => StockStatus::Untracked,
            Some(l) if l.on_hand.0 <= 0 => StockStatus::Out,
            Some(l) if l.reorder_at.0 > 0 && l.on_hand <= l.reorder_at => StockStatus::Low,
            Some(_) => StockStatus::Ok,
        }
    }

    pub(crate) fn receive(&mut self, lines: &[Received], at: i64) {
        for r in lines {
            let l = self.levels.entry(r.sku.clone()).or_default();
            l.on_hand = Qty(l.on_hand.0.saturating_add(r.qty.0));
            if r.unit_cost.is_some() {
                l.last_cost = r.unit_cost;
            }
            l.last_received = at;
            l.sold_since = Qty(0);
        }
    }

    pub(crate) fn count(&mut self, sku: &str, counted: Qty, at: i64) -> Qty {
        let l = self.levels.entry(sku.into()).or_default();
        let variance = Qty(counted.0.saturating_sub(l.on_hand.0));
        l.on_hand = counted;
        l.last_counted = at;
        l.sold_since = Qty(0);
        variance
    }

    pub(crate) fn adjust(&mut self, sku: &str, delta: Qty) {
        let l = self.levels.entry(sku.into()).or_default();
        l.on_hand = Qty(l.on_hand.0.saturating_add(delta.0));
    }

    pub(crate) fn set_reorder(&mut self, sku: &str, at: Qty) {
        self.levels.entry(sku.into()).or_default().reorder_at = at;
    }

    /// Stock leaves with a sale (tracked items only).
    pub(crate) fn sold(&mut self, sku: &str, qty: Qty) {
        if let Some(l) = self.levels.get_mut(sku) {
            l.on_hand = Qty(l.on_hand.0.saturating_sub(qty.0));
            l.sold_since = Qty(l.sold_since.0.saturating_add(qty.0));
        }
    }

    /// Refunded goods go back on the shelf (tracked items only).
    pub(crate) fn returned(&mut self, sku: &str, qty: Qty) {
        if let Some(l) = self.levels.get_mut(sku) {
            l.on_hand = Qty(l.on_hand.0.saturating_add(qty.0));
        }
    }

    /// A movement from another till (see [`crate::sync::StockMove`]).
    pub(crate) fn apply_move(&mut self, m: &crate::sync::StockMove) {
        if m.track {
            self.adjust(&m.sku, Qty(m.delta));
        } else if let Some(l) = self.levels.get_mut(&m.sku) {
            l.on_hand = Qty(l.on_hand.0.saturating_add(m.delta));
            if m.delta < 0 {
                l.sold_since = Qty(l.sold_since.0.saturating_sub(m.delta));
            }
        }
    }

    /// Replace every level (a lane's resync from the head).
    pub(crate) fn set_all(&mut self, levels: &[(String, StockLevel)]) {
        self.levels = levels.iter().cloned().collect();
    }

    pub(crate) fn forget(&mut self, sku: &str) {
        self.levels.remove(sku);
    }

    /// SKUs at or below their reorder level, or out of stock.
    pub fn alerts(&self) -> Vec<&String> {
        self.levels
            .keys()
            .filter(|s| matches!(self.status(s), StockStatus::Low | StockStatus::Out))
            .collect()
    }
}

/// Validate a quantity typed for stock: positive and at most a million units.
pub fn valid_qty(q: Qty) -> bool {
    q.0 > 0 && q.0 <= 1_000_000_000
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn receive_sell_count() {
        let mut inv = Inventory::default();
        assert_eq!(inv.status("A"), StockStatus::Untracked);
        inv.sold("A", Qty::units(1)); // untracked: no effect
        assert!(inv.get("A").is_none());
        inv.receive(
            &[Received {
                sku: "A".into(),
                qty: Qty::units(10),
                unit_cost: Some(Money(150)),
            }],
            100,
        );
        inv.set_reorder("A", Qty::units(3));
        assert_eq!(inv.status("A"), StockStatus::Ok);
        inv.sold("A", Qty::units(7));
        assert_eq!(inv.status("A"), StockStatus::Low);
        assert_eq!(inv.alerts(), vec![&String::from("A")]);
        inv.sold("A", Qty::units(4));
        assert_eq!(inv.get("A").map(|l| l.on_hand), Some(Qty::units(-1)));
        assert_eq!(inv.status("A"), StockStatus::Out);
        let var = inv.count("A", Qty::units(2), 200);
        assert_eq!(var, Qty::units(3));
        inv.returned("A", Qty::units(1));
        assert_eq!(inv.get("A").map(|l| l.on_hand), Some(Qty::units(3)));
        inv.adjust("A", Qty::units(-1));
        assert_eq!(inv.status("A"), StockStatus::Low);
    }
}
