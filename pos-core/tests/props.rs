//! Property tests for money/tax/rounding/refund/counter invariants.

mod common;
use common::*;

use pos_core::cart::{Cart, Discount, Line};
use pos_core::config::{Config, TaxCalc, TaxClass, TaxClassId, TaxMode};
use pos_core::money::{allocate, div_round};
use pos_core::shift::ReportKind;
use pos_core::state::{Event, PosState};
use pos_core::tender::{Payment, Tender, TenderKind};
use pos_core::{Money, Qty, Rate, Rounding};
use proptest::prelude::*;

fn rounding() -> impl Strategy<Value = Rounding> {
    prop_oneof![
        Just(Rounding::HalfUp),
        Just(Rounding::HalfEven),
        Just(Rounding::Down)
    ]
}

fn config() -> impl Strategy<Value = Config> {
    (
        any::<bool>(),
        any::<bool>(),
        rounding(),
        0u32..=250_000,
        0u32..=100_000,
        prop_oneof![Just(1i64), Just(5), Just(10), Just(50)],
    )
        .prop_map(|(incl, per_line, r, r1, r2, inc)| Config {
            tax_mode: if incl {
                TaxMode::Inclusive
            } else {
                TaxMode::Exclusive
            },
            tax_calc: if per_line {
                TaxCalc::PerLine
            } else {
                TaxCalc::PerInvoice
            },
            rounding: r,
            cash_increment: inc,
            tax_classes: vec![
                TaxClass {
                    id: TaxClassId(0),
                    code: 'A',
                    name: "A".into(),
                    rate: Rate(r1),
                },
                TaxClass {
                    id: TaxClassId(1),
                    code: 'B',
                    name: "B".into(),
                    rate: Rate(r2),
                },
                TaxClass {
                    id: TaxClassId(2),
                    code: 'Z',
                    name: "Z".into(),
                    rate: Rate::ZERO,
                },
            ],
            ..Config::default()
        })
}

fn discount() -> impl Strategy<Value = Option<Discount>> {
    prop_oneof![
        3 => Just(None),
        1 => (0u32..=1_000_000).prop_map(|r| Some(Discount::Percent(Rate(r)))),
        1 => (0i64..=50_000).prop_map(|m| Some(Discount::Fixed(Money(m)))),
    ]
}

fn line() -> impl Strategy<Value = Line> {
    (
        0i64..=10_000_000,
        1i64..=50_000,
        0u8..3,
        discount(),
        any::<bool>(),
        0u8..8,
    )
        .prop_map(|(price, q, tax, d, weighed, sku)| Line {
            sku: format!("S{sku}"),
            name: "x".into(),
            unit_price: Money(price),
            qty: if weighed {
                Qty(q)
            } else {
                Qty::units(1 + q % 20)
            },
            tax_class: TaxClassId(tax),
            discount: d,
            fixed_amount: None,
            price_override: false,
        })
}

