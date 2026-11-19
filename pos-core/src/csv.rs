//! CSV import/export (RFC 4180 subset: comma separator, `"` quoting, CRLF or
//! LF line endings). Pure formatting/parsing; I/O lives in the platform.

use crate::catalog::Item;
use crate::config::{Config, TaxClassId};
use crate::money::Money;
use crate::shift::Report;
use crate::state::Event;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt::Write;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvError {
    /// 1-based line number in the file.
    pub line: usize,
    pub msg: String,
}

fn err(line: usize, msg: &str) -> CsvError {
    CsvError {
        line,
        msg: msg.into(),
    }
}

/// Parse CSV text into records. Returns (line number, fields) per record.
pub fn parse(text: &str) -> Result<Vec<(usize, Vec<String>)>, CsvError> {
    let mut out = Vec::new();
    let mut rec: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_q = false;
    let mut line = 1usize;
    let mut rec_line = 1usize;
    let mut chars = text
        .strip_prefix('\u{feff}')
        .unwrap_or(text)
        .chars()
        .peekable();
    let mut any = false;
    while let Some(c) = chars.next() {
        if in_q {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    field.push('"');
                }
                '"' => in_q = false,
                '\n' => {
                    line += 1;
                    field.push('\n');
                }
                _ => field.push(c),
            }
            continue;
        }
        match c {
            '"' if field.is_empty() => {
                in_q = true;
                any = true;
            }
            '"' => return Err(err(line, "quote inside unquoted field")),
            ',' => {
                rec.push(core::mem::take(&mut field));
                any = true;
            }
            '\r' if chars.peek() == Some(&'\n') => {}
            '\n' => {
                if any || !field.is_empty() {
                    rec.push(core::mem::take(&mut field));
                    out.push((rec_line, core::mem::take(&mut rec)));
                }
                any = false;
                line += 1;
                rec_line = line;
            }
            _ => {
                field.push(c);
                any = true;
            }
        }
    }
    if in_q {
        return Err(err(line, "unterminated quote"));
    }
    if any || !field.is_empty() {
        rec.push(field);
        out.push((rec_line, rec));
    }
    Ok(out)
}

/// Quote a field if needed. Fields starting with `=`, `+`, `-`, `@` are
/// prefixed with `'` to defuse spreadsheet formula injection.
pub fn field(s: &str) -> String {
    let s = if s.starts_with(['=', '+', '@']) || (s.starts_with('-') && !is_number(s)) {
        alloc::format!("'{s}")
    } else {
        s.to_string()
    };
    if s.contains([',', '"', '\n', '\r']) {
        alloc::format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s
    }
}

fn is_number(s: &str) -> bool {
    let t = s.strip_prefix('-').unwrap_or(s);
    !t.is_empty() && t.bytes().all(|c| c.is_ascii_digit() || c == b'.')
}

/// Parse a decimal amount like `12.34`, `-0.5` or `7` into minor units.
pub fn parse_money(s: &str, decimals: u8) -> Option<Money> {
    let s = s.trim();
    let (neg, s) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s),
    };
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    if int.is_empty() && frac.is_empty() {
        return None;
    }
    if !int.bytes().all(|c| c.is_ascii_digit())
        || !frac.bytes().all(|c| c.is_ascii_digit())
        || frac.len() > usize::from(decimals)
    {
        return None;
    }
    let mut v: i64 = if int.is_empty() { 0 } else { int.parse().ok()? };
    for _ in 0..decimals {
        v = v.checked_mul(10)?;
    }
    let mut f: i64 = 0;
    for i in 0..usize::from(decimals) {
        f = f * 10 + frac.as_bytes().get(i).map_or(0, |c| i64::from(c - b'0'));
    }
    let v = v.checked_add(f)?;
    Some(Money(if neg { -v } else { v }))
}

pub const CATALOG_HEADER: &str = "sku,barcode,plu,name,price,tax_code,weighable,quick_key";

