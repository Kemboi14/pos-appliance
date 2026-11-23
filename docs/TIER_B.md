# Tier B (own kernel) — status and plan

Tier B (M7+) was started ahead of the Tier A real-hardware checklist in
HARDENING.md, at the owner's request; that validation runs in parallel.
Tier A stays the shipping configuration until Tier B passes the same
checklist.

## Status

| Step | State |
|------|-------|
| 1. Framebuffer, RTC, RNG | Done (2026-11-23) |
| 2. xHCI (+ PCI, LAPIC timer, MMIO mapping) | Done (2026-11-23) |
| 3. USB HID keyboard / absolute pointer | Done (2026-11-23) |
| 4. USB MSC → data partition | Done (2026-11-23) |
| 5. USB printer class, FAT export stick, hot-plug, USB 2.0 hubs | Done (2026-11-23) |
| 6. ACPI reset | Done (2026-11-23) |
| Boot chain: signed stage 0 → signed Limine (config enrolled) → hashed kernel | Done (2026-11-23) |
| Store key handover (same NVRAM key as Tier A) | Done (2026-11-23) |
| Store network: virtio-net, Intel e1000/e1000e (82540/82545/82574) | Done (2026-11-23), QEMU only |
| Intel I217/I218/I219 (PCH LAN), Realtek | **Not supported yet** |
| Real-hardware checklist (HARDENING.md) | **Not started** |

QEMU evidence, all in `cargo xtask ci`:

* `smoke --kernel`: boots to the POS UI from a stick on a root port.
* `e2e --kernel --hub`: the full Tier A end-to-end script on Tier B. The
  keyboard, tablet, boot stick and export stick all sit behind a USB hub.
  The export stick is hot-plugged at runtime before the catalog import.
  The script covers setup, sale, receipt and drawer kick on COM2, a power
  cut, reboot with the store intact, and CSV export with long file names,
  read back on the host.
* `tamper-test`: a one-byte kernel change is refused (kernel hash in
  `limine.conf`). A rewritten `limine.conf` is refused too (config hash
  enrolled in `LIMINE.EFI`).
* `key-test`: stage 0 creates the NVRAM key on first boot and hands the
  same key over on the next boot.
* `sb-test --kernel`: OVMF with Secure Boot enforced and our keys only.
  The signed chain boots; a signed stage 0 refuses an unsigned Limine; an
  unsigned stage 0 is refused.
* `repro`: the Tier B image is bit-identical across two clean builds.
* Host tests in `pos-drivers`: HID, USB descriptors, SCSI/BOT, GPT/MBR,
  RTC, and the FAT driver against `fatfs`-made volumes. Every written
  volume is checked with `fsck.fat -n`.

## As built

### Boot chain and ESP layout

```text
EFI/BOOT/BOOTX64.EFI   pos-stage0   (signed, db key)
EFI/BOOT/LIMINE.EFI    Limine 12.9  (config hash enrolled, then signed)
limine.conf            timeout 0, editor off, kernel path pinned by BLAKE2B
boot/pos-kernel        ELF, higher half
```

* **Stage 0** (`stage0/`, ~200 lines, UEFI app) does three things:
  1. Reads or creates the boot-services-only variable `PosStoreKey`, the
     same variable Tier A uses.
  2. Copies the key into one `EfiReservedMemoryType` page and publishes
     the page as an EFI configuration table
     (`6f1c2b9e-4a07-4d3b-9b52-8e0f7a1d3c64`).
  3. Starts `LIMINE.EFI` with `LoadImage`, so the firmware verifies
     Limine's signature.

  The kernel (`efikey.rs`) finds the table through Limine's EFI
  system-table response, copies the key and scrubs the page. Moving a
  stick between Tier A and Tier B on the same till keeps the store
  trusted. Without stage 0, or without NVRAM, the app shows the same
  "binding disabled" warning as Tier A.
* **Signing:** `cargo xtask sign --kernel` signs stage 0 and the enrolled
  Limine with the same db key as Tier A, so the Secure Boot enrollment
  steps in HARDENING.md do not change.
* **Limine bindings:** our own (`kernel/src/limine.rs`), because the
  `limine` crate needs nightly. Note that the crate's `File` struct is
  also missing the spec's `unused` field, which shifts the GPT GUIDs by 4
  bytes. Base revision 3: the HHDM covers RAM and the framebuffer only,
  and the kernel maps MMIO and firmware tables itself.

