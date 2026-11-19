//! State-machine flows: sale, refund, permissions, lockout, Z report.

mod common;
use common::*;

use pos_core::auth::Permission;
use pos_core::cart::{Cart, Line};
use pos_core::config::Config;
use pos_core::shift::ReportKind;
use pos_core::state::{DomainError, Event, PosState};
use pos_core::tender::{Payment, Tender, TenderKind};
use pos_core::{Money, Qty};

fn cash(amount: i64) -> Tender {
    Tender {
        kind: TenderKind::Cash,
        amount: Money(amount),
        reference: String::new(),
    }
}

fn sell(st: &mut PosState, skus: &[(&str, i64)], at: i64) -> Event {
    let mut cart = Cart::default();
    for (sku, q) in skus {
        let it = st.catalog.get(sku).unwrap().clone();
        cart.add(Line::from_item(&it, Qty::units(*q))).unwrap();
    }
    let total = cart.price(&st.config).unwrap().totals.total;
    let mut pay = Payment::new(total);
    pay.add(cash(total.0 + 1_00), &st.config).unwrap();
    let sale = st.build_sale(&cart, &pay, CASHIER, at).unwrap();
    let e = Event::SaleCompleted(sale);
    st.apply(&e).unwrap();
    e
}

#[test]
fn sale_refund_and_z() {
    let (mut st, _) = setup(Config::default());
    sell(&mut st, &[("A", 1), ("B", 2)], 10);
    assert_eq!(st.next_receipt, 2);
    let c = &st.shift.as_ref().unwrap().counters;
    assert_eq!(c.sales_total, Money(116_00 + 4_98));
    assert_eq!(c.change_given, Money(1_00));

    // Refund 1 of the 2 "B": half of 4.98 = 2.49.
    let r = st
        .build_refund(
            1,
            &[(1, Qty::ONE)],
            TenderKind::Cash,
            "",
            CASHIER,
            MGR,
            "damaged",
            11,
        )
        .unwrap();
    assert_eq!(r.total, Money(2_49));
    st.apply(&Event::RefundCompleted(r)).unwrap();
    assert_eq!(st.next_receipt, 3);
    // Refunding 2 more is more than was sold.
    let too_many = st.build_refund(
        1,
        &[(1, Qty::units(2))],
        TenderKind::Cash,
        "",
        CASHIER,
        MGR,
        "x",
        12,
    );
    assert_eq!(too_many.unwrap_err(), DomainError::RefundExceedsSale);

    // Expected cash = float 100 + tendered (121.98) − change 1.00 − refund 2.49.
    let shift = st.shift.as_ref().unwrap();
    assert_eq!(
        shift.expected_cash().unwrap(),
        Money(100_00 + 121_98 - 1_00 - 2_49)
    );

    let z = st.build_report(ReportKind::Z, MGR, 20, None).unwrap();
    assert_eq!(z.z_no, Some(1));
    st.apply(&Event::Report(z.clone())).unwrap();
    assert!(st.shift.is_none());
    assert_eq!(st.next_z, 2);
    assert_eq!(st.grand_total, Money(116_00 + 4_98 - 2_49));
    // Replaying the same Z again is rejected (no shift, and z number used).
    assert!(st.apply(&Event::Report(z)).is_err());
}

#[test]
fn cashier_cannot_refund_or_z_without_manager() {
    let (mut st, _) = setup(Config::default());
    sell(&mut st, &[("A", 1)], 10);
    let r = st
        .build_refund(
            1,
            &[(0, Qty::ONE)],
            TenderKind::Cash,
            "",
            CASHIER,
            CASHIER,
            "x",
            11,
        )
        .unwrap();
    assert_eq!(
        st.apply(&Event::RefundCompleted(r)),
        Err(DomainError::Forbidden(Permission::Refund))
    );
    let z = st.build_report(ReportKind::Z, CASHIER, 20, None).unwrap();
    assert!(st.apply(&Event::Report(z)).is_err());
    assert!(!st.allows(CASHIER, Permission::LineVoid));
    assert!(
        st.check_override(MGR, b"1234", &FakeHasher, Permission::LineVoid, 0)
            .is_ok()
    );
    assert_eq!(
        st.check_override(MGR, b"9999", &FakeHasher, Permission::LineVoid, 0),
        Err(DomainError::BadPin)
    );
    assert!(
        st.check_override(CASHIER, b"0000", &FakeHasher, Permission::LineVoid, 0)
            .is_err()
    );
}

