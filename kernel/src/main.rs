//! Tier B: the POS appliance on its own kernel, booted by Limine (chained
//! from the signed `pos-stage0` loader, which hands over the store key).
//!
//! Bring-up: own GDT/IDT, heap, guarded stack, SMEP/SMAP, framebuffer,
//! TSC/RTC clock, LAPIC timer, ACPI reset, xHCI with HID keyboard/pointer,
//! mass storage (data partition on the boot stick, FAT export stick) and
//! USB printer (COM2 serial fallback), and — only when the till is
//! configured into a store network — virtio-net / Intel e1000 Ethernet.
//! `pos-app` runs unchanged on these HAL objects. Single CPU; device work happens in the main loop with
//! interrupts off, and the CPU sleeps in `hlt` between 1 kHz timer ticks.
#![no_std]
#![no_main]

extern crate alloc;

mod acpi;
mod apic;
mod clock;
mod cpu;
mod efikey;
mod fb;
mod heap;
mod limine;
mod mm;
mod nic;
mod pci;
mod power;
mod rng;
mod serial;
mod usb;
mod xhci;

use pos_app::{App, Control, Hw};
use pos_hal::{Clock, Display, Input};

#[unsafe(no_mangle)]
extern "C" fn kmain() -> ! {
    if cfg!(feature = "debug-console") {
        let _ = serial::Uart::init(serial::COM1);
    }
    klog!("pos-kernel: start");
    cpu::init();
    if !limine::BASE.supported() {
        klog!("bootloader lacks base revision {}", limine::BASE_REVISION);
        power::fatal_reset();
    }
    let Some(hhdm) = limine::HHDM.response().map(|r| r.offset) else {
        klog!("no HHDM response");
        power::fatal_reset()
    };
    mm::set_hhdm(hhdm);
    let Some(heap) = heap::init(hhdm) else {
        klog!("no {} MiB region for the heap", heap::MIN_HEAP >> 20);
        power::fatal_reset()
    };
    klog!("heap {} MiB", heap >> 20);
    let _ = mm::switch_stack(kmain2);
    klog!("cannot allocate the kernel stack");
    power::fatal_reset()
}

extern "C" fn kmain2() -> ! {
    let (smep, smap) = mm::harden_cr4();
    let Some(mut disp) = limine::FRAMEBUFFER
        .response()
        .and_then(|r| r.first())
        .and_then(|f| fb::FbDisplay::new(f).ok())
    else {
        klog!("no usable framebuffer");
        power::fatal_reset()
    };
    let (w, h) = (disp.width(), disp.height());
    let clock = clock::TscClock::calibrate();
    let Some(apic) = apic::init(clock.ticks_per_us()) else {
        fatal_screen(&mut disp, "Local APIC unavailable")
    };
    let acpi_reset = limine::RSDP
        .response()
        .is_some_and(|r| acpi::init(r.address));
    let (key, key_src) = efikey::take();
    let mut entropy = rng::Entropy::init();
    let boot_disk = limine::EXECUTABLE_FILE
        .response()
        .and_then(|r| r.file())
        .map(|f| f.gpt_disk_uuid)
        .filter(|g| *g != [0; 16]);
    klog!(
        "display {w}x{h} {:?}; tsc {}/us {:?} invariant={}; smep={smep} smap={smap}; \
         x2apic={} timer {}/ms; acpi reset={acpi_reset}; rng {:?}; key {:?}; rtc {:?}",
        disp.pixel_format(),
        clock.ticks_per_us(),
        clock.calibration,
        clock.invariant,
        apic.x2apic,
        apic.timer_per_ms,
        entropy.source,
        key_src,
        clock.wall()
    );

    let stack = usb::Stack::init(w, h, boot_disk);
    if stack.borrow().controllers() == 0 {
        fatal_screen(&mut disp, "No USB (xHCI) controller found");
    }
    let dev = match usb::BootDisk::open(&stack) {
        Ok(d) => d,
        Err(e) => fatal_screen(
            &mut disp,
            match e {
                pos_hal::HalError::NotFound => "POS data partition not found on the boot stick",
                _ => "Boot stick not found or not readable over USB",
            },
        ),
    };
    let mut input = usb::UsbInput {
        stack: stack.clone(),
    };
    let mut media = usb::UsbMedia {
        stack: stack.clone(),
    };
    let mut printer = usb::KernelPrinter {
        stack: stack.clone(),
        serial: serial::SerialPrinter::init(),
    };

    let (mut app, status) = App::boot(dev, key, w, h);
    klog!("app boot: {status:?}");
    if matches!(
        key_src,
        efikey::KeySource::Fallback | efikey::KeySource::Missing
    ) {
        app.notice(
            "WARNING: firmware NVRAM/entropy unavailable - store binding disabled",
            true,
        );
    }
    if entropy.source == rng::Source::None {
        app.notice("WARNING: no hardware entropy source", true);
    }

    let mut next_tick = clock.monotonic_us();
    let mut network: Option<pos_app::sync::Network<nic::KernelNic>> = None;
    let mut nic_missing = false;
    loop {
        if network.is_none() && !nic_missing && app.node_config().is_some() {
            match nic::KernelNic::probe() {
                Some(n) => {
                    klog!("network: {} {:02x?}", n.describe(), pos_hal::Nic::mac(&n));
                    network = Some(pos_app::sync::Network::new(n));
                }
                None => {
                    nic_missing = true;
                    app.notice("Network: no supported network adapter found", true);
                }
            }
        }
        let mut hw = Hw {
            display: &mut disp,
            clock: &clock,
            printer: &mut printer,
            rng: &mut entropy,
            media: &mut media,
        };
        if let Some(n) = &mut network {
            n.step(&mut app, &mut hw, clock.monotonic_us() / 1000);
        }
        while let Some(ev) = input.poll() {
            if app.handle(ev, &mut hw) == Control::Reset {
                app.render(&mut hw);
                clock::spin_ms(3000);
                power::reset();
            }
        }
        let now = clock.monotonic_us();
        if now >= next_tick {
            next_tick = now + 1_000_000;
            app.tick(&mut hw);
        }
        app.render(&mut hw);
        cpu::wait_for_interrupt();
    }
}

/// Show a platform error full-screen, then reset after a pause.
fn fatal_screen(disp: &mut fb::FbDisplay, msg: &str) -> ! {
    klog!("fatal: {msg}");
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
    let _ = c.present(disp);
    power::fatal_reset()
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    klog!("panic: {info}");
    let _ = info;
    power::fatal_reset()
}
