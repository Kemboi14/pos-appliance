//! Jurisdiction extension point.
//!
//! A [`FiscalPolicy`] can veto sales/refunds, add mandatory receipt and
//! report text, and constrain configuration. Jurisdiction-specific
//! implementations (signature devices, tax-authority QR codes, ...) implement
//! this trait; only [`GenericPolicy`] ships here.

use crate::config::Config;
use crate::sale::{Refund, Sale};
use crate::shift::Report;
use alloc::string::String;
use alloc::vec::Vec;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FiscalRejection(pub String);

pub trait FiscalPolicy {
    fn name(&self) -> &'static str;
    /// Called before a sale is committed.
    fn validate_sale(&self, cfg: &Config, sale: &Sale) -> Result<(), FiscalRejection>;
    fn validate_refund(&self, cfg: &Config, refund: &Refund) -> Result<(), FiscalRejection>;
    fn validate_config(&self, cfg: &Config) -> Result<(), FiscalRejection>;
    /// Lines printed under the store header on every receipt.
    fn receipt_header(&self, cfg: &Config) -> Vec<String>;
    /// Lines printed after the totals (e.g. fiscal codes).
    fn receipt_trailer(&self, cfg: &Config, sale: &Sale) -> Vec<String>;
    /// Extra Z/X report lines.
    fn report_trailer(&self, cfg: &Config, report: &Report) -> Vec<String>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct GenericPolicy;

impl FiscalPolicy for GenericPolicy {
    fn name(&self) -> &'static str {
        "generic"
    }
    fn validate_sale(&self, _cfg: &Config, sale: &Sale) -> Result<(), FiscalRejection> {
        if sale.lines.is_empty() {
            return Err(FiscalRejection("empty sale".into()));
        }
        Ok(())
    }
    fn validate_refund(&self, _cfg: &Config, r: &Refund) -> Result<(), FiscalRejection> {
        if r.reason.trim().is_empty() {
            return Err(FiscalRejection("refund reason required".into()));
        }
        Ok(())
    }
    fn validate_config(&self, cfg: &Config) -> Result<(), FiscalRejection> {
        cfg.validate().map_err(|e| FiscalRejection(e.into()))
    }
    fn receipt_header(&self, cfg: &Config) -> Vec<String> {
        alloc::vec![alloc::format!("TAX ID: {}", cfg.tax_id)]
    }
    fn receipt_trailer(&self, _cfg: &Config, _sale: &Sale) -> Vec<String> {
        Vec::new()
    }
    fn report_trailer(&self, _cfg: &Config, _r: &Report) -> Vec<String> {
        Vec::new()
    }
}
