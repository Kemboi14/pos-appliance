//! Tier A: the POS appliance as a UEFI application.
//!
//! * Never calls `ExitBootServices`; every device is driven through firmware
//!   protocols.
//! * Opens no network protocol unless the till is configured into a store
//!   network; then it takes the Simple Network Protocol exclusively (the
//!   firmware's MNP/IP/PXE stack is disconnected) and runs its own
//!   encrypted store link (docs/NETWORK.md, docs/HARDENING.md).
//! * Single-threaded: blocks in `WaitForEvent` on keyboard, pointer and a
//!   periodic timer (20 ms while networked, 1 s otherwise); no busy loops.
#![no_std]
#![no_main]

extern crate alloc;

mod block;
mod clock;
mod display;
mod input;
mod keyvar;
mod media;
mod net;
mod printer;
mod rng;
mod speaker;
mod wifi;

use core::time::Duration;
use pos_app::{App, Control, Hw};
use pos_hal::Display;
use uefi::boot::{
    self, EventType, OpenProtocolAttributes, OpenProtocolParams, ScopedProtocol, TimerTrigger, Tpl,
};
use uefi::prelude::*;
use uefi::runtime::{self, ResetType};

/// Open a protocol without taking ownership (GetProtocol). Used for
/// consoles and device paths, which the firmware shares between drivers.
pub(crate) fn open_shared<P: uefi::proto::ProtocolPointer + ?Sized>(
    h: uefi::Handle,
) -> Option<ScopedProtocol<P>> {
    let params = OpenProtocolParams {
        handle: h,
        agent: boot::image_handle(),
        controller: None,
    };
    // SAFETY: GetProtocol takes no ownership; these protocols stay installed
    // for the lifetime of boot services, which this app never exits.
    unsafe { boot::open_protocol::<P>(params, OpenProtocolAttributes::GetProtocol) }.ok()
}

