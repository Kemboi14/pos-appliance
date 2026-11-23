//! Minimal Limine boot protocol bindings (only the requests we use).
//!
//! Written here instead of using the `limine` crate because that crate needs
//! nightly (`ptr_metadata`) and this project builds on stable only. IDs and
//! layouts follow the Limine protocol specification.

use core::cell::UnsafeCell;

const COMMON_MAGIC: [u64; 2] = [0xc7b1_dd30_df4c_8b88, 0x0a82_e883_a194_f07b];

/// Base revision we are written against. In revision 3 the HHDM covers only
/// usable, bootloader, executable and framebuffer memory, so MMIO has to be
/// mapped by the kernel itself (xHCI, step 2).
pub const BASE_REVISION: u64 = 3;

#[repr(C)]
pub struct BaseRevision(UnsafeCell<[u64; 3]>);
// SAFETY: written once by the bootloader before the kernel runs; read-only after.
unsafe impl Sync for BaseRevision {}

impl BaseRevision {
    const fn new(rev: u64) -> Self {
        Self(UnsafeCell::new([
            0xf956_2b2d_5c95_a6c8,
            0x6a7b_3849_4453_6bdc,
            rev,
        ]))
    }
    /// The bootloader zeroes the last word when it accepts the revision.
    pub fn supported(&self) -> bool {
        // SAFETY: the static is valid for the whole program; volatile because
        // the bootloader wrote it outside the compiler's view.
        unsafe { (self.0.get() as *const u64).add(2).read_volatile() == 0 }
    }
}

#[repr(C)]
pub struct Request<R: 'static, X = ()> {
    id: [u64; 4],
    revision: u64,
    response: UnsafeCell<*const R>,
    extra: X,
}
// SAFETY: as for BaseRevision.
unsafe impl<R, X> Sync for Request<R, X> {}

impl<R, X> Request<R, X> {
    const fn new(id: [u64; 2], extra: X) -> Self {
        Self {
            id: [COMMON_MAGIC[0], COMMON_MAGIC[1], id[0], id[1]],
            revision: 0,
            response: UnsafeCell::new(core::ptr::null()),
            extra,
        }
    }
    pub fn response(&self) -> Option<&'static R> {
        // SAFETY: the bootloader stores either null or a pointer to a response
        // in bootloader-reclaimable memory, which the kernel never reuses.
        unsafe { self.response.get().read_volatile().as_ref() }
    }
}

#[repr(C)]
pub struct HhdmResponse {
    pub revision: u64,
    pub offset: u64,
}

#[repr(C)]
pub struct Framebuffer {
    pub address: *mut u8,
    pub width: u64,
    pub height: u64,
    pub pitch: u64,
    pub bpp: u16,
    pub memory_model: u8,
    pub red_mask_size: u8,
    pub red_mask_shift: u8,
    pub green_mask_size: u8,
    pub green_mask_shift: u8,
    pub blue_mask_size: u8,
    pub blue_mask_shift: u8,
}

#[repr(C)]
pub struct FramebufferResponse {
    pub revision: u64,
    count: u64,
    framebuffers: *const *const Framebuffer,
}

impl FramebufferResponse {
    pub fn first(&self) -> Option<&'static Framebuffer> {
        if self.count == 0 {
            return None;
        }
        // SAFETY: `count > 0`, so entry 0 is a valid pointer per the protocol.
        unsafe { (*self.framebuffers).as_ref() }
    }
}

pub const MEMMAP_USABLE: u64 = 0;

#[repr(C)]
pub struct MemmapEntry {
    pub base: u64,
    pub length: u64,
    pub kind: u64,
}

#[repr(C)]
pub struct MemmapResponse {
    pub revision: u64,
    count: u64,
    entries: *const *const MemmapEntry,
}

impl MemmapResponse {
    pub fn entries(&self) -> impl Iterator<Item = &'static MemmapEntry> + '_ {
        (0..self.count as usize).filter_map(move |i| {
            // SAFETY: i < count; each entry pointer is valid per the protocol.
            unsafe { (*self.entries.add(i)).as_ref() }
        })
    }
}

