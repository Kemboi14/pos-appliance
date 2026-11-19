#![allow(dead_code)]
use pos_core::Money;
use pos_core::auth::{PIN_KDF_VERSION, PinHash, PinHasher, Role, User};
use pos_core::catalog::Item;
use pos_core::config::{Config, TaxClassId};
use pos_core::state::{Event, PosState};

/// Test-only hasher (NOT a KDF): mixes pin and salt deterministically.
pub struct FakeHasher;
impl PinHasher for FakeHasher {
    fn hash(&self, pin: &[u8], salt: &[u8; 16]) -> [u8; 32] {
        let mut h = [0u8; 32];
        for (i, b) in h.iter_mut().enumerate() {
            *b = salt[i % 16] ^ pin.get(i % pin.len().max(1)).copied().unwrap_or(0) ^ (i as u8);
        }
        h
    }
}

pub fn user(id: u16, role: Role, pin: &str) -> User {
    let salt = [id as u8; 16];
    User {
        id,
        name: format!("user{id}"),
        role,
        pin: PinHash {
            version: PIN_KDF_VERSION,
            salt,
            hash: FakeHasher.hash(pin.as_bytes(), &salt),
        },
        active: true,
    }
}

pub fn item(sku: &str, price: i64, tax: u8) -> Item {
    Item {
        sku: sku.into(),
        barcode: None,
        plu: None,
        name: format!("Item {sku}"),
        price: Money(price),
        tax_class: TaxClassId(tax),
        weighable: false,
        quick_key: None,
    }
}

pub const MGR: u16 = 1;
pub const CASHIER: u16 = 2;

/// A state with a manager (PIN 1234), a cashier (PIN 0000), a few items and
/// an open shift with a 100.00 float.
pub fn setup(cfg: Config) -> (PosState, Vec<Event>) {
    let mut st = PosState::default();
    let mut log = Vec::new();
    let mut ap = |st: &mut PosState, e: Event| {
        st.apply(&e).unwrap();
        log.push(e);
    };
    ap(
        &mut st,
        Event::UserSet {
            by: None,
            at: 1,
            user: user(MGR, Role::Manager, "1234"),
        },
    );
    ap(
        &mut st,
        Event::UserSet {
            by: Some(MGR),
            at: 2,
            user: user(CASHIER, Role::Cashier, "0000"),
        },
    );
    ap(
        &mut st,
        Event::ConfigSet {
            by: MGR,
            at: 3,
            config: cfg,
        },
    );
    for (sku, p, t) in [
        ("A", 116_00, 0),
        ("B", 2_49, 0),
        ("C", 1_00, 1),
        ("D", 9_99, 0),
    ] {
        ap(
            &mut st,
            Event::CatalogUpsert {
                by: MGR,
                at: 4,
                item: item(sku, p, t),
            },
        );
    }
    ap(
        &mut st,
        Event::ShiftOpened {
            shift_id: 1,
            by: CASHIER,
            at: 5,
            float: Money(100_00),
        },
    );
    (st, log)
}
