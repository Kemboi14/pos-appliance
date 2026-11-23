//! Tier B stage 0: the first signed UEFI image on the stick.
//!
//! 1. Reads the per-machine store key from the boot-services-only UEFI
//!    variable `PosStoreKey` (the same variable Tier A uses), or creates it.
//! 2. Copies it into one EfiReservedMemoryType page, publishes that page as
//!    an EFI configuration table (GUID below), and wipes its own copy.
//! 3. Loads and starts `\EFI\BOOT\LIMINE.EFI` from the same partition.
//!    Under Secure Boot the firmware verifies Limine's signature (our db
//!    key). Limine then checks its enrolled config hash, and the config
//!    pins the kernel's hash.
//!
//! The kernel finds the page through the EFI system table, reads the key
//! and scrubs the page (kernel/src/efikey.rs). Nothing else in the boot
//! chain can read the key: the variable is not accessible at runtime, and
//! the only images that run are ours.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use core::arch::x86_64::{__cpuid, __cpuid_count, _rdrand64_step, _rdseed64_step};
use core::time::Duration;
use uefi::boot::{
    self, AllocateType, LoadImageSource, MemoryType, OpenProtocolAttributes, OpenProtocolParams,
};
use uefi::prelude::*;
use uefi::proto::BootPolicy;
use uefi::proto::device_path::DevicePath;
use uefi::proto::device_path::build::{self, DevicePathBuilder};
use uefi::proto::loaded_image::LoadedImage;
use uefi::proto::rng::Rng;
use uefi::runtime::{self, ResetType, VariableAttributes, VariableVendor};
use uefi::{Guid, cstr16, guid};

/// Must match `kernel/src/efikey.rs`.
static TABLE_GUID: Guid = guid!("6f1c2b9e-4a07-4d3b-9b52-8e0f7a1d3c64");
/// Must match Tier A (`platform-uefi/src/keyvar.rs`).
const VENDOR: VariableVendor = VariableVendor(guid!("5b0f9c4e-2a7d-4f61-9e83-1c6d2b8a4f70"));

#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Existing = 0,
    Generated = 1,
    Fallback = 2,
}

#[target_feature(enable = "rdseed")]
fn rdseed() -> Option<u64> {
    let mut v = 0u64;
    (0..100).find_map(|_| (_rdseed64_step(&mut v) == 1).then_some(v))
}

#[target_feature(enable = "rdrand")]
fn rdrand() -> Option<u64> {
    let mut v = 0u64;
    (0..10).find_map(|_| (_rdrand64_step(&mut v) == 1).then_some(v))
}

/// EFI RNG protocol, else RDSEED, else RDRAND, with the same health checks
/// as the platforms. `false` = no trustworthy entropy.
fn fill_random(buf: &mut [u8; 32]) -> bool {
    if let Ok(h) = boot::get_handle_for_protocol::<Rng>()
        && let Ok(mut rng) = boot::open_protocol_exclusive::<Rng>(h)
        && rng.get_rng(None, buf).is_ok()
    {
        return buf.iter().any(|&b| b != 0);
    }
    let has_seed = __cpuid(0).eax >= 7 && __cpuid_count(7, 0).ebx & (1 << 18) != 0;
    let has_rand = __cpuid(1).ecx & (1 << 30) != 0;
    let mut last = None;
    for chunk in buf.chunks_mut(8) {
        // SAFETY: CPU support checked just above.
        let w = unsafe {
            if has_seed {
                rdseed()
            } else if has_rand {
                rdrand()
            } else {
                None
            }
        };
        let Some(w) = w else { return false };
        if w == 0 || w == u64::MAX || Some(w) == last {
            return false;
        }
        last = Some(w);
        chunk.copy_from_slice(&w.to_le_bytes());
    }
    true
}

