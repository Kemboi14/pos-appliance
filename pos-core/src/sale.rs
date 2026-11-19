//! Completed sales and refunds as they are journaled.

use crate::auth::UserId;
use crate::cart::{Line, PricedLine, Totals};
use crate::money::{Money, Qty};
use crate::tender::Tender;
use alloc::string::String;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SoldLine {
    pub line: Line,
    pub priced: PricedLine,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sale {
    pub receipt_no: u64,
    pub shift_id: u32,
    pub cashier: UserId,
    pub at: i64,
    pub lines: Vec<SoldLine>,
    pub totals: Totals,
    pub tenders: Vec<Tender>,
    pub rounding: Money,
    pub change: Money,
}

impl Sale {
    /// Amount actually collected: total + cash rounding.
    pub fn collected(&self) -> Option<Money> {
        self.totals.total.checked_add(self.rounding).ok()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefundLine {
    /// Index into the original sale's lines.
    pub index: u16,
    pub sku: String,
    pub qty: Qty,
    /// Refunded amount for this line, tax included (always ≥ 0).
    pub total: Money,
    pub tax: Money,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refund {
    /// Refunds take their own number from the same gap-free receipt sequence.
    pub receipt_no: u64,
    pub original_receipt: u64,
    pub shift_id: u32,
    pub cashier: UserId,
    pub authorized_by: UserId,
    pub at: i64,
    pub lines: Vec<RefundLine>,
    pub total: Money,
    pub tax: Money,
    /// Payout tenders (amounts positive = paid to customer).
    pub payout: Vec<Tender>,
    pub reason: String,
}

/// Refund bookkeeping kept for every sale inside the refund window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refundable {
    pub receipt_no: u64,
    pub lines: Vec<RefundableLine>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefundableLine {
    pub sku: String,
    pub name: String,
    pub qty: Qty,
    pub total: Money,
    pub tax: Money,
    pub refunded_qty: Qty,
    pub refunded_total: Money,
    pub refunded_tax: Money,
}
