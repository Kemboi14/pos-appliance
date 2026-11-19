//! Worked tax/discount/rounding examples. Each case documents the arithmetic.

mod common;
use common::*;

use pos_core::cart::{Cart, Discount, Line};
use pos_core::config::{Config, TaxCalc, TaxClass, TaxClassId, TaxMode};
use pos_core::tender::{Payment, Tender, TenderKind};
use pos_core::{Money, Qty, Rate, Rounding};

fn cfg(mode: TaxMode, calc: TaxCalc, rounding: Rounding, rate: Rate) -> Config {
    Config {
        tax_mode: mode,
        tax_calc: calc,
        rounding,
        tax_classes: vec![
            TaxClass {
                id: TaxClassId(0),
                code: 'A',
                name: "Std".into(),
                rate,
            },
            TaxClass {
                id: TaxClassId(1),
                code: 'Z',
                name: "Zero".into(),
                rate: Rate::ZERO,
            },
        ],
        ..Config::default()
    }
}

fn line(sku: &str, price: i64, qty: Qty, tax: u8) -> Line {
    Line::from_item(&item(sku, price, tax), qty)
}

#[test]
fn inclusive_16pct_exact_and_rounded() {
    let c = cfg(
        TaxMode::Inclusive,
        TaxCalc::PerLine,
        Rounding::HalfUp,
        Rate::percent(16),
    );
    let mut cart = Cart::default();
    // 116.00 incl. 16 %: tax = 11600 × 16/116 = 1600 exactly.
    cart.add(line("A", 116_00, Qty::ONE, 0)).unwrap();
    // 3 × 10.00 = 30.00: tax = 3000 × 16/116 = 413.79… → 414.
    cart.add(line("B", 10_00, Qty::units(3), 0)).unwrap();
    let p = cart.price(&c).unwrap();
    assert_eq!(p.lines[0].tax, Money(1600));
    assert_eq!(p.lines[1].tax, Money(414));
    assert_eq!(p.totals.tax, Money(2014));
    assert_eq!(p.totals.total, Money(146_00)); // inclusive: total = gross
}

#[test]
fn per_line_vs_per_invoice_inclusive() {
    // Three different 1.00 items at 16 % inclusive.
    // Per line:    100 × 16/116 = 13.79 → 14, ×3 = 42.
    // Per invoice: 300 × 16/116 = 41.38 → 41.
    for (calc, want) in [(TaxCalc::PerLine, 42), (TaxCalc::PerInvoice, 41)] {
        let c = cfg(
            TaxMode::Inclusive,
            calc,
            Rounding::HalfUp,
            Rate::percent(16),
        );
        let mut cart = Cart::default();
        for s in ["X", "Y", "Z"] {
            cart.add(line(s, 1_00, Qty::ONE, 0)).unwrap();
        }
        let p = cart.price(&c).unwrap();
        assert_eq!(p.totals.tax, Money(want), "{calc:?}");
        assert_eq!(p.lines.iter().map(|l| l.tax.0).sum::<i64>(), want);
        assert_eq!(p.totals.total, Money(300));
    }
}

#[test]
fn exclusive_8875_us_style() {
    // 1.99 + 2.49 at 8.875 % exclusive.
    // Per line:    199 × .08875 = 17.66 → 18; 249 × .08875 = 22.10 → 22; = 40.
    // Per invoice: 448 × .08875 = 39.76 → 40.
    for calc in [TaxCalc::PerLine, TaxCalc::PerInvoice] {
        let c = cfg(TaxMode::Exclusive, calc, Rounding::HalfUp, Rate(88_750));
        let mut cart = Cart::default();
        cart.add(line("P", 1_99, Qty::ONE, 0)).unwrap();
        cart.add(line("Q", 2_49, Qty::ONE, 0)).unwrap();
        let p = cart.price(&c).unwrap();
        assert_eq!(p.totals.tax, Money(40));
        assert_eq!(p.totals.total, Money(488));
    }
}

#[test]
fn half_up_vs_bankers() {
    // Three 0.05 items at 10 % exclusive: per-line tax is exactly 0.5 cent.
    // HalfUp per line: 1+1+1 = 3.  HalfEven per line: 0+0+0 = 0.
    // Per invoice: 15 × 10 % = 1.5 → HalfUp 2, HalfEven 2.
    let cases = [
        (TaxCalc::PerLine, Rounding::HalfUp, 3),
        (TaxCalc::PerLine, Rounding::HalfEven, 0),
        (TaxCalc::PerInvoice, Rounding::HalfUp, 2),
        (TaxCalc::PerInvoice, Rounding::HalfEven, 2),
    ];
    for (calc, r, want) in cases {
        let c = cfg(TaxMode::Exclusive, calc, r, Rate::percent(10));
        let mut cart = Cart::default();
        for s in ["X", "Y", "Z"] {
            cart.add(line(s, 5, Qty::ONE, 0)).unwrap();
        }
        assert_eq!(
            cart.price(&c).unwrap().totals.tax,
            Money(want),
            "{calc:?} {r:?}"
        );
    }
}

