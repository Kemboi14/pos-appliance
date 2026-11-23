//! Memory: physical/virtual translation through the HHDM, DMA buffers from
//! the heap, MMIO mapping into Limine's page tables, and a kernel stack with
//! an unmapped guard page.
//!
//! The heap is one physically contiguous region inside the HHDM, so any heap
//! allocation is physically contiguous and its physical address is
//! `virt - hhdm`. x86 DMA is cache-coherent, so no cache maintenance.

use alloc::alloc::{alloc_zeroed, dealloc};
use core::alloc::Layout;
use core::arch::asm;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU64, Ordering};

static HHDM: AtomicU64 = AtomicU64::new(0);

pub fn set_hhdm(off: u64) {
    HHDM.store(off, Ordering::Relaxed);
}

pub fn phys_to_virt(p: u64) -> u64 {
    HHDM.load(Ordering::Relaxed) + p
}

const P: u64 = 1;
const RW: u64 = 1 << 1;
const PWT: u64 = 1 << 3;
const PCD: u64 = 1 << 4;
const PS: u64 = 1 << 7;
const NX: u64 = 1 << 63;
const ADDR: u64 = 0x000f_ffff_ffff_f000;

/// A zeroed, physically contiguous buffer for device DMA. Freed on drop.
#[derive(Debug)]
pub struct Dma {
    ptr: NonNull<u8>,
    layout: Layout,
}

impl Dma {
    /// `align` must be a power of two; use `align >= len` (rounded up to a
    /// power of two) when the buffer must not cross that boundary (xHCI: 64
    /// KiB for transfer buffers, 4 KiB for rings and contexts).
    pub fn new(len: usize, align: usize) -> Option<Dma> {
        let layout = Layout::from_size_align(len.max(1), align).ok()?;
        // SAFETY: non-zero size.
        let ptr = NonNull::new(unsafe { alloc_zeroed(layout) })?;
        Some(Dma { ptr, layout })
    }
    pub fn len(&self) -> usize {
        self.layout.size()
    }
    pub fn virt(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }
    pub fn phys(&self) -> u64 {
        self.ptr.as_ptr() as u64 - HHDM.load(Ordering::Relaxed)
    }
    pub fn read32(&self, off: usize) -> u32 {
        assert!(off + 4 <= self.len());
        // SAFETY: in bounds (checked); the device may write it, hence volatile.
        unsafe { self.virt().add(off).cast::<u32>().read_volatile() }
    }
    pub fn write32(&self, off: usize, v: u32) {
        assert!(off + 4 <= self.len());
        // SAFETY: in bounds (checked); volatile so the device sees the store.
        unsafe { self.virt().add(off).cast::<u32>().write_volatile(v) }
    }
    pub fn write64(&self, off: usize, v: u64) {
        self.write32(off, v as u32);
        self.write32(off + 4, (v >> 32) as u32);
    }
    /// Copy out of the buffer (volatile: the device wrote it).
    pub fn copy_out(&self, dst: &mut [u8]) {
        assert!(dst.len() <= self.len());
        for (i, d) in dst.iter_mut().enumerate() {
            // SAFETY: i < dst.len() <= len.
            *d = unsafe { self.virt().add(i).read_volatile() };
        }
    }
    pub fn copy_in(&self, src: &[u8]) {
        assert!(src.len() <= self.len());
        for (i, s) in src.iter().enumerate() {
            // SAFETY: i < src.len() <= len.
            unsafe { self.virt().add(i).write_volatile(*s) };
        }
    }
}

impl Drop for Dma {
    fn drop(&mut self) {
        // SAFETY: allocated in `new` with this layout.
        unsafe { dealloc(self.ptr.as_ptr(), self.layout) }
    }
}

