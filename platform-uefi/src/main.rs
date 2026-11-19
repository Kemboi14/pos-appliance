//! Tier A: the POS appliance as a UEFI application.
//!
//! This binary never calls `ExitBootServices`; every device is driven through
//! firmware protocols. It opens no network protocol (SNP/MNP/IP4/TCP).
#![no_std]
#![no_main]

extern crate alloc;

mod display;

use core::time::Duration;
use pos_hal::Display;
use uefi::boot::{self, EventType, TimerTrigger, Tpl};
use uefi::prelude::*;
use uefi::runtime::{self, ResetType};

/// M0 test-pattern colours, checked by `cargo xtask qemu-smoke` via a QEMU
/// screendump.
const BG: u32 = 0x001E_3A5F;
const BOX: u32 = 0x00F0_A000;

#[entry]
fn main() -> Status {
    // The firmware arms a 5-minute watchdog before starting a boot option;
    // an appliance runs forever, so disable it.
    if boot::set_watchdog_timer(0, 0x10000, None).is_err() {
        return Status::ABORTED;
    }

    #[cfg(feature = "debug-console")]
    {
        let _ = uefi::helpers::init();
        log::info!("pos-uefi: debug console enabled");
    }

    let mut disp = match display::GopDisplay::init() {
        Ok(d) => d,
        Err(_) => fatal_reset(),
    };
    draw_test_pattern(&mut disp);

    // Event loop: block in WaitForEvent on keyboard + a 500 ms periodic tick.
    // SAFETY: no notify function or context is registered, so there are no
    // callback invariants to uphold.
    let tick = match unsafe { boot::create_event(EventType::TIMER, Tpl::CALLBACK, None, None) } {
        Ok(e) => e,
        Err(_) => fatal_reset(),
    };
    if boot::set_timer(&tick, TimerTrigger::Periodic(Duration::from_millis(500))).is_err() {
        fatal_reset();
    }
    let key_event = match uefi::system::with_stdin(|i| i.wait_for_key_event()) {
        Ok(e) => e,
        Err(_) => fatal_reset(),
    };
    let mut events = [key_event, tick];
    let mut ticks: u32 = 0;
    loop {
        match boot::wait_for_event(&mut events) {
            Ok(0) => {
                let key = uefi::system::with_stdin(|i| i.read_key()).ok().flatten();
                if let Some(uefi::proto::console::text::Key::Printable(c)) = key {
                    if char::from(c) == 'r' {
                        runtime::reset(ResetType::COLD, Status::SUCCESS, None);
                    }
                }
            }
            Ok(_) => {
                ticks = ticks.wrapping_add(1);
                // Blink a heartbeat square so liveness is visible.
                let c = if ticks % 2 == 0 { BOX } else { BG };
                let px = [c; 16 * 16];
                let _ = disp.blit(&px, 16, 8, 8, 16, 16);
            }
            Err(_) => fatal_reset(),
        }
    }
}

fn draw_test_pattern(d: &mut display::GopDisplay) {
    let (w, h) = (d.width(), d.height());
    let row: alloc::vec::Vec<u32> = alloc::vec![BG; w as usize];
    for y in 0..h {
        let _ = d.blit(&row, w as usize, 0, y, w, 1);
    }
    let (bw, bh) = (w / 4, h / 4);
    let boxpx = alloc::vec![BOX; (bw * bh) as usize];
    let _ = d.blit(&boxpx, bw as usize, (w - bw) / 2, (h - bh) / 2, bw, bh);
}

/// Unrecoverable platform failure: wait briefly so a technician can see the
/// screen state, then cold-reset. The store is crash-consistent, so a reset is
/// always safe.
fn fatal_reset() -> ! {
    boot::stall(Duration::from_secs(10));
    runtime::reset(ResetType::COLD, Status::DEVICE_ERROR, None)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    #[cfg(feature = "debug-console")]
    log::error!("panic: {_info}");
    fatal_reset()
}
