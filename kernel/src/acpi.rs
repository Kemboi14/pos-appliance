//! ACPI: just enough to find the FADT reset register (step 6). The
//! appliance only ever resets; there is no S5 power-off path.

use crate::cpu::outb;
use crate::mm;
use core::sync::atomic::{AtomicU64, Ordering};

/// Packed reset register: bits 0..8 value, 8..16 address space, address in
/// RESET_ADDR. 0 = none.
static RESET_INFO: AtomicU64 = AtomicU64::new(0);
static RESET_ADDR: AtomicU64 = AtomicU64::new(0);

fn read_table(phys: u64) -> Option<(u64, u32)> {
    let hdr = mm::map_ram_ro(phys, 36)?;
    // SAFETY: header mapped; length at offset 4.
    let len = unsafe { ((hdr + 4) as *const u32).read_unaligned() };
    if !(36..=1 << 20).contains(&len) {
        return None;
    }
    let v = mm::map_ram_ro(phys, u64::from(len))?;
    Some((v, len))
}

fn checksum_ok(v: u64, len: u32) -> bool {
    let mut s = 0u8;
    for i in 0..u64::from(len) {
        // SAFETY: mapped by `read_table` for `len` bytes.
        s = s.wrapping_add(unsafe { ((v + i) as *const u8).read() });
    }
    s == 0
}

fn sig(v: u64) -> [u8; 4] {
    // SAFETY: callers pass a mapped table.
    unsafe { (v as *const [u8; 4]).read_unaligned() }
}

/// Locate the FADT and remember its reset register. Returns whether one
/// is usable.
pub fn init(rsdp_phys: u64) -> bool {
    find_reset(rsdp_phys).is_some()
}

fn find_reset(rsdp_phys: u64) -> Option<()> {
    let r = mm::map_ram_ro(rsdp_phys, 36)?;
    // SAFETY: RSDP mapped (36 bytes covers v1 and v2).
    let (sigr, rev, rsdt, xsdt) = unsafe {
        (
            (r as *const [u8; 8]).read_unaligned(),
            ((r + 15) as *const u8).read(),
            ((r + 16) as *const u32).read_unaligned(),
            ((r + 24) as *const u64).read_unaligned(),
        )
    };
    if &sigr != b"RSD PTR " {
        return None;
    }
    let (root, entry) = if rev >= 2 && xsdt != 0 {
        (xsdt, 8)
    } else {
        (u64::from(rsdt), 4)
    };
    let (rv, rlen) = read_table(root)?;
    if !checksum_ok(rv, rlen) {
        return None;
    }
    let count = (u64::from(rlen) - 36) / entry;
    for i in 0..count {
        let at = rv + 36 + i * entry;
        // SAFETY: inside the mapped root table.
        let phys = unsafe {
            if entry == 8 {
                (at as *const u64).read_unaligned()
            } else {
                u64::from((at as *const u32).read_unaligned())
            }
        };
        let Some((t, len)) = read_table(phys) else {
            continue;
        };
        if sig(t) != *b"FACP" || len < 129 || !checksum_ok(t, len) {
            continue;
        }
        // SAFETY: FADT mapped for `len >= 129` bytes.
        let (flags, space, addr, value) = unsafe {
            (
                ((t + 112) as *const u32).read_unaligned(),
                ((t + 116) as *const u8).read(),
                ((t + 120) as *const u64).read_unaligned(),
                ((t + 128) as *const u8).read(),
            )
        };
        if flags & (1 << 10) == 0 || addr == 0 || space > 1 {
            return None;
        }
        RESET_ADDR.store(addr, Ordering::Relaxed);
        RESET_INFO.store(
            1 << 16 | u64::from(space) << 8 | u64::from(value),
            Ordering::Relaxed,
        );
        return Some(());
    }
    None
}

/// Write the FADT reset value, if a reset register was found.
pub fn reset() {
    let info = RESET_INFO.load(Ordering::Relaxed);
    if info == 0 {
        return;
    }
    let addr = RESET_ADDR.load(Ordering::Relaxed);
    let value = info as u8;
    match (info >> 8) as u8 {
        1 => outb(addr as u16, value),
        _ => {
            if let Some(v) = mm::map_mmio(addr, 1) {
                // SAFETY: mapped MMIO byte register from the FADT.
                unsafe { (v as *mut u8).write_volatile(value) };
            }
        }
    }
}