fn cr3() -> u64 {
    let v: u64;
    // SAFETY: reading CR3 has no side effects.
    unsafe { asm!("mov {}, cr3", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v & ADDR
}

fn invlpg(va: u64) {
    // SAFETY: flushing a TLB entry has no other effect.
    unsafe { asm!("invlpg [{}]", in(reg) va, options(nostack, preserves_flags)) };
}

/// Page-table pages are never freed.
fn new_table() -> Option<u64> {
    let t = Dma::new(4096, 4096)?;
    let p = t.phys();
    core::mem::forget(t);
    Some(p)
}

/// Map one 4 KiB page `va -> pa` with `flags`, creating intermediate tables.
/// Leaves an existing mapping (4 KiB or huge) untouched.
fn map_page(va: u64, pa: u64, flags: u64) -> Option<()> {
    let mut table = cr3();
    for level in [39u32, 30, 21] {
        let idx = ((va >> level) & 0x1ff) as usize;
        let entry = (phys_to_virt(table) as *mut u64).wrapping_add(idx);
        // SAFETY: `table` is a live page table reached from CR3 through the
        // HHDM (bootloader-reclaimable or our own heap pages, never reused).
        let e = unsafe { entry.read_volatile() };
        if e & P == 0 {
            let t = new_table()?;
            // SAFETY: as above.
            unsafe { entry.write_volatile(t | P | RW) };
            table = t;
        } else if e & PS != 0 {
            return Some(()); // covered by a huge page already
        } else {
            table = e & ADDR;
        }
    }
    let entry = (phys_to_virt(table) as *mut u64).wrapping_add(((va >> 12) & 0x1ff) as usize);
    // SAFETY: as above.
    unsafe {
        if entry.read_volatile() & P == 0 {
            entry.write_volatile((pa & ADDR) | flags);
            invlpg(va);
        }
    }
    Some(())
}

/// Map an MMIO range uncached (PCD|PWT), non-executable, at `hhdm + phys`.
/// Returns the virtual address of `phys`.
pub fn map_mmio(phys: u64, len: u64) -> Option<u64> {
    let start = phys & !0xfff;
    let end = (phys + len.max(1) + 0xfff) & !0xfff;
    let mut p = start;
    while p < end {
        map_page(phys_to_virt(p), p, P | RW | PCD | PWT | NX)?;
        p += 4096;
    }
    Some(phys_to_virt(phys))
}

/// Map ordinary RAM (e.g. firmware tables Limine did not map) read-only.
pub fn map_ram_ro(phys: u64, len: u64) -> Option<u64> {
    let start = phys & !0xfff;
    let end = (phys + len.max(1) + 0xfff) & !0xfff;
    let mut p = start;
    while p < end {
        map_page(phys_to_virt(p), p, P | NX)?;
        p += 4096;
    }
    Some(phys_to_virt(phys))
}

/// Map ordinary RAM writable (firmware-reserved pages we must scrub).
pub fn map_ram_rw(phys: u64, len: u64) -> Option<u64> {
    let start = phys & !0xfff;
    let end = (phys + len.max(1) + 0xfff) & !0xfff;
    let mut p = start;
    while p < end {
        map_page(phys_to_virt(p), p, P | RW | NX)?;
        p += 4096;
    }
    Some(phys_to_virt(phys))
}

/// Our stack lives outside the HHDM so the page below it can stay unmapped:
/// an overflow faults instead of corrupting the heap. The #PF then hits the
/// guard again and escalates to #DF, which runs on its own IST stack.
const STACK_TOP: u64 = 0xffff_fe00_0010_0000 + STACK_SIZE;
pub const STACK_SIZE: u64 = 1024 * 1024;

/// Switch to a guarded stack and continue in `f`. Never returns.
pub fn switch_stack(f: extern "C" fn() -> !) -> Option<core::convert::Infallible> {
    let mem = Dma::new(STACK_SIZE as usize, 4096)?;
    let base = mem.phys();
    core::mem::forget(mem);
    let bottom = STACK_TOP - STACK_SIZE;
    let mut off = 0;
    while off < STACK_SIZE {
        map_page(bottom + off, base + off, P | RW | NX)?;
        off += 4096;
    }
    // SAFETY: the new stack is mapped and 16-byte aligned; `f` never returns,
    // so nothing on the old stack is used again.
    unsafe {
        asm!(
            "mov rsp, {top}",
            "xor ebp, ebp",
            "call {f}",
            "ud2",
            top = in(reg) STACK_TOP,
            f = in(reg) f,
            options(noreturn)
        )
    }
}

/// Enable SMEP/SMAP when the CPU has them (no user pages exist, so this
/// only turns a class of exploitation bugs into faults).
pub fn harden_cr4() -> (bool, bool) {
    use core::arch::x86_64::{__cpuid, __cpuid_count};
    if __cpuid(0).eax < 7 {
        return (false, false);
    }
    let b = __cpuid_count(7, 0).ebx;
    let smep = b & (1 << 7) != 0;
    let smap = b & (1 << 20) != 0;
    let mut cr4: u64;
    // SAFETY: reading/writing CR4 bits the CPU advertises.
    unsafe {
        asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags));
        if smep {
            cr4 |= 1 << 20;
        }
        if smap {
            cr4 |= 1 << 21;
        }
        asm!("mov cr4, {}", in(reg) cr4, options(nostack, preserves_flags));
    }
    (smep, smap)
}