#[test]
fn receipt_numbers_cannot_be_skipped_or_reused() {
    let (mut st, _) = setup(Config::default());
    let e = sell(&mut st, &[("A", 1)], 10);
    // Re-applying the same sale (same receipt number) is rejected.
    assert!(matches!(
        st.apply(&e),
        Err(DomainError::ReceiptSequence {
            expected: 2,
            got: 1
        })
    ));
    if let Event::SaleCompleted(mut s) = e {
        s.receipt_no = 5;
        assert!(st.apply(&Event::SaleCompleted(s)).is_err());
    }
}

#[test]
fn tampered_totals_rejected() {
    let (mut st, _) = setup(Config::default());
    let mut cart = Cart::default();
    cart.add(Line::from_item(st.catalog.get("A").unwrap(), Qty::ONE))
        .unwrap();
    let total = cart.price(&st.config).unwrap().totals.total;
    let mut pay = Payment::new(total);
    pay.add(cash(total.0), &st.config).unwrap();
    let mut sale = st.build_sale(&cart, &pay, CASHIER, 10).unwrap();
    sale.totals.total = Money(1);
    assert_eq!(
        st.apply(&Event::SaleCompleted(sale)),
        Err(DomainError::InconsistentTotals)
    );
}

#[test]
fn lockout_survives_replay_and_manager_unlocks() {
    let (mut st, mut log) = setup(Config::default());
    for i in 0..5 {
        let (e, ok) = st.login(CASHIER, b"9999", &FakeHasher, 100 + i).unwrap();
        assert!(!ok);
        st.apply(&e).unwrap();
        log.push(e);
    }
    assert_eq!(
        st.login(CASHIER, b"0000", &FakeHasher, 200),
        Err(DomainError::Locked)
    );
    // Rebuild from the journal: still locked.
    let mut re = PosState::default();
    for e in &log {
        re.apply(e).unwrap();
    }
    assert!(re.is_locked(CASHIER, 300));
    let u = Event::UserUnlocked {
        user: CASHIER,
        by: MGR,
        at: 301,
    };
    re.apply(&u).unwrap();
    let (e, ok) = re.login(CASHIER, b"0000", &FakeHasher, 302).unwrap();
    assert!(ok);
    re.apply(&e).unwrap();
}

#[test]
fn manager_lockout_is_temporary() {
    let (mut st, _) = setup(Config::default());
    for i in 0..5 {
        let (e, _) = st.login(MGR, b"0000", &FakeHasher, 1000 + i).unwrap();
        st.apply(&e).unwrap();
    }
    assert!(st.is_locked(MGR, 1004));
    assert!(!st.is_locked(MGR, 1004 + 301));
    assert!(st.login(MGR, b"1234", &FakeHasher, 1004 + 301).unwrap().1);
}

#[test]
fn cannot_remove_last_manager() {
    let (mut st, _) = setup(Config::default());
    let mut m = st.users[&MGR].clone();
    m.active = false;
    assert_eq!(
        st.apply(&Event::UserSet {
            by: Some(MGR),
            at: 9,
            user: m
        }),
        Err(DomainError::LastManager)
    );
}

#[test]
fn snapshot_roundtrip_via_postcard() {
    let (mut st, _) = setup(Config::default());
    sell(&mut st, &[("A", 2), ("D", 1)], 10);
    let bytes = postcard::to_allocvec(&st).unwrap();
    let back: PosState = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(back, st);
}

#[test]
fn csv_catalog_import_is_validated() {
    let cfg = Config::default();
    let good = "sku,barcode,plu,name,price,tax_code,weighable,quick_key\n\
                MILK,4006381333931,,Milk 1L,1.20,A,0,1\n\
                APPLES,,04711,\"Apples, red\",3.99,Z,1,\n";
    let items = pos_core::csv::catalog_from_csv(good, &cfg).unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[1].name, "Apples, red");
    let bad_check = good.replace("4006381333931", "4006381333932");
    let e = pos_core::csv::catalog_from_csv(&bad_check, &cfg).unwrap_err();
    assert_eq!(e.line, 2);
    let dup = format!("{good}MILK2,4006381333931,,Dup,1.00,A,0,\n");
    assert!(pos_core::csv::catalog_from_csv(&dup, &cfg).is_err());
}
