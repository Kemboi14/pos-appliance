//! Users, roles, permissions and PIN lockout policy.
//!
//! PIN hashing itself (Argon2id) is behind [`PinHasher`] so this crate stays
//! free of crypto dependencies; `pos-app` provides the Argon2id hasher.

use alloc::string::String;
use serde::{Deserialize, Serialize};

pub type UserId = u16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Role {
    Cashier,
    Manager,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Permission {
    Sell,
    LineVoid,
    VoidSale,
    Refund,
    PriceOverride,
    Discount,
    NoSale,
    Reprint,
    XReport,
    ZReport,
    OpenShift,
    ManageCatalog,
    ManageUsers,
    Export,
    Import,
    Configure,
    UnlockUser,
}

impl Permission {
    pub fn label(self) -> &'static str {
        match self {
            Permission::Sell => "sell",
            Permission::LineVoid => "line void",
            Permission::VoidSale => "void sale",
            Permission::Refund => "refund",
            Permission::PriceOverride => "price override",
            Permission::Discount => "discount",
            Permission::NoSale => "no sale",
            Permission::Reprint => "reprint",
            Permission::XReport => "X report",
            Permission::ZReport => "Z report",
            Permission::OpenShift => "open shift",
            Permission::ManageCatalog => "catalog",
            Permission::ManageUsers => "users",
            Permission::Export => "export",
            Permission::Import => "import",
            Permission::Configure => "configure",
            Permission::UnlockUser => "unlock user",
        }
    }
}

/// Static role → permission matrix. Anything a cashier lacks needs a manager
/// override (manager PIN), which is logged as an audit event.
pub fn role_allows(role: Role, p: Permission) -> bool {
    match role {
        Role::Manager => true,
        Role::Cashier => matches!(
            p,
            Permission::Sell | Permission::Reprint | Permission::XReport | Permission::OpenShift
        ),
    }
}

/// Argon2id output plus its salt. Parameters are fixed per [`PIN_KDF_VERSION`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinHash {
    pub version: u8,
    pub salt: [u8; 16],
    pub hash: [u8; 32],
}

pub const PIN_KDF_VERSION: u8 = 1;

pub trait PinHasher {
    fn hash(&self, pin: &[u8], salt: &[u8; 16]) -> [u8; 32];
}

/// Constant-time comparison of two hashes.
pub fn ct_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut d = 0u8;
    for i in 0..32 {
        d |= a[i] ^ b[i];
    }
    d == 0
}

pub fn verify_pin(h: &dyn PinHasher, pin: &[u8], stored: &PinHash) -> bool {
    stored.version == PIN_KDF_VERSION && ct_eq(&h.hash(pin, &stored.salt), &stored.hash)
}

/// PINs are 4–8 digits.
pub fn valid_pin(pin: &[u8]) -> bool {
    (4..=8).contains(&pin.len()) && pin.iter().all(u8::is_ascii_digit)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub id: UserId,
    pub name: String,
    pub role: Role,
    pub pin: PinHash,
    pub active: bool,
}

/// Persistent per-user lockout state (survives reboot because it is rebuilt
/// from `LoginFailed` events).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockState {
    pub failures: u8,
    /// Cashiers: locked until a manager unlocks. Managers: locked until this
    /// wall-clock time (escalating), so the last manager can never be locked
    /// out permanently.
    pub locked: bool,
    pub locked_until: i64,
    pub lockouts: u8,
}

/// Manager lockout duration after the n-th lockout: 5 min × 2^n, max 8 h.
pub fn manager_lockout_secs(lockouts: u8) -> i64 {
    (300i64 << u32::from(lockouts.min(7))).min(8 * 3600)
}