#[test]
fn basket_discount_allocation_is_exact() {
    // 10 % off 3.33 + 3.33 + 3.34 = 10.00 → discount 1.00.
    // Proportional shares 33.3, 33.3, 33.4 → floors 33,33,33, 1 cent left goes
    // to the largest remainder (line 3): 33, 33, 34.
    let c = cfg(
        TaxMode::Inclusive,
        TaxCalc::PerLine,
        Rounding::HalfUp,
        Rate::ZERO,
    );
    let mut cart = Cart::default();
    cart.add(line("X", 3_33, Qty::ONE, 1)).unwrap();
    cart.add(line("Y", 3_33, Qty::ONE, 1)).unwrap();
    cart.add(line("Z", 3_34, Qty::ONE, 1)).unwrap();
    cart.basket_discount = Some(Discount::Percent(Rate::percent(10)));
    let p = cart.price(&c).unwrap();
    let shares: Vec<i64> = p.lines.iter().map(|l| l.basket_discount.0).collect();
    assert_eq!(shares, [33, 33, 34]);
    assert_eq!(p.totals.total, Money(9_00));
    assert_eq!(p.totals.discount, Money(1_00));
}

#[test]
fn fixed_discount_clamped_to_line() {
    let c = cfg(
        TaxMode::Inclusive,
        TaxCalc::PerLine,
        Rounding::HalfUp,
        Rate::percent(16),
    );
    let mut cart = Cart::default();
    let mut l = line("X", 5_00, Qty::ONE, 0);
    l.discount = Some(Discount::Fixed(Money(9_00)));
    cart.add(l).unwrap();
    let p = cart.price(&c).unwrap();
    assert_eq!(p.lines[0].line_discount, Money(5_00));
    assert_eq!(p.totals.total, Money(0));
    assert_eq!(p.totals.tax, Money(0));
}

#[test]
fn weighable_quantity() {
    // 1.234 kg at 4.99/kg = 6.15766 → 6.16.
    let c = cfg(
        TaxMode::Inclusive,
        TaxCalc::PerLine,
        Rounding::HalfUp,
        Rate::ZERO,
    );
    let mut cart = Cart::default();
    cart.add(line("W", 4_99, Qty(1234), 1)).unwrap();
    assert_eq!(cart.price(&c).unwrap().totals.total, Money(6_16));
}

#[test]
fn cash_rounding_and_change() {
    let mut c = cfg(
        TaxMode::Inclusive,
        TaxCalc::PerLine,
        Rounding::HalfUp,
        Rate::ZERO,
    );
    c.cash_increment = 5;
    // Due 10.02: cash rounds to 10.00 (−0.02), customer gives 20.00, change 10.00.
    let mut p = Payment::new(Money(10_02));
    assert_eq!(p.cash_due(&c).unwrap(), Money(10_00));
    p.add(
        Tender {
            kind: TenderKind::Cash,
            amount: Money(20_00),
            reference: String::new(),
        },
        &c,
    )
    .unwrap();
    assert!(p.is_complete());
    assert_eq!(p.rounding, Money(-2));
    assert_eq!(p.change, Money(10_00));
}

#[test]
fn split_tender_card_then_cash() {
    let mut c = cfg(
        TaxMode::Inclusive,
        TaxCalc::PerLine,
        Rounding::HalfUp,
        Rate::ZERO,
    );
    c.cash_increment = 5;
    let mut p = Payment::new(Money(25_03));
    let card = |amt| Tender {
        kind: TenderKind::Card,
        amount: Money(amt),
        reference: "4242/A1B2C3".into(),
    };
    // Card cannot exceed the balance.
    assert!(p.add(card(30_00), &c).is_err());
    p.add(card(20_00), &c).unwrap();
    assert_eq!(p.remaining().unwrap(), Money(5_03));
    // Partial cash does not round; final cash does: 5.03 → 5.05.
    p.add(
        Tender {
            kind: TenderKind::Cash,
            amount: Money(1_00),
            reference: String::new(),
        },
        &c,
    )
    .unwrap();
    assert_eq!(p.rounding, Money(0));
    p.add(
        Tender {
            kind: TenderKind::Cash,
            amount: Money(5_00),
            reference: String::new(),
        },
        &c,
    )
    .unwrap();
    // Owed 25.05 after rounding; paid 20.00 + 1.00 + 5.00 = 26.00.
    assert_eq!(p.rounding, Money(2));
    assert_eq!(p.change, Money(95));
    assert!(p.is_complete());
    // Full PAN is rejected.
    let mut p2 = Payment::new(Money(1_00));
    let bad = Tender {
        kind: TenderKind::Card,
        amount: Money(1_00),
        reference: "4242424242424242/X".into(),
    };
    assert!(p2.add(bad, &c).is_err());
}
