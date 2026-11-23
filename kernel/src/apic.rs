//! Interrupts: legacy PICs remapped and masked, local APIC (xAPIC MMIO or
//! x2APIC MSRs) with a 1 kHz periodic timer. The timer only wakes the CPU
//! from `hlt`; all device work happens in the main loop with interrupts
//! off, so handlers touch nothing but an atomic counter and the EOI register.

use crate::cpu::{self, outb, rdmsr, rdtsc, wrmsr};
use core::arch::naked_asm;
use core::sync::atomic::{AtomicU64, Ordering};

const TIMER_VECTOR: u8 = 0x40;
const SPURIOUS_VECTOR: u8 = 0xff;
const PIC_BASE: u8 = 0x20;
pub const TICK_HZ: u64 = 1000;

/// Timer interrupts since boot (only the ISR touches it; kept for debugging
/// with a monitor attached).
pub static TICKS: AtomicU64 = AtomicU64::new(0);
/// Virtual address of the xAPIC EOI register, or 0 in x2APIC mode.
static EOI_ADDR: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy)]
enum Mode {
    X(u64),
    X2,
}

#[derive(Debug)]
pub struct Apic {
    mode: Mode,
    pub x2apic: bool,
    pub timer_per_ms: u32,
}

impl Apic {
    fn read(&self, reg: u32) -> u32 {
        match self.mode {
            // SAFETY: `base` is the mapped LAPIC page; reg < 0x400.
            Mode::X(base) => unsafe { ((base + u64::from(reg)) as *const u32).read_volatile() },
            Mode::X2 => rdmsr(0x800 + (reg >> 4)) as u32,
        }
    }
    fn write(&self, reg: u32, v: u32) {
        match self.mode {
            // SAFETY: as above.
            Mode::X(base) => unsafe { ((base + u64::from(reg)) as *mut u32).write_volatile(v) },
            Mode::X2 => wrmsr(0x800 + (reg >> 4), u64::from(v)),
        }
    }
}

#[unsafe(naked)]
extern "C" fn timer_isr() {
    naked_asm!(
        "push rax",
        "lock inc qword ptr [rip + {ticks}]",
        "mov rax, qword ptr [rip + {eoi}]",
        "test rax, rax",
        "jz 2f",
        "mov dword ptr [rax], 0",
        "pop rax",
        "iretq",
        "2:",
        "push rcx",
        "push rdx",
        "mov ecx, 0x80b",
        "xor eax, eax",
        "xor edx, edx",
        "wrmsr",
        "pop rdx",
        "pop rcx",
        "pop rax",
        "iretq",
        ticks = sym TICKS,
        eoi = sym EOI_ADDR,
    )
}

/// Spurious LAPIC interrupts and stray PIC IRQs: no EOI, just return.
#[unsafe(naked)]
extern "C" fn ignore_isr() {
    naked_asm!("iretq")
}

fn mask_pics() {
    // ICW1..ICW4: remap to 0x20/0x28 so a stray IRQ can't look like an
    // exception, then mask every line.
    outb(0x20, 0x11);
    outb(0xa0, 0x11);
    outb(0x21, PIC_BASE);
    outb(0xa1, PIC_BASE + 8);
    outb(0x21, 4);
    outb(0xa1, 2);
    outb(0x21, 1);
    outb(0xa1, 1);
    outb(0x21, 0xff);
    outb(0xa1, 0xff);
}

pub fn init(tsc_per_us: u64) -> Option<Apic> {
    mask_pics();
    for v in PIC_BASE..PIC_BASE + 16 {
        cpu::set_gate(v, ignore_isr);
    }
    cpu::set_gate(SPURIOUS_VECTOR, ignore_isr);
    cpu::set_gate(TIMER_VECTOR, timer_isr);

    let base_msr = rdmsr(0x1b);
    if base_msr & (1 << 11) == 0 {
        wrmsr(0x1b, base_msr | (1 << 11)); // global enable
    }
    let x2apic = rdmsr(0x1b) & (1 << 10) != 0;
    let mode = if x2apic {
        Mode::X2
    } else {
        let phys = rdmsr(0x1b) & 0x000f_ffff_ffff_f000;
        let va = crate::mm::map_mmio(phys, 4096)?;
        EOI_ADDR.store(va + 0xb0, Ordering::Relaxed);
        Mode::X(va)
    };
    let mut apic = Apic {
        mode,
        x2apic,
        timer_per_ms: 0,
    };
    apic.write(0x80, 0); // TPR: accept everything
    apic.write(0xf0, 0x100 | u32::from(SPURIOUS_VECTOR)); // enable
    apic.write(0x3e0, 0x3); // divide by 16

    // Calibrate the timer against the TSC over 10 ms, masked.
    apic.write(0x320, 1 << 16);
    apic.write(0x380, u32::MAX);
    let end = rdtsc() + 10_000 * tsc_per_us;
    while rdtsc() < end {
        core::hint::spin_loop();
    }
    let elapsed = u32::MAX - apic.read(0x390);
    apic.write(0x380, 0);
    apic.timer_per_ms = (elapsed / 10).max(1);

    apic.write(0x320, (1 << 17) | u32::from(TIMER_VECTOR)); // periodic
    apic.write(
        0x380,
        (u64::from(apic.timer_per_ms) * 1000 / TICK_HZ).max(1) as u32,
    );
    Some(apic)
}
