# Hardening, Secure Boot enrollment, hardware validation

## Firmware settings (per till)

1. **Secure Boot: custom mode.** Clear factory keys, enroll our PK/KEK/db (below). Do not enroll the Microsoft CAs unless the hardware needs signed option ROMs (discrete GPU/NIC); if it does, add only "Microsoft UEFI CA" to db and accept that anything Microsoft-signed (e.g. shim) can then boot.
2. **Firmware administrator password** set and recorded in the shop's safe. Without it, anyone can disable Secure Boot or boot another OS and read the store-binding NVRAM variable.
3. **Boot order**: USB stick only; disable "boot from other devices", one-time boot menus (F12/F11) if the firmware allows, and PXE.
4. **Network**: disable the UEFI network stack (often "Network Stack", "IPv4/IPv6 PXE Support"), Wake-on-LAN, and the onboard NIC if possible. The app never opens SNP/MNP/IP4/TCP; disabling the stack removes the firmware's own attack surface too. Wi-Fi/Bluetooth off.
5. **Serial console redirection: off.** Otherwise firmware text is sent to a serial receipt printer at power-on (observed in QEMU: OVMF writes its console to every serial port).
6. **CSM/Legacy boot: off.**
7. **RTC set to UTC.** The journal records UTC; local time display is a future setting.
8. **USB power**: disable USB selective suspend / "USB power off in S5" features that can drop the stick during operation.
9. **Restore on AC power loss: Power On**, so a till comes back after an outage without staff touching it.

## Secure Boot key generation and enrollment

```sh
nix develop
cargo xtask keys            # keys/PK.{key,crt,cer,esl,auth}, KEK.*, db.*, GUID.txt
cargo xtask sign            # out/BOOTX64.signed.EFI, out/pos-signed.img
cargo xtask sb-test         # proves the chain in OVMF (signed boots, unsigned refused)
```

Enroll on a till (pick whichever the firmware supports):

* **Firmware UI** (most business PCs): Security → Secure Boot → Custom/Expert key management → "Delete all keys" → enroll `db.cer`, `KEK.cer`, then `PK.cer` last (enrolling PK leaves Setup Mode). Copy the `.cer` files to a FAT stick first.
* **Signed updates** (`.auth`, e.g. with KeyTool.efi or `efi-updatevar` from a Linux live system in Setup Mode): write `db.auth`, `KEK.auth`, then `PK.auth`.

Keep `*.key` offline (encrypted backup). Losing db.key means re-signing requires new keys and re-enrollment on every till; leaking it means anyone can sign images your tills will boot.

### Tier B images

Tier B uses the same keys. `cargo xtask sign --kernel` signs stage 0 and
the config-enrolled `LIMINE.EFI` with `db.key` and writes
`out/pos-kernel-signed.img`. Nothing else in the Tier B chain is a PE
image: Limine checks `limine.conf` against the hash enrolled in its signed
binary, and `limine.conf` pins the kernel's BLAKE2B hash. Changing any
file on the ESP therefore breaks the chain (see `cargo xtask tamper-test`
and `sb-test --kernel`).

## Writing the stick

```sh
lsblk -o NAME,SIZE,MODEL,TRAN,MOUNTPOINTS   # find the stick: TRAN=usb, expected size
sudo umount /dev/sdX?* 2>/dev/null
sudo dd if=out/pos-signed.img of=/dev/sdX bs=4M conv=fsync status=progress
```

`dd` writes to whatever device you name, without confirmation. Writing to the wrong `/dev/sdX` destroys that disk. Verify the model and size first. The data partition occupies the rest of a 256 MiB image; larger sticks simply leave space unused (resize support is future work).

## Real-hardware validation checklist

Record results per hardware model; a model is approved only when all pass.

- [ ] Boots signed image with Secure Boot enforced; unsigned image refused.
- [ ] Display: picks a mode ≤1920×1080; text legible; no tearing on full redraws.
- [ ] Keyboard: all F1–F12, arrows, Esc, Backspace, numeric keypad, `*`.
- [ ] Barcode scanner (keyboard-wedge, US layout, Enter suffix): EAN-13, UPC-A, EAN-8, price-embedded 2x codes classified as scans; human typing never misclassified. Tune `scanner_max_gap_ms` if needed.
- [ ] Touch screen (if any): AbsolutePointer present, taps hit buttons accurately at the panel's native resolution.
- [ ] USB receipt printer: detected as class 0x07; receipt prints; € prints in CP858; QR and EAN-13 scan back correctly; cut works; drawer opens on cash sales and No-sale only.
- [ ] Serial printer (if used): `PosPrinterSerial` selects the right port; baud matches the printer (9600 default).
- [ ] Export stick inserted **after** boot is detected; files readable on a PC; CSV opens in a spreadsheet without formula execution.
- [ ] Catalog import of 10 000 items completes; invalid file rejected with a line number and nothing changed.
- [ ] **Power-cut test**: pull AC (not a clean shutdown) 50× at random moments, including during "processing payment". Every boot recovers; no confirmed sale is missing; receipt numbers stay gap-free (check via export). Repeat with the production stick model.
- [ ] Moving the stick to a second till shows "STORE NOT BOUND"; adoption needs a manager PIN and is journaled.
- [ ] Firmware NVRAM variable `PosStoreKey` survives reboot and firmware updates (if a BIOS update wipes NVRAM, adoption will be required afterwards — expected).
- [ ] Entropy source reported (EFI RNG or RDSEED/RDRAND); no "no hardware entropy" warning.
- [ ] 24-hour soak with a scripted keystroke generator: no slowdown, no watchdog reset.
- [ ] RTC drift within ±2 s/day; set to UTC.

Tier B additionally (run with `pos-kernel-signed.img`):

- [ ] Every xHCI controller on the board starts; devices on each rear/front port enumerate. Note which ports sit behind internal hubs.
- [ ] Touch panel's HID report descriptor is recognised (taps land where touched). If not, capture the descriptor for `pos_drivers::hid` tests.
- [ ] Boot stick found on every port it may be plugged into (not behind a USB 3 hub).
- [ ] Power-cut test (50×) repeated on Tier B with the production stick model; note whether the stick accepts SYNCHRONIZE CACHE (debug build logs it).
- [ ] Same stick moved Tier A → Tier B → Tier A on one till stays trusted (shared NVRAM key).
- [ ] 24-hour soak: clock does not drift against the RTC (TSC keeps running in idle).

## Known hardware/firmware risks

* **Flush semantics on USB sticks.** edk2's USB mass-storage driver completes `FlushBlocks` without SYNCHRONIZE CACHE; consumer sticks may lose acknowledged writes or corrupt their FTL on power loss. Mitigation: industrial pSLC sticks with power-loss protection; UPS; the power-cut checklist item on the exact stick model.
* **PartitionInfo protocol** is missing on some firmware (OVMF does not install it on USB partitions). Fallback: the fixed unique partition GUID in the HD() device-path node. Sticks cloned from the same image share it; the boot-disk device-path check still keeps a second POS stick from being used as the data partition.
* **Firmware USB keyboard timing** varies; scanner detection thresholds may need tuning per model.
* **UEFI variable services** may be unavailable or read-only on some firmware; the app then warns that store binding is disabled (it still runs).
* **GOP without a linear framebuffer** (BltOnly) is not supported; such firmware is rare on x86 PCs.
* **Firmware updates** may reset Secure Boot keys or NVRAM; re-enroll keys and expect a store adoption prompt.
* **USB printer quirks**: some printers expose vendor-class (0xFF) interfaces instead of Printer class; those need a VID/PID allow-list (not implemented).