#[repr(C)]
pub struct StackSizeResponse {
    pub revision: u64,
}

#[used]
#[unsafe(link_section = ".limine_requests_start")]
static START: [u64; 4] = [
    0xf6b8_f4b3_9de7_d1ae,
    0xfab9_1a69_40fc_b9cf,
    0x785c_6ed0_15d3_e316,
    0x181e_920a_7852_b9d9,
];

#[used]
#[unsafe(link_section = ".limine_requests")]
pub static BASE: BaseRevision = BaseRevision::new(BASE_REVISION);

#[used]
#[unsafe(link_section = ".limine_requests")]
pub static HHDM: Request<HhdmResponse> =
    Request::new([0x48dc_f1cb_8ad2_b852, 0x6398_4e95_9a98_244b], ());

#[used]
#[unsafe(link_section = ".limine_requests")]
pub static FRAMEBUFFER: Request<FramebufferResponse> =
    Request::new([0x9d58_27dc_d881_dd75, 0xa314_8604_f6fa_b11b], ());

#[used]
#[unsafe(link_section = ".limine_requests")]
pub static MEMMAP: Request<MemmapResponse> =
    Request::new([0x67cf_3d9d_378a_806f, 0xe304_acdf_c50c_3c62], ());

/// Argon2 and the UI keep large values on the stack in debug builds.
pub const STACK_SIZE: u64 = 1024 * 1024;

#[used]
#[unsafe(link_section = ".limine_requests")]
pub static STACK: Request<StackSizeResponse, u64> =
    Request::new([0x224e_f046_0a8e_8926, 0xe1cb_0fc2_5f46_ea3d], STACK_SIZE);

/// Physical address of the EFI system table (base revision 3).
#[repr(C)]
pub struct EfiSystemTableResponse {
    pub revision: u64,
    pub address: u64,
}

#[used]
#[unsafe(link_section = ".limine_requests")]
pub static EFI_SYSTEM_TABLE: Request<EfiSystemTableResponse> =
    Request::new([0x5ceb_a516_3eaa_f6d6, 0x0a69_8161_0cf6_5fcc], ());

/// Physical address of the ACPI RSDP (base revision 3).
#[repr(C)]
pub struct RsdpResponse {
    pub revision: u64,
    pub address: u64,
}

#[used]
#[unsafe(link_section = ".limine_requests")]
pub static RSDP: Request<RsdpResponse> =
    Request::new([0xc5e7_7b6b_397e_7b43, 0x2763_7845_accd_cf3c], ());

#[repr(C)]
pub struct File {
    pub revision: u64,
    pub address: u64,
    pub size: u64,
    pub path: u64,
    pub cmdline: u64,
    pub media_type: u32,
    /// Present in the protocol spec (the `limine` crate omits it, which
    /// shifts every later field by 4 bytes).
    pub unused: u32,
    pub tftp_ip: u32,
    pub tftp_port: u32,
    pub partition_index: u32,
    pub mbr_disk_id: u32,
    /// On-disk (mixed-endian) GUID of the disk the kernel was loaded from.
    pub gpt_disk_uuid: [u8; 16],
    pub gpt_part_uuid: [u8; 16],
    pub part_uuid: [u8; 16],
}

#[repr(C)]
pub struct ExecutableFileResponse {
    pub revision: u64,
    file: *const File,
}

impl ExecutableFileResponse {
    pub fn file(&self) -> Option<&'static File> {
        // SAFETY: valid per the protocol when a response exists.
        unsafe { self.file.as_ref() }
    }
}

#[used]
#[unsafe(link_section = ".limine_requests")]
pub static EXECUTABLE_FILE: Request<ExecutableFileResponse> =
    Request::new([0xad97_e90e_83f1_ed67, 0x31eb_5d1c_5ff2_3b69], ());

#[used]
#[unsafe(link_section = ".limine_requests_end")]
static END: [u64; 2] = [0xadc0_e053_1bb1_0d03, 0x9572_709f_3176_4c62];
