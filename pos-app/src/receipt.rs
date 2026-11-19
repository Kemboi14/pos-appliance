//! Receipt and report rendering to ESC/POS.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use pos_core::config::Config;
use pos_core::fiscal::FiscalPolicy;
use pos_core::money::Money;
use pos_core::sale::{Refund, Sale};
use pos_core::shift::{Report, ReportKind};
use pos_core::tender::TenderKind;
use pos_escpos::{Align, CodePage, Cut, Encoder, QrEcc, two_col};
use pos_hal::DateTime;

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn fmt_time(t: i64) -> String {
    match DateTime::from_unix(t) {
        Some(d) => format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            d.year, d.month, d.day, d.hour, d.minute, d.second
        ),
        None => String::from("----"),
    }
}

fn m(cfg: &Config, v: Money) -> String {
    format!("{}", v.display(cfg.decimals))
}

fn header(e: &mut Encoder, cfg: &Config, policy: &dyn FiscalPolicy) {
    let w = usize::from(cfg.printer_columns);
    e.align(Align::Center).bold(true).size(2, 2);
    e.line(&cfg.store_name.chars().take(w / 2).collect::<String>());
    e.size(1, 1).bold(false);
    for a in &cfg.address {
        e.line(a);
    }
    for l in policy.receipt_header(cfg) {
        e.line(&l);
    }
    e.align(Align::Left);
}

fn rule(e: &mut Encoder, cfg: &Config) {
    let s: String = core::iter::repeat_n('-', usize::from(cfg.printer_columns)).collect();
    e.line(&s);
}

/// Customer receipt. `copy` marks reprints; `hash` is the journal record's
/// chain hash (its first 16 hex digits are printed and encoded in the QR).
pub fn sale_receipt(
    cfg: &Config,
    policy: &dyn FiscalPolicy,
    s: &Sale,
    cashier: &str,
    hash: &[u8; 32],
    copy: bool,
) -> Vec<u8> {
    let w = usize::from(cfg.printer_columns);
    let mut e = Encoder::new(CodePage::Cp858);
    header(&mut e, cfg, policy);
    if copy {
        e.align(Align::Center)
            .invert(true)
            .bold(true)
            .line(" COPY ")
            .invert(false)
            .bold(false);
        e.align(Align::Left);
    }
    e.line(&two_col(
        &format!("Receipt #{}", s.receipt_no),
        &fmt_time(s.at),
        w,
    ));
    e.line(&format!("Cashier: {cashier}  Shift {}", s.shift_id));
    rule(&mut e, cfg);
    for l in &s.lines {
        let code = cfg.tax_class(l.line.tax_class).map_or('?', |t| t.code);
        e.line(&two_col(
            &l.line.name,
            &format!("{} {code}", m(cfg, l.priced.gross)),
            w,
        ));
        if l.line.qty != pos_core::Qty::ONE || l.line.fixed_amount.is_some() {
            e.line(&format!("  {} x {}", l.line.qty, m(cfg, l.line.unit_price)));
        }
        let disc = l.priced.line_discount;
        if !disc.is_zero() {
            e.line(&two_col("  Discount", &format!("-{}", m(cfg, disc)), w));
        }
        if l.line.price_override {
            e.line("  (price override)");
        }
    }
    rule(&mut e, cfg);
    let t = &s.totals;
    e.line(&two_col("Subtotal", &m(cfg, t.subtotal), w));
    if !t.discount.is_zero() {
        e.line(&two_col(
            "Discounts",
            &format!("-{}", m(cfg, t.discount)),
            w,
        ));
    }
    for tl in &t.taxes {
        let code = cfg.tax_class(tl.class).map_or('?', |x| x.code);
        let label = match cfg.tax_mode {
            pos_core::config::TaxMode::Inclusive => {
                format!("{code} {} incl. on {}", tl.rate, m(cfg, tl.base))
            }
            pos_core::config::TaxMode::Exclusive => {
                format!("{code} {} on {}", tl.rate, m(cfg, tl.base))
            }
        };
        e.line(&two_col(&label, &m(cfg, tl.tax), w));
    }
    e.bold(true).size(1, 2);
    e.line(&two_col(
        &format!("TOTAL {}", cfg.currency),
        &m(cfg, t.total),
        w,
    ));
    e.size(1, 1).bold(false);
    if !s.rounding.is_zero() {
        e.line(&two_col("Cash rounding", &m(cfg, s.rounding), w));
    }
    for tn in &s.tenders {
        let label = match tn.kind {
            TenderKind::Cash => String::from("Cash"),
            TenderKind::Card => format!("Card {}", tn.reference),
            TenderKind::Voucher => format!("Voucher {}", tn.reference),
        };
        e.line(&two_col(&label, &m(cfg, tn.amount), w));
    }
    if !s.change.is_zero() {
        e.line(&two_col("Change", &m(cfg, s.change), w));
    }
    for l in policy.receipt_trailer(cfg, s) {
        e.line(&l);
    }
    rule(&mut e, cfg);
    e.align(Align::Center);
    for f in &cfg.receipt_footer {
        e.line(f);
    }
    let h = hex(&hash[..8]);
    e.line(&format!("Journal {h}"));
    let qr = format!("R{}|{}|{}|{}", s.receipt_no, m(cfg, t.total), s.at, h);
    let _ = e.qr(qr.as_bytes(), 4, QrEcc::M);
    e.feed(3).cut(Cut::Partial, 0);
    e.finish()
}

