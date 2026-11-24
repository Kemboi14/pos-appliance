//! PC speaker through PIT channel 2 (port 0x61 gate). Many modern boards
//! have no speaker fitted; then this is silent, which is harmless.

use crate::clock;
use crate::cpu::{inb, outb};

#[derive(Debug, Default)]
pub struct Speaker;

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
        clock::spin_ms(u64::from(ms.min(500)));
        outb(0x61, gate & !3);
    }
}