/// Parse a catalog CSV. The whole file is validated; the first error aborts
/// the import (the caller then commits nothing).
pub fn catalog_from_csv(text: &str, cfg: &Config) -> Result<Vec<Item>, CsvError> {
    let recs = parse(text)?;
    let mut it = recs.into_iter();
    let (hl, header) = it.next().ok_or_else(|| err(1, "empty file"))?;
    let want: Vec<&str> = CATALOG_HEADER.split(',').collect();
    let got: Vec<String> = header
        .iter()
        .map(|h| h.trim().to_ascii_lowercase())
        .collect();
    if got != want {
        return Err(err(
            hl,
            "header must be: sku,barcode,plu,name,price,tax_code,weighable,quick_key",
        ));
    }
    let mut items = Vec::new();
    for (ln, r) in it {
        if r.len() != want.len() {
            return Err(err(ln, "wrong number of fields"));
        }
        let opt = |s: &str| {
            let t = s.trim();
            (!t.is_empty()).then(|| String::from(t))
        };
        let price = parse_money(&r[4], cfg.decimals).ok_or_else(|| err(ln, "bad price"))?;
        let code = r[5].trim();
        let tax = cfg
            .tax_classes
            .iter()
            .find(|t| t.code.to_string().eq_ignore_ascii_case(code))
            .map(|t| t.id)
            .ok_or_else(|| err(ln, "unknown tax code"))?;
        let weighable = match r[6].trim().to_ascii_lowercase().as_str() {
            "" | "0" | "n" | "no" | "false" => false,
            "1" | "y" | "yes" | "true" => true,
            _ => return Err(err(ln, "bad weighable flag")),
        };
        let quick_key = match r[7].trim() {
            "" => None,
            q => Some(q.parse::<u8>().map_err(|_| err(ln, "bad quick key"))?),
        };
        let item = Item {
            sku: r[0].trim().into(),
            barcode: opt(&r[1]),
            plu: opt(&r[2]),
            name: r[3].trim().into(),
            price,
            tax_class: tax,
            weighable,
            quick_key,
        };
        items.push(item);
    }
    // Full validation (duplicates, barcodes) exactly as the event will see it.
    crate::catalog::Catalog::from_items(&items, cfg)
        .map_err(|(i, e)| err(i + 2, &alloc::format!("{e:?}")))?;
    Ok(items)
}

fn tax_code(cfg: &Config, id: TaxClassId) -> char {
    cfg.tax_class(id).map_or('?', |t| t.code)
}

pub fn catalog_to_csv(items: &[&Item], cfg: &Config) -> String {
    let mut s = String::from(CATALOG_HEADER);
    s.push_str("\r\n");
    for i in items {
        let _ = write!(
            s,
            "{},{},{},{},{},{},{},{}\r\n",
            field(&i.sku),
            field(i.barcode.as_deref().unwrap_or("")),
            field(i.plu.as_deref().unwrap_or("")),
            field(&i.name),
            i.price.display(cfg.decimals),
            tax_code(cfg, i.tax_class),
            u8::from(i.weighable),
            i.quick_key.map(|q| q.to_string()).unwrap_or_default()
        );
    }
    s
}

