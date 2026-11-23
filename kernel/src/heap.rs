//! Kernel heap: the largest usable region from the Limine memory map,
//! accessed through the higher-half direct map.

use crate::limine::{self, MEMMAP_USABLE};
use linked_list_allocator::LockedHeap;

#[global_allocator]
static HEAP: LockedHeap = LockedHeap::empty();

/// Argon2 alone needs 19 MiB; the canvas and store caches a few more.
pub const MIN_HEAP: u64 = 64 * 1024 * 1024;
const MAX_HEAP: u64 = 512 * 1024 * 1024;

/// Returns the heap size in bytes, or `None` if no region is large enough.
pub fn init(hhdm: u64) -> Option<u64> {
    let mm = limine::MEMMAP.response()?;
    let best = mm
        .entries()
        .filter(|e| e.kind == MEMMAP_USABLE && e.base >= 0x10_0000)
        .max_by_key(|e| e.length)?;
    if best.length < MIN_HEAP {
        return None;
    }
    let size = best.length.min(MAX_HEAP);
    // SAFETY: the region is usable RAM per the bootloader, mapped in the HHDM,
    // and handed to the allocator exactly once.
    unsafe {
        HEAP.lock()
            .init((hhdm + best.base) as *mut u8, size as usize);
    }
    Some(size)
}
