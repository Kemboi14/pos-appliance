# POS appliance (offline, UEFI, USB-booted)

A single-purpose point-of-sale system that boots from a USB stick on x86_64
UEFI PCs, runs nothing but the POS, and keeps all data on the same stick in a
power-loss-safe, hash-chained journal. No network stack, no OS, no shell.

```
pos-core/       domain: catalog, cart, pricing/tax/discounts, tenders, refunds,
                shifts, X/Z, users/PIN policy, event-sourced state (no_std, no unsafe, no floats)
pos-hal/        hardware traits: Display, Input, Clock, BlockDevice, ExternalMedia,
                Printer, Rng, Power
pos-store/      log-structured store on a raw partition (superblocks, snapshots,
                CRC32C + BLAKE3 hash chain, ring with export watermark, machine binding)
pos-ui/         software-rendered immediate-mode UI, Latin-1 + currency fonts,
                scanner (keyboard-wedge) detection
pos-escpos/     ESC/POS encoder (code pages, cut, drawer kick, EAN-13, Code 128, QR)
pos-app/        the application (screens, commit-before-confirm, receipts, export/import),
                generic over pos-hal — shared by Tier A and Tier B
platform-uefi/  Tier A: pos-hal on UEFI boot services (never exits boot services)
pos-drivers/    host-tested driver logic for Tier B: USB descriptors, HID, SCSI/BOT,
                GPT/MBR, FAT16/32, RTC decoding
kernel/         Tier B: own kernel (xHCI, HID, mass storage, printer, hubs) booted by Limine
stage0/         Tier B: signed UEFI stage 0 (store-key handover, starts Limine)
xtask/          image builder, QEMU runner, e2e, Secure Boot signing, CI driver
```

## Quick start

Requires Nix with flakes (pins Rust 1.100.0, QEMU, OVMF, sbsigntools,
efitools, virt-firmware). Everything below runs without root.

```sh
nix develop                      # enter the pinned toolchain shell
cargo xtask test                 # host tests: core, store (incl. 14k power-cut runs), ui, escpos, app
cargo xtask image --release      # out/pos.img + SHA-256 of BOOTX64.EFI and the image
cargo xtask qemu --release       # boot it in a QEMU window (printer bytes -> out/printer.bin)
cargo xtask e2e                  # scripted QEMU sale + power cut + reboot + export check
cargo xtask keys && cargo xtask sign   # Secure Boot keys, signed out/pos-signed.img
cargo xtask sb-test              # OVMF with SB enforced: signed boots, unsigned refused
cargo xtask repro                # two clean release builds, bit-identical or fail
cargo xtask smoke --kernel       # Tier B: stage 0 -> Limine -> pos-kernel to the POS UI
cargo xtask e2e --kernel --hub   # Tier B e2e, all devices behind a USB hub, stick hot-plugged
cargo xtask tamper-test          # Tier B: modified kernel or limine.conf is refused
cargo xtask key-test             # Tier B: NVRAM store key created, then handed over
cargo xtask sign --kernel && cargo xtask sb-test --kernel   # Tier B Secure Boot chain
cargo xtask ci                   # everything CI runs
```

Write to a stick (destroys everything on the target; **check the device
name twice** with `lsblk` — `dd` to the wrong disk erases it):

```sh
lsblk -o NAME,SIZE,MODEL,TRAN      # identify the stick (TRAN=usb)
sudo dd if=out/pos-signed.img of=/dev/sdX bs=4M conv=fsync status=progress
```

Docs: [docs/DESIGN.md](docs/DESIGN.md) (decisions per milestone, on-disk
format, threat model), [docs/HARDENING.md](docs/HARDENING.md) (firmware
settings, Secure Boot enrollment, hardware validation checklist, risks),
[docs/OPERATIONS.md](docs/OPERATIONS.md) (keys, CSV/settings formats),
[docs/TIER_B.md](docs/TIER_B.md) (own-kernel plan and status).