pub fn report_to_csv(r: &Report, cfg: &Config) -> String {
    let d = cfg.decimals;
    let c = &r.shift.counters;
    let mut s = String::from("field,value\r\n");
    let mut kv = |k: &str, v: String| {
        let _ = write!(s, "{},{}\r\n", field(k), field(&v));
    };
    kv("report", alloc::format!("{:?}", r.kind));
    kv("z_no", r.z_no.map(|z| z.to_string()).unwrap_or_default());
    kv("at_unix", r.at.to_string());
    kv("shift", r.shift.id.to_string());
    kv("opened_at_unix", r.shift.opened_at.to_string());
    kv(
        "first_receipt",
        c.first_receipt.map(|x| x.to_string()).unwrap_or_default(),
    );
    kv(
        "last_receipt",
        c.last_receipt.map(|x| x.to_string()).unwrap_or_default(),
    );
    kv("sales_count", c.sales.to_string());
    kv("refunds_count", c.refunds.to_string());
    kv("gross", c.gross.display(d).to_string());
    kv("discounts", c.discounts.display(d).to_string());
    kv("sales_total", c.sales_total.display(d).to_string());
    kv("refunds_total", c.refunds_total.display(d).to_string());
    kv("net", c.net().unwrap_or(Money::ZERO).display(d).to_string());
    for (id, t) in &c.taxes {
        let code = tax_code(cfg, *id);
        kv(&alloc::format!("tax_{code}_rate"), t.rate.to_string());
        kv(
            &alloc::format!("tax_{code}_base"),
            t.base.display(d).to_string(),
        );
        kv(
            &alloc::format!("tax_{code}_tax"),
            t.tax.display(d).to_string(),
        );
    }
    kv("refund_tax", c.refund_tax.display(d).to_string());
    for (k, v) in &c.tenders {
        kv(
            &alloc::format!("tender_{}", k.label().to_ascii_lowercase()),
            v.display(d).to_string(),
        );
    }
    for (k, v) in &c.payouts {
        kv(
            &alloc::format!("payout_{}", k.label().to_ascii_lowercase()),
            v.display(d).to_string(),
        );
    }
    kv("change_given", c.change_given.display(d).to_string());
    kv("cash_rounding", c.cash_rounding.display(d).to_string());
    kv("float", r.shift.float.display(d).to_string());
    kv("expected_cash", r.expected_cash.display(d).to_string());
    kv(
        "counted_cash",
        r.counted_cash
            .map(|m| m.display(d).to_string())
            .unwrap_or_default(),
    );
    kv("line_voids", c.line_voids.to_string());
    kv("sale_voids", c.sale_voids.to_string());
    kv("no_sales", c.no_sales.to_string());
    kv("reprints", c.reprints.to_string());
    kv("overrides", c.overrides.to_string());
    kv("grand_total", r.grand_total.display(d).to_string());
    s
}

pub const JOURNAL_HEADER: &str = "seq,hash,at_unix,kind,receipt,user,amount,detail";

