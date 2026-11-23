//! The per-machine store-binding key, handed over by `pos-stage0`.
//!
//! Tier A keeps the key in a boot-services-only UEFI variable, which cannot
//! be read after ExitBootServices (Limine exits them before starting us).
//! So the signed stage-0 loader, which runs before Limine, reads or creates
//! that same variable and copies the key into one firmware-reserved page.
//! It publishes the page's physical address as an EFI configuration table.
//! We read the key here and wipe the page. The same machine therefore has
//! the same key under Tier A and Tier B.

use crate::limine;
use crate::mm;

/// Must match `pos-stage0`.
pub const TABLE_GUID: [u8; 16] = pos_drivers::guid("6f1c2b9e-4a07-4d3b-9b52-8e0f7a1d3c64");
pub const MAGIC: &[u8; 8] = b"POSKEY01";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    /// Read from NVRAM by stage 0.
    Existing,
    /// Generated on this boot by stage 0 and stored in NVRAM.
    Generated,
    /// Stage 0 ran but had no NVRAM or entropy.
    Fallback,
    /// No stage 0 (booted Limine directly): binding disabled.
    Missing,
}

/// Find the handover page, copy the key, scrub the page.
pub fn take() -> ([u8; 32], KeySource) {
    let none = ([0u8; 32], KeySource::Missing);
    let Some(st_phys) = limine::EFI_SYSTEM_TABLE.response().map(|r| r.address) else {
        return none;
    };
    let Some(st) = mm::map_ram_ro(st_phys, 120) else {
        return none;
    };
    // SAFETY: mapped above; the EFI system table is at least 120 bytes.
    let (sig, n, tables) = unsafe {
        (
            (st as *const u64).read_volatile(),
            ((st + 104) as *const u64).read_volatile(),
            ((st + 112) as *const u64).read_volatile(),
        )
    };
    if sig != 0x5453_5953_2049_4249 || n > 1024 {
        return none;
    }
    let Some(arr) = mm::map_ram_ro(tables, n * 24) else {
        return none;
    };
    for i in 0..n {
        let e = arr + i * 24;
        let mut g = [0u8; 16];
        for (k, b) in g.iter_mut().enumerate() {
            // SAFETY: inside the mapped table array.
            *b = unsafe { ((e + k as u64) as *const u8).read_volatile() };
        }
        if g != TABLE_GUID {
            continue;
        }
        // SAFETY: as above.
        let page_phys = unsafe { ((e + 16) as *const u64).read_volatile() };
        let Some(page) = mm::map_ram_rw(page_phys, 4096) else {
            return none;
        };
        let p = page as *mut u8;
        let mut hdr = [0u8; 16];
        let mut key = [0u8; 32];
        // SAFETY: the page is mapped writable; layout is magic[8],
        // version u32, source u32, key[32].
        unsafe {
            for (k, b) in hdr.iter_mut().enumerate() {
                *b = p.add(k).read_volatile();
            }
            for (k, b) in key.iter_mut().enumerate() {
                *b = p.add(16 + k).read_volatile();
            }
            for k in 0..48 {
                p.add(k).write_volatile(0);
            }
        }
        if &hdr[0..8] != MAGIC {
            return none;
        }
        let src = match u32::from_le_bytes([hdr[12], hdr[13], hdr[14], hdr[15]]) {
            0 => KeySource::Existing,
            1 => KeySource::Generated,
            _ => KeySource::Fallback,
        };
        return if src == KeySource::Fallback {
            ([0u8; 32], src)
        } else {
            (key, src)
        };
    }
    none
}
