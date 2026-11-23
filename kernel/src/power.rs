//! Reset: ACPI FADT reset register, then the keyboard-controller reset
//! line, then a triple fault.

use crate::cpu::{self, inb, outb};

pub fn reset() -> ! {
    crate::acpi::reset();
    crate::clock::spin_ms(100);
    // Wait for the KBC input buffer to empty, then pulse the reset line.
    for _ in 0..100_000 {
        if inb(0x64) & 0x02 == 0 {
            break;
        }
        core::hint::spin_loop();
    }
    outb(0x64, 0xfe);
    crate::clock::spin_ms(100);
    cpu::triple_fault()
}

/// Unrecoverable failure: pause so a message on screen can be read, then
/// reset. The store is crash-consistent, so a reset is always safe.
pub fn fatal_reset() -> ! {
    crate::clock::spin_ms(10_000);
    reset()
}