/// One journal row per event; `hash` is the record's hash-chain value.
pub fn journal_row(seq: u64, hash_hex: &str, e: &Event, cfg: &Config) -> String {
    let d = cfg.decimals;
    let (receipt, user, amount, detail): (String, String, String, String) = match e {
        Event::SaleCompleted(s) => (
            s.receipt_no.to_string(),
            s.cashier.to_string(),
            s.totals.total.display(d).to_string(),
            alloc::format!(
                "lines={} tax={} tenders={}",
                s.lines.len(),
                s.totals.tax.display(d),
                s.tenders
                    .iter()
                    .map(|t| alloc::format!("{}:{}", t.kind.label(), t.amount.display(d)))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
        ),
        Event::RefundCompleted(r) => (
            r.receipt_no.to_string(),
            r.cashier.to_string(),
            r.total.display(d).to_string(),
            alloc::format!(
                "orig={} auth={} reason={}",
                r.original_receipt,
                r.authorized_by,
                r.reason
            ),
        ),
        Event::Report(r) => (
            r.z_no.map(|z| z.to_string()).unwrap_or_default(),
            r.by.to_string(),
            r.shift
                .counters
                .net()
                .unwrap_or(Money::ZERO)
                .display(d)
                .to_string(),
            alloc::format!("shift={}", r.shift.id),
        ),
        Event::LineVoided {
            by,
            authorized_by,
            sku,
            amount,
            ..
        } => (
            String::new(),
            by.to_string(),
            amount.display(d).to_string(),
            alloc::format!("sku={sku} auth={authorized_by}"),
        ),
        Event::SaleVoided {
            by,
            authorized_by,
            amount,
            lines,
            ..
        } => (
            String::new(),
            by.to_string(),
            amount.display(d).to_string(),
            alloc::format!("lines={lines} auth={authorized_by}"),
        ),
        Event::Override {
            cashier,
            manager,
            permission,
            ..
        } => (
            String::new(),
            cashier.to_string(),
            String::new(),
            alloc::format!("{} by {manager}", permission.label()),
        ),
        Event::ShiftOpened {
            shift_id,
            by,
            float,
            ..
        } => (
            String::new(),
            by.to_string(),
            float.display(d).to_string(),
            alloc::format!("shift={shift_id}"),
        ),
        Event::Reprint { by, receipt_no, .. } => (
            receipt_no.to_string(),
            by.to_string(),
            String::new(),
            String::new(),
        ),
        Event::LoginOk { user, .. }
        | Event::LoginFailed { user, .. }
        | Event::Logout { user, .. } => (
            String::new(),
            user.to_string(),
            String::new(),
            String::new(),
        ),
        Event::UserUnlocked { user, by, .. } => (
            String::new(),
            by.to_string(),
            String::new(),
            alloc::format!("user={user}"),
        ),
        Event::UserSet { by, user, .. } => (
            String::new(),
            by.map(|b| b.to_string()).unwrap_or_default(),
            String::new(),
            alloc::format!(
                "id={} name={} role={:?} active={}",
                user.id,
                user.name,
                user.role,
                user.active
            ),
        ),
        Event::CatalogUpsert { by, item, .. } => (
            String::new(),
            by.to_string(),
            item.price.display(d).to_string(),
            alloc::format!("sku={} name={}", item.sku, item.name),
        ),
        Event::CatalogDelete { by, sku, .. } => (
            String::new(),
            by.to_string(),
            String::new(),
            alloc::format!("sku={sku}"),
        ),
        Event::CatalogReplace {
            by, items, source, ..
        } => (
            String::new(),
            by.to_string(),
            String::new(),
            alloc::format!("items={} source={source}", items.len()),
        ),
        Event::ConfigSet { by, .. } => {
            (String::new(), by.to_string(), String::new(), String::new())
        }
        Event::NoSale { by, .. } => (String::new(), by.to_string(), String::new(), String::new()),
        Event::Exported {
            by, what, bytes, ..
        } => (
            String::new(),
            by.to_string(),
            String::new(),
            alloc::format!("{what} bytes={bytes}"),
        ),
    };
    alloc::format!(
        "{},{},{},{},{},{},{},{}\r\n",
        seq,
        hash_hex,
        e.at(),
        e.kind(),
        receipt,
        user,
        field(&amount),
        field(&detail)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_quoted() {
        let r = parse("a,\"b,c\",\"d\"\"e\"\r\n\n1,2,3").unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].1, ["a", "b,c", "d\"e"]);
        assert_eq!(r[1].0, 3);
        assert!(parse("\"abc").is_err());
    }

    #[test]
    fn money_parse() {
        assert_eq!(parse_money("12.34", 2), Some(Money(1234)));
        assert_eq!(parse_money("12.3", 2), Some(Money(1230)));
        assert_eq!(parse_money("-0.05", 2), Some(Money(-5)));
        assert_eq!(parse_money("7", 0), Some(Money(7)));
        assert_eq!(parse_money("1.234", 2), None);
        assert_eq!(parse_money("1e3", 2), None);
        assert_eq!(parse_money("", 2), None);
        assert_eq!(parse_money("99999999999999999999", 2), None);
    }

    #[test]
    fn formula_injection_defused() {
        assert_eq!(field("=CMD()"), "'=CMD()");
        assert_eq!(field("-12.50"), "-12.50");
        assert_eq!(field("a,b"), "\"a,b\"");
    }
}