fn cart() -> impl Strategy<Value = Cart> {
    (prop::collection::vec(line(), 1..25), discount()).prop_map(|(lines, bd)| {
        let mut c = Cart::default();
        for l in lines {
            c.add(l).unwrap();
        }
        c.basket_discount = bd;
        c
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn div_round_within_half(n in -1_000_000_000_000i128..1_000_000_000_000, d in 1i128..1_000_000, r in rounding()) {
        let q = div_round(n, d, r).unwrap();
        // |q·d − n| ≤ d/2 for the "half" modes, < d for Down.
        let err = (q * d - n).abs();
        match r {
            Rounding::Down => prop_assert!(err < d),
            _ => prop_assert!(2 * err <= d),
        }
    }

    #[test]
    fn allocate_sums_exactly(total in -10_000_000i64..10_000_000, w in prop::collection::vec(0i64..1_000_000, 1..30)) {
        let parts = allocate(Money(total), &w).unwrap();
        prop_assert_eq!(parts.iter().map(|m| m.0).sum::<i64>(), total);
        // Each part is within 1 of its exact proportional share.
        let ws: i128 = w.iter().map(|&x| x as i128).sum();
        if ws > 0 {
            for (p, &wi) in parts.iter().zip(&w) {
                let exact_num = total as i128 * wi as i128;
                prop_assert!(((p.0 as i128) * ws - exact_num).abs() <= ws);
            }
        }
    }

    #[test]
    fn pricing_invariants(c in cart(), cfg in config()) {
        let p = c.price(&cfg).unwrap();
        let t = &p.totals;
        // Σ line totals == total; Σ line tax == tax total; Σ class tax == tax.
        prop_assert_eq!(p.lines.iter().map(|l| l.total.0).sum::<i64>(), t.total.0);
        prop_assert_eq!(p.lines.iter().map(|l| l.tax.0).sum::<i64>(), t.tax.0);
        prop_assert_eq!(t.taxes.iter().map(|x| x.tax.0).sum::<i64>(), t.tax.0);
        prop_assert!(t.discount <= t.subtotal);
        for l in &p.lines {
            prop_assert!(l.amount.0 >= 0 && l.tax.0 >= 0 && l.total.0 >= 0);
            prop_assert_eq!(l.amount.0, l.gross.0 - l.line_discount.0 - l.basket_discount.0);
        }
        match cfg.tax_mode {
            TaxMode::Inclusive => prop_assert_eq!(t.total.0, t.subtotal.0 - t.discount.0),
            TaxMode::Exclusive => prop_assert_eq!(t.total.0, t.subtotal.0 - t.discount.0 + t.tax.0),
        }
        // Inclusive tax never exceeds the amount it is extracted from.
        if cfg.tax_mode == TaxMode::Inclusive {
            prop_assert!(t.tax <= t.total);
        }
    }

    #[test]
    fn cash_payment_settles(due in 0i64..10_000_000, extra in 0i64..100_000, cfg in config()) {
        let mut p = Payment::new(Money(due));
        let need = p.cash_due(&cfg).unwrap();
        if need.0 > 0 {
            p.add(Tender { kind: TenderKind::Cash, amount: Money(need.0 + extra), reference: String::new() }, &cfg).unwrap();
        }
        prop_assert!(p.is_complete());
        prop_assert_eq!(p.change.0, if need.0 > 0 { extra } else { 0 });
        prop_assert!(2 * p.rounding.0.abs() <= cfg.cash_increment);
        // Tendered − change == due + rounding.
        prop_assert_eq!(p.tendered().unwrap().0 - p.change.0, due + p.rounding.0);
    }

    /// Random sales and partial refunds: refunds never exceed the sale, every
    /// refund of the full quantity refunds exactly the line total, receipt
    /// numbers are gap-free, and the grand total equals sales − refunds.
    #[test]
    fn refunds_and_counters(
        cfg in config(),
        sales in prop::collection::vec(prop::collection::vec((0usize..4, 1i64..5), 1..5), 1..8),
        refunds in prop::collection::vec((0usize..8, 0u16..5, 1i64..4), 0..20),
    ) {
        let (mut st, mut log) = setup(cfg);
        let skus = ["A", "B", "C", "D"];
        let mut receipts = Vec::new();
        let mut sold = 0i64;
        let mut refunded = 0i64;
        for (k, s) in sales.iter().enumerate() {
            let mut cart = Cart::default();
            for &(i, q) in s {
                let it = st.catalog.get(skus[i]).unwrap().clone();
                cart.add(Line::from_item(&it, Qty::units(q))).unwrap();
            }
            let total = cart.price(&st.config).unwrap().totals.total;
            let mut pay = Payment::new(total);
            let need = pay.cash_due(&st.config).unwrap();
            if need.0 > 0 {
                pay.add(Tender { kind: TenderKind::Cash, amount: need, reference: String::new() }, &st.config).unwrap();
            }
            let sale = st.build_sale(&cart, &pay, CASHIER, 100 + k as i64).unwrap();
            receipts.push(sale.receipt_no);
            sold += sale.totals.total.0;
            let e = Event::SaleCompleted(sale);
            st.apply(&e).unwrap();
            log.push(e);
        }
        for (k, &(ri, li, q)) in refunds.iter().enumerate() {
            let Some(&orig) = receipts.get(ri) else { continue };
            let before = st.refundable[&orig].clone();
            match st.build_refund(orig, &[(li, Qty::units(q))], TenderKind::Cash, "", CASHIER, MGR, "r", 1000 + k as i64) {
                Ok(r) => {
                    let line = &before.lines[li as usize];
                    prop_assert!(r.total.0 <= line.total.0 - line.refunded_total.0);
                    refunded += r.total.0;
                    let e = Event::RefundCompleted(r);
                    st.apply(&e).unwrap();
                    log.push(e);
                }
                Err(_) => {
                    // Rejected only because it would exceed what is left.
                    if let Some(line) = before.lines.get(li as usize) {
                        prop_assert!(q * 1000 > line.qty.0 - line.refunded_qty.0);
                    }
                }
            }
        }
        for rf in st.refundable.values() {
            for l in &rf.lines {
                prop_assert!(l.refunded_qty <= l.qty);
                prop_assert!(l.refunded_total <= l.total);
                if l.refunded_qty == l.qty {
                    prop_assert_eq!(l.refunded_total, l.total);
                    prop_assert_eq!(l.refunded_tax, l.tax);
                }
            }
        }
        prop_assert_eq!(st.grand_total.0, sold - refunded);
        let c = &st.shift.as_ref().unwrap().counters;
        prop_assert_eq!(c.net().unwrap().0, sold - refunded);
        prop_assert_eq!(c.last_receipt.unwrap(), st.next_receipt - 1);

        // Z report, then replay the whole journal from scratch: identical state.
        let z = st.build_report(ReportKind::Z, MGR, 9999, None).unwrap();
        let e = Event::Report(z);
        st.apply(&e).unwrap();
        log.push(e);
        let mut replay = PosState::default();
        for e in &log {
            replay.apply(e).unwrap();
        }
        prop_assert_eq!(replay, st);
    }
}
