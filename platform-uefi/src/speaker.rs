//! PC speaker through PIT channel 2 (port 0x61 gate). Boot services run
//! at ring 0 with I/O ports accessible. Many modern boards have no speaker
//! fitted; then this is silent, which is harmless.

use core::arch::asm;
use core::time::Duration;

#[derive(Debug, Default)]
pub struct Speaker;

fn outb(port: u16, v: u8) {
    // SAFETY: PIT/port-0x61 accesses only touch the speaker timer and gate.
    unsafe {
        asm!("out dx, al", in("dx") port, in("al") v, options(nomem, nostack, preserves_flags))
    }
}

fn inb(port: u16) -> u8 {
    let v: u8;
    // SAFETY: as above; reading port 0x61 has no side effects.
    unsafe {
        asm!("in al, dx", out("al") v, in("dx") port, options(nomem, nostack, preserves_flags))
    }
    v
}

impl pos_hal::Sound for Speaker {
    fn beep(&mut self, hz: u32, ms: u32) {
        if hz < 20 || ms == 0 {
            return;
        }
        let div = (1_193_182 / hz).clamp(1, 0xFFFF) as u16;
        outb(0x43, 0xB6);
        outb(0x42, div as u8);
        outb(0x42, (div >> 8) as u8);
        let gate = inb(0x61);
        outb(0x61, gate | 3);
        uefi::boot::stall(Duration::from_millis(u64::from(ms.min(500))));
        outb(0x61, gate & !3);
    }
}
