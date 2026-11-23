//! PCI configuration space through the legacy 0xCF8/0xCFC mechanism, which
//! every PC chipset still decodes for the first 256 bytes of each function.

use alloc::vec::Vec;
use core::arch::asm;

fn outl(port: u16, v: u32) {
    // SAFETY: PCI configuration ports only.
    unsafe {
        asm!("out dx, eax", in("dx") port, in("eax") v, options(nomem, nostack, preserves_flags))
    }
}

fn inl(port: u16) -> u32 {
    let v: u32;
    // SAFETY: PCI configuration ports only.
    unsafe {
        asm!("in eax, dx", out("eax") v, in("dx") port, options(nomem, nostack, preserves_flags))
    }
    v
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Function {
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
    pub vendor: u16,
    pub device: u16,
    pub class: u8,
    pub subclass: u8,
    pub prog_if: u8,
}

fn addr(bus: u8, dev: u8, func: u8, off: u8) -> u32 {
    0x8000_0000
        | u32::from(bus) << 16
        | u32::from(dev) << 11
        | u32::from(func) << 8
        | u32::from(off & 0xfc)
}

impl Function {
    pub fn read(&self, off: u8) -> u32 {
        outl(0xcf8, addr(self.bus, self.dev, self.func, off));
        inl(0xcfc)
    }
    pub fn write(&self, off: u8, v: u32) {
        outl(0xcf8, addr(self.bus, self.dev, self.func, off));
        outl(0xcfc, v);
    }

    /// Physical address and size of a memory BAR (64-bit aware).
    pub fn memory_bar(&self, n: u8) -> Option<(u64, u64)> {
        let off = 0x10 + 4 * n;
        let lo = self.read(off);
        if lo & 1 != 0 {
            return None; // I/O BAR
        }
        let is64 = (lo >> 1) & 3 == 2;
        let hi = if is64 { self.read(off + 4) } else { 0 };
        // Size the BAR with memory decoding off.
        let cmd = self.read(0x04);
        self.write(0x04, cmd & !0x2);
        self.write(off, u32::MAX);
        let mut mask = u64::from(self.read(off) & !0xf);
        if is64 {
            self.write(off + 4, u32::MAX);
            mask |= u64::from(self.read(off + 4)) << 32;
            self.write(off + 4, hi);
        } else {
            mask |= 0xffff_ffff_0000_0000;
        }
        self.write(off, lo);
        self.write(0x04, cmd);
        let base = u64::from(lo & !0xf) | u64::from(hi) << 32;
        let size = (!mask).wrapping_add(1);
        (base != 0 && size != 0).then_some((base, size))
    }

    /// Memory decoding + bus mastering on, legacy INTx off (we poll).
    pub fn enable_mmio_dma(&self) {
        let cmd = self.read(0x04) & 0xffff;
        self.write(0x04, cmd | 0x2 | 0x4 | 0x400);
    }
}

pub fn enumerate() -> Vec<Function> {
    let mut out = Vec::new();
    for bus in 0..=255u8 {
        for dev in 0..32u8 {
            for func in 0..8u8 {
                let probe = Function {
                    bus,
                    dev,
                    func,
                    vendor: 0,
                    device: 0,
                    class: 0,
                    subclass: 0,
                    prog_if: 0,
                };
                let id = probe.read(0);
                if id & 0xffff == 0xffff {
                    if func == 0 {
                        break;
                    }
                    continue;
                }
                let cls = probe.read(0x08);
                out.push(Function {
                    vendor: id as u16,
                    device: (id >> 16) as u16,
                    class: (cls >> 24) as u8,
                    subclass: (cls >> 16) as u8,
                    prog_if: (cls >> 8) as u8,
                    ..probe
                });
                let header = (probe.read(0x0c) >> 16) as u8;
                if func == 0 && header & 0x80 == 0 {
                    break; // single-function device
                }
            }
        }
    }
    out
}