### Kernel

* **CPU:** our own GDT and TSS (IST stacks for #DF, NMI and #MC) and an
  IDT with naked-asm exception stubs that log and reset. SMEP and SMAP
  are on when the CPU has them. The 1 MiB kernel stack lives outside the
  HHDM with an unmapped guard page below it.
* **Memory:** `linked_list_allocator` over the largest usable region
  (64–512 MiB). DMA buffers come from the heap, which is physically
  contiguous: physical = virtual − HHDM. MMIO is mapped uncached and NX
  into Limine's page tables. W^X for the kernel image comes from the
  per-PHDR permissions in `linker.ld`.
* **Interrupts:** the PICs are remapped and masked. The LAPIC (xAPIC or
  x2APIC) runs a 1 kHz timer, calibrated against the TSC, whose only job
  is to wake `hlt`. All device work happens in the main loop with
  interrupts off.
* **Clock:** TSC calibrated on PIT channel 2, falling back to CPUID 0x16.
  The CMOS RTC is decoded by `pos_drivers::rtc`.
* **xHCI** (`xhci.rs`):
  * BIOS handoff, reset, scratchpads, one command ring and one event
    ring; polled, no MSI.
  * Enable Slot, Address Device, Evaluate Context and Configure
    Endpoint, including route strings, TT and hub fields.
  * Control, bulk and interrupt transfers.
  * Endpoint recovery: Reset/Stop Endpoint, then Set TR Dequeue.
* **USB devices** (`usb.rs`):
  * Root-port and hub-port enumeration, with hot-plug and removal of
    whole subtrees.
  * HID boot keyboard: US layout, typematic repeat, keyboard-wedge
    scanners.
  * HID absolute pointer: report-descriptor parser covering tablets and
    single-touch digitizers.
  * Mass storage over BOT/SCSI:
    * Reset recovery, plus one retry per command.
    * `flush` = SYNCHRONIZE CACHE. A device that rejects it with ILLEGAL
      REQUEST is treated as write-through.
  * Printer class: bulk OUT and GET_PORT_STATUS; COM2 serial fallback.
* **Disks:**
  * The boot stick is the MSC device whose GPT disk GUID matches the one
    Limine reports for the kernel's disk. The store opens on its POS data
    partition.
  * The export stick is any other MSC device with a FAT16/32 volume
    (GPT, MBR or superfloppy), handled by `pos_drivers::fat`: root
    directory only, long names read and written.
* **Reset:** the FADT reset register, then the KBC, then a triple fault.

### Known limits (to settle on real hardware)

* **SuperSpeed (USB 3) hubs are not supported.** USB 2.0 hubs are,
  including the USB 2.0 half of a USB 3 hub. A USB 3 stick behind a USB 3
  hub will not be found: put the boot stick on a root port.
* **No exFAT.** Export sticks must be FAT32 (or FAT16), as for Tier A's
  firmware.
* **No ACPI power-off.** The app never needs one; Power::shutdown halts.
* **No MSI and no multi-core.** Polling at 1 kHz is plenty for HID and
  BOT throughput on a POS.
* **The TSC is assumed to keep running in C-states.** QEMU reports it as
  non-invariant; the 24-hour soak on hardware checks this.
* **`SYNCHRONIZE CACHE` is skipped on devices that reject it.** This is
  the same behaviour as EDK2's USB mass-storage driver under Tier A. The
  hardware power-cut test is what proves a stick model safe.

## Boot: Limine

Limine over the `bootloader` crate: it hands over a linear framebuffer, the
memory map, RSDP and a higher-half direct map via a stable protocol, it boots
from the same FAT ESP, and it can be Secure-Boot-signed together with the
kernel (Limine enforces a config hash/kernel checksum). `bootloader` ties the
build to its own image tooling and is less flexible about the ESP layout we
already produce.

## Original plan

The pre-build plan (async executor, IOAPIC, HPET calibration, `fatfs`
for export) was superseded by the "As built" section above: the kernel
polls in one main loop, calibrates on the PIT, and uses its own FAT
driver. The step order above is the order the plan set out.
