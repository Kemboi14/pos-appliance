# Tier B (own kernel) — plan, not started

Tier B (M7+) has not been implemented. It starts after Tier A passes the
real-hardware checklist in HARDENING.md. A placeholder crate was removed so
the workspace contains only working code.

## Boot: Limine

Limine over the `bootloader` crate: it hands over a linear framebuffer, the
memory map, RSDP and a higher-half direct map via a stable protocol, it boots
from the same FAT ESP, and it can be Secure-Boot-signed together with the
kernel (Limine enforces a config hash/kernel checksum). `bootloader` ties the
build to its own image tooling and is less flexible about the ESP layout we
already produce.

## Architecture

* Higher-half kernel, 4-level paging, W^X, NX, guard pages around stacks, SMEP/SMAP when CPUID reports them.
* GDT/TSS (IST stacks for #DF/#NMI/#MC), IDT, Local APIC + IOAPIC from the ACPI MADT (`acpi` crate); timer = LAPIC timer calibrated against HPET or PIT.
* Single address space, cooperative async executor; the POS runs as kernel tasks. `pos-app` is used unchanged: Tier B only supplies new HAL objects.

## Driver order (each behind the same pos-hal trait, each with a QEMU test)

1. Framebuffer `Display` from Limine (reuses the GOP blit logic) + CMOS RTC `Clock` + RDRAND/RDSEED `Rng` → app boots to the setup screen with a RAM `BlockDevice`.
2. xHCI: controller reset, command/event rings, slot/endpoint contexts, port enumeration, control transfers, then interrupt and bulk endpoints. Test: enumerate QEMU `qemu-xhci` devices.
3. USB HID boot keyboard (`Input`), then report-protocol digitizer (usb-tablet) for touch.
4. USB Mass Storage BOT + SCSI (INQUIRY, READ CAPACITY(16), READ/WRITE(10/16), SYNCHRONIZE CACHE) → `BlockDevice` over the data partition (GPT parsed by us). Unlike UEFI, flush issues SYNCHRONIZE CACHE. Re-run the M2 fault-injection suite's QEMU variant.
5. USB Printer class bulk OUT → `Printer`; second MSC stick + FAT (`fatfs` no_std) → `ExternalMedia`.
6. Power: ACPI reset register / keyboard-controller reset.

Rough size: 4–8 engineer-weeks for 1–5 with QEMU tests, then the same hardware checklist as Tier A.
