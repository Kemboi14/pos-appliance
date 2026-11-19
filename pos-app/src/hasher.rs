//! Argon2id PIN hasher with pinned parameters.
//!
//! Parameters (KDF version 1): Argon2id v0x13, m = 19456 KiB, t = 2, p = 1,
//! 32-byte output, 16-byte random salt — the OWASP 2023 minimum profile.
//! Changing them requires a new `PIN_KDF_VERSION` and re-enrolment.
//!
//! Honest limits: a 4–8 digit PIN has at most 10^8 values, so anyone holding
//! a copy of the store can brute-force a PIN offline regardless of the KDF.
//! The real protections are online lockout, physical custody of the stick,
//! and the hash chain making any after-the-fact change evident.

use argon2::{Algorithm, Argon2, Params, Version};
use pos_core::auth::PinHasher;

pub const M_COST_KIB: u32 = 19_456;
pub const T_COST: u32 = 2;
pub const P_COST: u32 = 1;

#[derive(Debug)]
pub struct Argon2Hasher {
    a: Argon2<'static>,
}

impl Argon2Hasher {
    pub fn new() -> Self {
        Self::with_params(M_COST_KIB, T_COST)
    }
    /// Reduced parameters for host tests only.
    pub fn with_params(m_kib: u32, t: u32) -> Self {
        let params = Params::new(m_kib, t, P_COST, Some(32)).unwrap_or_default();
        Argon2Hasher {
            a: Argon2::new(Algorithm::Argon2id, Version::V0x13, params),
        }
    }
}

impl Default for Argon2Hasher {
    fn default() -> Self {
        Self::new()
    }
}

impl PinHasher for Argon2Hasher {
    fn hash(&self, pin: &[u8], salt: &[u8; 16]) -> [u8; 32] {
        let mut out = [0u8; 32];
        // Only fails for invalid lengths, which are fixed here.
        if self.a.hash_password_into(pin, salt, &mut out).is_err() {
            out = [0xFF; 32];
        }
        out
    }
}
