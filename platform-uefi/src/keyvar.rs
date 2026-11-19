//! Per-machine store-binding key, kept in a non-volatile, boot-services-only
//! UEFI variable (not visible to an OS at runtime). Generated on first boot.

use uefi::runtime::{self, VariableAttributes, VariableVendor};
use uefi::{cstr16, guid};

pub const VENDOR: VariableVendor = VariableVendor(guid!("5b0f9c4e-2a7d-4f61-9e83-1c6d2b8a4f70"));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    Existing,
    Generated,
    /// NVRAM unavailable or no entropy: binding is effectively disabled.
    Fallback,
}

pub fn machine_key(rng: &mut dyn pos_hal::Rng) -> ([u8; 32], KeySource) {
    let name = cstr16!("PosStoreKey");
    let mut buf = [0u8; 32];
    if let Ok((v, _)) = runtime::get_variable(name, &VENDOR, &mut buf)
        && v.len() == 32
    {
        let mut k = [0u8; 32];
        k.copy_from_slice(v);
        return (k, KeySource::Existing);
    }
    let mut k = [0u8; 32];
    if rng.fill(&mut k).is_err() {
        return ([0u8; 32], KeySource::Fallback);
    }
    let attrs = VariableAttributes::NON_VOLATILE | VariableAttributes::BOOTSERVICE_ACCESS;
    match runtime::set_variable(name, &VENDOR, attrs, &k) {
        Ok(()) => (k, KeySource::Generated),
        Err(_) => ([0u8; 32], KeySource::Fallback),
    }
}
