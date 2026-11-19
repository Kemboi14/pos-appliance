//! Jurisdiction and store configuration, persisted as a `ConfigSet` event.

use crate::barcode::PriceEmbed;
use crate::money::{Rate, Rounding};
use alloc::string::String;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TaxClassId(pub u8);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaxClass {
    pub id: TaxClassId,
    /// Single-letter code printed next to each receipt line (A, B, ...).
    pub code: char,
    pub name: String,
    pub rate: Rate,
}

/// Whether catalog prices include tax.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaxMode {
    /// Shelf price includes tax (VAT style). Tax is extracted:
    /// `tax = amount × r / (1 + r)`.
    Inclusive,
    /// Tax is added on top (US sales-tax style): `tax = amount × r`.
    Exclusive,
}

/// Where tax rounding happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaxCalc {
    /// Round tax on every line, then sum.
    PerLine,
    /// Sum line amounts per tax class, round once per class; the class tax
    /// is then allocated back to lines (largest remainder) for refunds.
    PerInvoice,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub store_name: String,
    pub address: Vec<String>,
    pub tax_id: String,
    pub currency: String,
    pub decimals: u8,
    pub rounding: Rounding,
    pub tax_mode: TaxMode,
    pub tax_calc: TaxCalc,
    pub tax_classes: Vec<TaxClass>,
    /// Cash rounding increment in minor units; 1 disables cash rounding.
    pub cash_increment: i64,
    pub cash_rounding: Rounding,
    pub price_embed: Option<PriceEmbed>,
    /// Failed PIN attempts before lockout.
    pub max_pin_failures: u8,
    /// Keyboard-wedge scanner detection: max gap between characters (ms).
    pub scanner_max_gap_ms: u16,
    /// Minimum code length for an input burst to count as a scan.
    pub scanner_min_len: u8,
    pub receipt_footer: Vec<String>,
    /// Printer paper width in characters (Font A): 32 for 58 mm, 42/48 for 80 mm.
    pub printer_columns: u8,
    /// Number of completed sales kept refundable (older receipts can no
    /// longer be refunded at the till).
    pub refund_window: u32,
    /// Take a state snapshot every N committed records.
    pub snapshot_every: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            store_name: "DEMO STORE".into(),
            address: alloc::vec!["1 Example Street".into()],
            tax_id: "TAX-000000".into(),
            currency: "KES".into(),
            decimals: 2,
            rounding: Rounding::HalfUp,
            tax_mode: TaxMode::Inclusive,
            tax_calc: TaxCalc::PerLine,
            tax_classes: alloc::vec![
                TaxClass {
                    id: TaxClassId(0),
                    code: 'A',
                    name: "Standard".into(),
                    rate: Rate::percent(16),
                },
                TaxClass {
                    id: TaxClassId(1),
                    code: 'Z',
                    name: "Zero".into(),
                    rate: Rate::ZERO,
                },
            ],
            cash_increment: 1,
            cash_rounding: Rounding::HalfUp,
            price_embed: Some(PriceEmbed::default()),
            max_pin_failures: 5,
            scanner_max_gap_ms: 30,
            scanner_min_len: 6,
            receipt_footer: alloc::vec!["Thank you!".into()],
            printer_columns: 42,
            refund_window: 20_000,
            snapshot_every: 256,
        }
    }
}

impl Config {
    pub fn tax_class(&self, id: TaxClassId) -> Option<&TaxClass> {
        self.tax_classes.iter().find(|t| t.id == id)
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.decimals > 4 {
            return Err("decimals > 4");
        }
        if self.cash_increment < 1 {
            return Err("cash increment < 1");
        }
        if self.tax_classes.is_empty() {
            return Err("no tax classes");
        }
        for (i, t) in self.tax_classes.iter().enumerate() {
            if !t.rate.is_valid_fraction() {
                return Err("tax rate > 100%");
            }
            if self.tax_classes[..i]
                .iter()
                .any(|o| o.id == t.id || o.code == t.code)
            {
                return Err("duplicate tax class");
            }
        }
        if !(16..=80).contains(&self.printer_columns) {
            return Err("printer columns out of range");
        }
        if self.max_pin_failures == 0 || self.snapshot_every == 0 || self.refund_window == 0 {
            return Err("zero limit");
        }
        Ok(())
    }
}
