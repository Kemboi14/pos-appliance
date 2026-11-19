//! `SETTINGS.TXT`: `key = value` lines, `#` comments. Keys that may repeat
//! (`address`, `footer`, `tax_class`) replace the whole list when present.
//!
//! ```text
//! store_name = CORNER SHOP
//! address = 12 Market Road
//! tax_id = P051234567X
//! currency = KES
//! decimals = 2
//! rounding = half_up            # half_up | half_even | down
//! tax_mode = inclusive          # inclusive | exclusive
//! tax_calc = per_line           # per_line | per_invoice
//! tax_class = 0,A,Standard,16   # id,code,name,percent
//! tax_class = 1,Z,Zero,0
//! cash_increment = 5            # minor units; 1 = off
//! cash_rounding = half_up
//! price_embed = price           # off | price | weight
//! price_embed_prefixes = 20-29
//! max_pin_failures = 5
//! scanner_max_gap_ms = 30
//! scanner_min_len = 6
//! printer_columns = 42
//! refund_window = 2000
//! snapshot_every = 256
//! footer = Thank you!
//! ```

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use pos_core::barcode::{EmbedKind, PriceEmbed};
use pos_core::config::{Config, TaxCalc, TaxClass, TaxClassId, TaxMode};
use pos_core::csv::parse_money;
use pos_core::{Rate, Rounding};

fn rounding(v: &str) -> Option<Rounding> {
    match v {
        "half_up" => Some(Rounding::HalfUp),
        "half_even" | "bankers" => Some(Rounding::HalfEven),
        "down" => Some(Rounding::Down),
        _ => None,
    }
}

pub fn parse(text: &str, base: &Config) -> Result<Config, String> {
    let mut c = base.clone();
    let mut address: Option<Vec<String>> = None;
    let mut footer: Option<Vec<String>> = None;
    let mut taxes: Option<Vec<TaxClass>> = None;
    for (i, raw) in text.lines().enumerate() {
        let ln = i + 1;
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let (k, v) = line
            .split_once('=')
            .ok_or_else(|| format!("line {ln}: expected key = value"))?;
        let (k, v) = (k.trim(), v.trim());
        let bad = || format!("line {ln}: bad value for {k}");
        let num = |v: &str| v.parse::<u32>().map_err(|_| bad());
        match k {
            "store_name" => c.store_name = v.into(),
            "address" => address.get_or_insert_with(Vec::new).push(v.into()),
            "footer" => footer.get_or_insert_with(Vec::new).push(v.into()),
            "tax_id" => c.tax_id = v.into(),
            "currency" => c.currency = v.into(),
            "decimals" => c.decimals = u8::try_from(num(v)?).map_err(|_| bad())?,
            "rounding" => c.rounding = rounding(v).ok_or_else(bad)?,
            "cash_rounding" => c.cash_rounding = rounding(v).ok_or_else(bad)?,
            "tax_mode" => {
                c.tax_mode = match v {
                    "inclusive" => TaxMode::Inclusive,
                    "exclusive" => TaxMode::Exclusive,
                    _ => return Err(bad()),
                }
            }
            "tax_calc" => {
                c.tax_calc = match v {
                    "per_line" => TaxCalc::PerLine,
                    "per_invoice" => TaxCalc::PerInvoice,
                    _ => return Err(bad()),
                }
            }
            "tax_class" => {
                let f: Vec<&str> = v.split(',').map(str::trim).collect();
                if f.len() != 4 || f[1].chars().count() != 1 {
                    return Err(bad());
                }
                let id = u8::try_from(num(f[0])?).map_err(|_| bad())?;
                // Percent with up to 4 decimals: 7.5 → 75000, which is
                // exactly ppm (1 % = 10 000 ppm).
                let pct = parse_money(f[3], 4).ok_or_else(bad)?;
                let ppm = u32::try_from(pct.0).map_err(|_| bad())?;
                taxes.get_or_insert_with(Vec::new).push(TaxClass {
                    id: TaxClassId(id),
                    code: f[1].chars().next().unwrap_or('?'),
                    name: f[2].into(),
                    rate: Rate(ppm),
                });
            }
            "cash_increment" => c.cash_increment = i64::from(num(v)?),
            "price_embed" => {
                c.price_embed = match v {
                    "off" => None,
                    "price" | "weight" => {
                        let mut p = c.price_embed.clone().unwrap_or_default();
                        p.kind = if v == "price" {
                            EmbedKind::Price
                        } else {
                            EmbedKind::Weight
                        };
                        Some(p)
                    }
                    _ => return Err(bad()),
                }
            }
            "price_embed_prefixes" => {
                let (a, b) = v.split_once('-').unwrap_or((v, v));
                let (a, b) = (num(a.trim())?, num(b.trim())?);
                if a > b || b > 99 {
                    return Err(bad());
                }
                let mut p = c.price_embed.clone().unwrap_or_default();
                p.prefixes = (a as u8..=b as u8).collect();
                c.price_embed = Some(p);
            }
            "max_pin_failures" => c.max_pin_failures = u8::try_from(num(v)?).map_err(|_| bad())?,
            "scanner_max_gap_ms" => {
                c.scanner_max_gap_ms = u16::try_from(num(v)?).map_err(|_| bad())?
            }
            "scanner_min_len" => c.scanner_min_len = u8::try_from(num(v)?).map_err(|_| bad())?,
            "printer_columns" => c.printer_columns = u8::try_from(num(v)?).map_err(|_| bad())?,
            "refund_window" => c.refund_window = num(v)?,
            "snapshot_every" => c.snapshot_every = num(v)?,
            _ => return Err(format!("line {ln}: unknown key {k}")),
        }
    }
    if let Some(a) = address {
        c.address = a;
    }
    if let Some(f) = footer {
        c.receipt_footer = f;
    }
    if let Some(t) = taxes {
        c.tax_classes = t;
    }
    let _ = PriceEmbed::default;
    c.validate().map_err(|e| format!("invalid settings: {e}"))?;
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_example() {
        let t = "store_name = CORNER SHOP\naddress = A\naddress = B\ntax_class = 0,A,Std,7.5\n\
                 tax_class = 1,Z,Zero,0\ntax_mode = exclusive\ncash_increment = 5\nprice_embed = weight\n";
        let c = parse(t, &Config::default()).unwrap();
        assert_eq!(c.store_name, "CORNER SHOP");
        assert_eq!(c.address.len(), 2);
        assert_eq!(c.tax_classes[0].rate, Rate(75_000));
        assert_eq!(c.tax_mode, TaxMode::Exclusive);
        assert_eq!(c.price_embed.unwrap().kind, EmbedKind::Weight);
        assert!(parse("bogus = 1", &Config::default()).is_err());
        assert!(parse("decimals = 9", &Config::default()).is_err());
    }
}