fn machine_key() -> ([u8; 32], Source) {
    let name = cstr16!("PosStoreKey");
    let mut buf = [0u8; 32];
    if let Ok((v, _)) = runtime::get_variable(name, &VENDOR, &mut buf)
        && v.len() == 32
    {
        let mut k = [0u8; 32];
        k.copy_from_slice(v);
        buf.fill(0);
        return (k, Source::Existing);
    }
    let mut k = [0u8; 32];
    if !fill_random(&mut k) {
        return ([0u8; 32], Source::Fallback);
    }
    let attrs = VariableAttributes::NON_VOLATILE | VariableAttributes::BOOTSERVICE_ACCESS;
    match runtime::set_variable(name, &VENDOR, attrs, &k) {
        Ok(()) => (k, Source::Generated),
        Err(_) => ([0u8; 32], Source::Fallback),
    }
}

/// Publish the key page. Layout: "POSKEY01", version u32 = 1, source u32,
/// key[32].
fn hand_over(key: &mut [u8; 32], src: Source) -> uefi::Result {
    let page = boot::allocate_pages(AllocateType::AnyPages, MemoryType::RESERVED, 1)?;
    let p = page.as_ptr();
    // SAFETY: we own the freshly allocated 4 KiB page.
    unsafe {
        core::ptr::write_bytes(p, 0, 4096);
        core::ptr::copy_nonoverlapping(b"POSKEY01".as_ptr(), p, 8);
        core::ptr::copy_nonoverlapping(1u32.to_le_bytes().as_ptr(), p.add(8), 4);
        core::ptr::copy_nonoverlapping((src as u32).to_le_bytes().as_ptr(), p.add(12), 4);
        core::ptr::copy_nonoverlapping(key.as_ptr(), p.add(16), 32);
        boot::install_configuration_table(&TABLE_GUID, p.cast())?;
    }
    key.fill(0);
    Ok(())
}

fn start_limine() -> uefi::Result {
    let li = boot::open_protocol_exclusive::<LoadedImage>(boot::image_handle())?;
    let dev = li.device().ok_or(uefi::Status::NOT_FOUND)?;
    let params = OpenProtocolParams {
        handle: dev,
        agent: boot::image_handle(),
        controller: None,
    };
    // SAFETY: GetProtocol takes no ownership; the device path stays
    // installed while boot services run.
    let dp =
        unsafe { boot::open_protocol::<DevicePath>(params, OpenProtocolAttributes::GetProtocol) }?;
    let mut v = Vec::new();
    let mut b = DevicePathBuilder::with_vec(&mut v);
    for node in dp.node_iter() {
        b = b.push(&node).map_err(|_| uefi::Status::OUT_OF_RESOURCES)?;
    }
    let path = b
        .push(&build::media::FilePath {
            path_name: cstr16!("\\EFI\\BOOT\\LIMINE.EFI"),
        })
        .and_then(DevicePathBuilder::finalize)
        .map_err(|_| uefi::Status::OUT_OF_RESOURCES)?;
    drop(li);
    let h = boot::load_image(
        boot::image_handle(),
        LoadImageSource::FromDevicePath {
            device_path: path,
            boot_policy: BootPolicy::ExactMatch,
        },
    )?;
    boot::start_image(h)
}

#[entry]
fn main() -> Status {
    let (mut key, src) = machine_key();
    // A failed handover is not fatal: the kernel then reports the binding
    // as disabled, exactly like Tier A without NVRAM.
    let _ = hand_over(&mut key, src);
    key.fill(0);
    let err = start_limine().err().map_or(Status::ABORTED, |e| e.status());
    // Only reached if Limine is missing, unsigned (Secure Boot) or failed.
    uefi::system::with_stdout(|o| {
        let _ = o.output_string(cstr16!(
            "POS: cannot start the boot loader (LIMINE.EFI). Restarting.\r\n"
        ));
    });
    boot::stall(Duration::from_secs(10));
    runtime::reset(ResetType::COLD, err, None)
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    boot::stall(Duration::from_secs(10));
    runtime::reset(ResetType::COLD, Status::ABORTED, None)
}