#[entry]
fn main() -> Status {
    // The firmware arms a 5-minute watchdog before starting a boot option;
    // an appliance runs indefinitely.
    if boot::set_watchdog_timer(0, 0x10000, None).is_err() {
        return Status::ABORTED;
    }
    #[cfg(feature = "debug-console")]
    {
        let _ = uefi::helpers::init();
        log::info!("pos-uefi: debug console enabled");
    }

    let Ok(mut disp) = display::GopDisplay::init() else {
        fatal_reset()
    };
    let (w, h) = (disp.width(), disp.height());
    let clock = clock::TscClock::calibrate();
    let mut entropy = rng::Entropy::init();
    let (key, key_src) = keyvar::machine_key(&mut entropy);
    let mut printer = printer::UefiPrinter::init();
    let mut media = media::UsbMedia::new();
    let mut wifi = wifi::UefiWifi::init();
    let mut speaker = speaker::Speaker;
    let Some(mut input) = input::UefiInput::init(w, h) else {
        fatal_screen(&mut disp, "No keyboard input available")
    };
    #[cfg(feature = "debug-console")]
    log::info!(
        "display {w}x{h}, rng {:?}, key {:?}, printer {}",
        entropy.source,
        key_src,
        pos_hal::Printer::describe(&printer)
    );

    let dev = match block::find_data_partition() {
        Ok(d) => d,
        Err(e) => fatal_screen(
            &mut disp,
            match e {
                block::FindError::NoBootDevice => "Cannot identify the boot device",
                block::FindError::NotFound => "POS data partition not found on the boot stick",
                block::FindError::OpenFailed => "POS data partition is read-only or unavailable",
            },
        ),
    };

    let (mut app, _status) = App::boot(dev, key, w, h);
    if key_src == keyvar::KeySource::Fallback {
        app.notice(
            "WARNING: firmware NVRAM/entropy unavailable - store binding disabled",
            true,
        );
    }
    if entropy.source == rng::Source::None {
        app.notice("WARNING: no hardware entropy source", true);
    }

    // SAFETY: no notify function/context: nothing to uphold.
    let Ok(tick) = (unsafe { boot::create_event(EventType::TIMER, Tpl::CALLBACK, None, None) })
    else {
        fatal_reset()
    };
    if boot::set_timer(&tick, TimerTrigger::Periodic(Duration::from_secs(1))).is_err() {
        fatal_reset();
    }
    let mut events = input.wait_events();
    let tick_index = events.len();
    events.push(tick);

    {
        let mut hw = Hw {
            display: &mut disp,
            clock: &clock,
            printer: &mut printer,
            rng: &mut entropy,
            media: &mut media,
            wifi: &mut wifi,
            sound: &mut speaker,
        };
        app.render(&mut hw);
    }

    let mut network: Option<pos_app::sync::Network<net::UefiNic>> = None;
    let mut nic_missing = false;
    let mut fast = false;
    let mut last_tick_us = clock_now(&clock);
    loop {
        let which = boot::wait_for_event(&events).unwrap_or(tick_index);
        input.pump(&clock);
        // Networked: wake every 20 ms to run the stack.
        let want_fast = app.node_config().is_some();
        if want_fast != fast {
            fast = want_fast;
            let period = if fast {
                Duration::from_millis(20)
            } else {
                Duration::from_secs(1)
            };
            let _ = boot::set_timer(&events[tick_index], TimerTrigger::Periodic(period));
        }
        if fast && network.is_none() && !nic_missing && app.wifi_settled() {
            match net::UefiNic::init(app.wifi_preferred()) {
                Some(n) => network = Some(pos_app::sync::Network::new(n)),
                None => {
                    nic_missing = true;
                    app.notice("Network: no usable network adapter found", true);
                }
            }
        }
        let mut hw = Hw {
            display: &mut disp,
            clock: &clock,
            printer: &mut printer,
            rng: &mut entropy,
            media: &mut media,
            wifi: &mut wifi,
            sound: &mut speaker,
        };
        if let Some(n) = &mut network {
            n.step(&mut app, &mut hw, clock_now(&clock) / 1000);
        }
        while let Some(ev) = pos_hal::Input::poll(&mut input) {
            if app.handle(ev, &mut hw) == Control::Reset {
                app.render(&mut hw);
                boot::stall(Duration::from_secs(3));
                runtime::reset(ResetType::COLD, Status::DEVICE_ERROR, None);
            }
        }
        let now = clock_now(&clock);
        if which == tick_index && now.saturating_sub(last_tick_us) >= 1_000_000 {
            last_tick_us = now;
            app.tick(&mut hw);
        }
        app.render(&mut hw);
    }
}

fn clock_now(c: &clock::TscClock) -> u64 {
    pos_hal::Clock::monotonic_us(c)
}

/// Show a platform error full-screen and wait for a key, then reset.
fn fatal_screen(disp: &mut display::GopDisplay, msg: &str) -> ! {
    let mut c = pos_ui::Canvas::new(disp.width(), disp.height());
    let b = c.bounds();
    c.fill_rect(b, pos_ui::THEME.danger);
    let (top, rest) = b.split_top(b.h / 2);
    c.text_in(top, "POS cannot start", pos_ui::Font::Large, 0x00FF_FFFF, 1);
    c.text_in(
        rest.split_top(40).0,
        msg,
        pos_ui::Font::Normal,
        0x00FF_FFFF,
        1,
    );
    c.text_in(
        rest.split_top(100).0.split_bottom(30).1,
        "Press any key to restart",
        pos_ui::Font::Normal,
        0x00FF_FFFF,
        1,
    );
    let _ = c.present(disp);
    if let Ok(ev) = uefi::system::with_stdin(|i| i.wait_for_key_event()) {
        let evs = [ev];
        let _ = boot::wait_for_event(&evs);
    }
    runtime::reset(ResetType::COLD, Status::DEVICE_ERROR, None)
}

/// Unrecoverable failure before the display works: pause, then cold reset.
/// The store is crash-consistent, so a reset is always safe.
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