pub fn refund_receipt(
    cfg: &Config,
    policy: &dyn FiscalPolicy,
    r: &Refund,
    cashier: &str,
    hash: &[u8; 32],
    copy: bool,
) -> Vec<u8> {
    let w = usize::from(cfg.printer_columns);
    let mut e = Encoder::new(CodePage::Cp858);
    header(&mut e, cfg, policy);
    e.align(Align::Center).bold(true).line("REFUND").bold(false);
    if copy {
        e.invert(true).line(" COPY ").invert(false);
    }
    e.align(Align::Left);
    e.line(&two_col(
        &format!("Receipt #{}", r.receipt_no),
        &fmt_time(r.at),
        w,
    ));
    e.line(&format!(
        "Original #{}  Cashier: {cashier}",
        r.original_receipt
    ));
    e.line(&format!("Authorised by user {}", r.authorized_by));
    e.line(&format!("Reason: {}", r.reason));
    rule(&mut e, cfg);
    for l in &r.lines {
        e.line(&two_col(
            &format!("{} x {}", l.qty, l.sku),
            &format!("-{}", m(cfg, l.total)),
            w,
        ));
    }
    rule(&mut e, cfg);
    e.bold(true)
        .line(&two_col(
            "REFUND TOTAL",
            &format!("-{}", m(cfg, r.total)),
            w,
        ))
        .bold(false);
    e.line(&two_col("of which tax", &m(cfg, r.tax), w));
    for p in &r.payout {
        e.line(&two_col(
            &format!("Paid out {}", p.kind.label()),
            &m(cfg, p.amount),
            w,
        ));
    }
    e.align(Align::Center)
        .line(&format!("Journal {}", hex(&hash[..8])));
    e.line("Customer signature:");
    e.feed(2).line("______________________");
    e.feed(3).cut(Cut::Partial, 0);
    e.finish()
}

pub fn report_lines(cfg: &Config, policy: &dyn FiscalPolicy, r: &Report) -> Vec<String> {
    let w = usize::from(cfg.printer_columns);
    let c = &r.shift.counters;
    let mut v = Vec::new();
    let title = match r.kind {
        ReportKind::X => String::from("X REPORT (no reset)"),
        ReportKind::Z => format!("Z REPORT #{}", r.z_no.unwrap_or(0)),
    };
    v.push(title);
    v.push(format!("Printed {}", fmt_time(r.at)));
    v.push(format!(
        "Shift {} opened {}",
        r.shift.id,
        fmt_time(r.shift.opened_at)
    ));
    v.push(format!(
        "Receipts {} - {}",
        c.first_receipt
            .map_or(String::from("-"), |x| format!("{x}")),
        c.last_receipt.map_or(String::from("-"), |x| format!("{x}"))
    ));
    let mut kv = |k: &str, val: String| v.push(two_col(k, &val, w));
    kv("Sales", format!("{}", c.sales));
    kv("Gross", m(cfg, c.gross));
    kv("Discounts", m(cfg, c.discounts));
    kv("Sales total", m(cfg, c.sales_total));
    kv(
        "Refunds",
        format!("{} / {}", c.refunds, m(cfg, c.refunds_total)),
    );
    kv("NET", m(cfg, c.net().unwrap_or(Money::ZERO)));
    for (id, t) in &c.taxes {
        let code = cfg.tax_class(*id).map_or('?', |x| x.code);
        kv(&format!("Tax {code} {} base", t.rate), m(cfg, t.base));
        kv(&format!("Tax {code} {}", t.rate), m(cfg, t.tax));
    }
    kv("Refund tax", m(cfg, c.refund_tax));
    for k in [TenderKind::Cash, TenderKind::Card, TenderKind::Voucher] {
        kv(&format!("Tender {}", k.label()), m(cfg, c.tender(k)));
    }
    kv("Change given", m(cfg, c.change_given));
    kv("Cash rounding", m(cfg, c.cash_rounding));
    kv("Refund payouts cash", m(cfg, c.payout(TenderKind::Cash)));
    kv("Float", m(cfg, r.shift.float));
    kv("EXPECTED CASH", m(cfg, r.expected_cash));
    if let Some(cc) = r.counted_cash {
        kv("Counted cash", m(cfg, cc));
        kv(
            "Difference",
            m(cfg, cc.checked_sub(r.expected_cash).unwrap_or(Money::ZERO)),
        );
    }
    kv("Line voids", format!("{}", c.line_voids));
    kv("Sale voids", format!("{}", c.sale_voids));
    kv("No sales", format!("{}", c.no_sales));
    kv("Reprints", format!("{}", c.reprints));
    kv("Overrides", format!("{}", c.overrides));
    kv("GRAND TOTAL", m(cfg, r.grand_total));
    v.extend(policy.report_trailer(cfg, r));
    v
}

pub fn report_receipt(
    cfg: &Config,
    policy: &dyn FiscalPolicy,
    r: &Report,
    hash: Option<&[u8; 32]>,
) -> Vec<u8> {
    let mut e = Encoder::new(CodePage::Cp858);
    header(&mut e, cfg, policy);
    let lines = report_lines(cfg, policy, r);
    e.align(Align::Center)
        .bold(true)
        .line(&lines[0])
        .bold(false)
        .align(Align::Left);
    for l in &lines[1..] {
        e.line(l);
    }
    if let Some(h) = hash {
        e.align(Align::Center)
            .line(&format!("Journal {}", hex(&h[..8])));
    }
    e.feed(3).cut(Cut::Partial, 0);
    e.finish()
}

pub fn drawer_kick() -> Vec<u8> {
    let mut e = Encoder::new(CodePage::Cp858);
    e.drawer_kick();
    e.finish()
}
