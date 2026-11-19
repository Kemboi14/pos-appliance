# Design notes

## Requirements challenged or reinterpreted

| Spec item | Problem | What was built instead |
|---|---|---|
| "verifies its own data-partition superblock **signature**" | A signing key embedded in the binary is identical on every till and extractable from the (public) image; it proves nothing. | Each machine generates a random 32-byte key on first boot and keeps it in a non-volatile, **boot-services-only** UEFI variable (`PosStoreKey`). Superblocks carry a keyed-BLAKE3 MAC under it. A stick moved to another till (or a tampered superblock) is `Foreign`: the app refuses to write until a manager PIN (verified against the store's own user table) authorises adoption, which is journaled (`StoreAdopted`). With Secure Boot + firmware password only our signed image can read that variable. |
| Tamper evidence from the hash chain alone | Anyone holding the stick can rewrite the whole chain consistently. | The chain is anchored *outside* the device: every receipt prints the first 8 bytes of its record hash (and a QR with receipt no/total/time/hash), every Z report prints the head hash, and exports carry every record hash. Rewriting history then contradicts paper and exported copies. |
| Argon2id PINs | A 4–8 digit PIN has ≤10⁸ values; offline brute force of a stolen store is cheap whatever the KDF. | Argon2id is used (pinned m=19 MiB, t=2, p=1), but the real controls are journaled online lockout (survives reboot), manager-only unlock, time-limited manager lockout (the last manager can never be locked out permanently), and physical custody. Documented, not oversold. |
| "card-offline-record" | Storing card numbers would put the appliance in PCI scope. | Card tenders accept only `last4/approval` (validated: exactly 4 digits); full PANs are rejected. |
| "FlushBlocks before confirm" | edk2's USB mass-storage `FlushBlocks` does not issue SCSI SYNCHRONIZE CACHE; a stick may acknowledge writes still in its controller RAM. | The software ordering is enforced and tested, but true durability depends on the stick. Use industrial pSLC sticks with power-loss protection and/or a small UPS (see HARDENING.md). |
| Workspace layout | Tier A and Tier B would otherwise duplicate all screen/flow logic. | Added `pos-app` (no_std, generic over pos-hal). Both tiers are then thin HAL implementations. |
| Golden tests on QEMU printer output | QEMU has no USB printer device, and the receipt contains the wall-clock time. | Byte-for-byte golden files are produced on the host (`pos-escpos`, `pos-app`) with a fixed clock. In QEMU the printer is the serial fallback (COM2 → `out/printer.bin`) and the e2e test asserts receipt content and the drawer-kick sequence. USB Printer class code is exercised only on real hardware (checklist item). |
| Fixed GPT GUIDs vs. reproducibility | Random disk/partition GUIDs make every image different. | Image GUIDs are fixed (reproducible); store *identity* is a random `store_id` assigned when the device binds a factory-fresh store. |

## M0 — workspace, toolchain, image, QEMU

* Rust 1.100.0 stable pinned in `rust-toolchain.toml`; no nightly features. Targets: `x86_64-unknown-uefi` (Tier A), `x86_64-unknown-none` (reserved for Tier B).
* Nix flake pins QEMU 11.1, OVMF edk2-202608 (plain and Secure Boot/SMM builds), sbsigntools, efitools, virt-firmware. `flake.lock` + `Cargo.lock` are committed; all cargo calls use `--locked`.
* Image (`xtask/src/image.rs`): pure Rust (`gpt`, `fatfs`), no loop mounts. 256 MiB: protective MBR, GPT, 64 MiB FAT32 ESP with `\EFI\BOOT\BOOTX64.EFI`, rest = POS data partition (type GUID `138f131d-afdf-4718-a368-f3126495b979`) with two blank superblocks. FAT timestamps come from `SOURCE_DATE_EPOCH`.
* Reproducibility: `--remap-path-prefix` for the cargo registry and workspace, `/Brepro` for lld-link, fixed GUIDs/volume ids, fixed FAT time. `cargo xtask repro` wipes the target dir between two release builds and compares EFI and image hashes (verified identical).
* Release profile: `panic=abort`, fat LTO, `codegen-units=1`, symbols stripped. Dependencies are optimised even in dev builds (Argon2 is unusable at opt-level 0).

## M1 — domain (`pos-core`)

* **Event sourcing.** Every durable change is a `state::Event`. The live path validates a command, builds the event, commits it, then applies it; recovery replays the same events through the same `PosState::apply`. `apply` re-validates every invariant (receipt sequence, totals re-derivation, refund limits, permissions) and is transactional (clone-and-swap), so invalid events can never be half-applied.
* **Money** is `i64` minor units with checked ops; intermediate products use `i128`. `Rate` is parts-per-million (8.875 % = 88 750). `Qty` is thousandths (grams for weighables). `#![forbid(unsafe_code)]`, `clippy::float_arithmetic = deny`; no float types appear anywhere in the crate.
* **Rounding** (`HalfUp`, `HalfEven`, `Down`) is configured per jurisdiction and used for every division. Basket discounts and per-invoice tax are distributed to lines with the largest-remainder method so line totals always sum exactly to the invoice total (proptest-verified across random carts and configs).
* **Tax**: inclusive (`a·r/(1+r)`) or exclusive (`a·r`), per-line or per-invoice; worked examples in `pos-core/tests/pricing.rs` (16 % VAT, 8.875 % NY sales tax, half-up vs banker's on 0.5-cent cases).
* **Tenders**: split tender; cash rounding (e.g. 0.05) only when cash settles the balance; non-cash cannot over-tender; change only from cash.
* **Refunds** reference a receipt inside the refund window; partial refunds are pro-rated and the last refund of a line takes the exact remainder, so Σ refunds == line total exactly (proptest).
* **Shifts/Z**: Z numbers and receipt numbers are gap-free and can only advance by one (enforced in `apply`); the grand total is never reset.
* **FiscalPolicy** trait (`fiscal.rs`): validate sale/refund/config, add receipt header/trailer and report lines. `GenericPolicy` ships; jurisdictions implement the trait.
* Tests: 34 (unit, worked examples, state-machine flows, 5 proptest properties × 2000 cases).

## M2 — persistence (`pos-store`)

On-disk layout (LBAs relative to the partition):

```
0  superblock A (even generations)     1  superblock B (odd generations)
2  snapshot slot 0 | snapshot slot 1   (each 1/8 of the partition, 64 KiB..32 MiB)
log_start .. end: record ring
record = 64-byte header {magic "PREC", ver, kind, seq, len, epoch, hash[32], crc32c} + payload, padded to blocks
hash   = BLAKE3(prev_hash ‖ seq ‖ kind ‖ epoch ‖ payload); genesis prev = BLAKE3("POS-GENESIS" ‖ store_id)
```

* **Commit** = one sequential write of all record blocks (batches merged) + one flush; nothing is acknowledged before the flush returns.
* **Snapshot** = write to the slot *not* referenced by the newest superblock, flush, then write superblock generation+1 into the copy *not* holding the newest generation, flush. Any torn write leaves the previous generation and its snapshot intact.
* **Recovery** = newest superblock whose snapshot verifies (BLAKE3) → scan the log from the snapshot position accepting records while magic, sequence, CRC32C and chain all verify. Blank space or an older lap's record is a clean end; anything else is reported as a torn tail and discarded.
* **Epoch**: the superblock epoch is bumped on every trusted open and mixed into every record hash. Without it, an un-acknowledged record left behind by a torn batch could chain onto an identical retried record after a reboot ("ghost" record). After any write error the store is poisoned until reopened.
* **Ring + retention**: a `WRAP` record marks the jump to `log_start`. Records needed by either superblock's snapshot, or not yet exported (the *retain* watermark, advanced only after a successful export), are never overwritten — the store returns `Full` and the UI demands an export. Audit data is never silently dropped.
* **Fault injection** (`pos-store/tests/fault_injection.rs`): a device model with a volatile write cache; power is cut at *every* block-write index of 48 random workloads (14 570 runs). At the cut the in-flight block is torn (old/new/mixed/garbage) and every unflushed block independently survives or is lost. Each run checks: all acknowledged records present in order, nothing else except a prefix of the in-flight batch, chain and snapshot verify, and the store keeps working (append, reopen, verify). The workloads exercise 48 ring wraps, 31 log-full events and 1 441 snapshots. Additional tests cover cuts during format/bind, foreign keys, history tampering and full-log refusal.

**Write amplification.** A typical sale is one ~0.6–1 KiB record (2 × 512-byte blocks) + flush. Cheap TLC sticks program 16 KiB pages and may rewrite more on every flushed small write, so expect ~16–30× at the flash level; snapshots (state size, typically 0.1–1 MiB, every 256 records) add ~1–4 KiB per record. At 500 sales/day that is tens of MB/day physical — wear is not the main risk; **power-loss behaviour of consumer controllers is**. Use industrial pSLC sticks with power-loss protection (e.g. Swissbit, Innodisk, ATP industrial lines) and keep `snapshot_every` at 256 or higher.

## M3 — UI and input

* `pos-ui`: RGB888 back buffer with dirty-rectangle presentation; X11 misc-fixed bitmap fonts (public domain, via embedded-graphics) — ISO 8859-1, `€`/`Š`/`Œ`… from ISO 8859-15, hand-drawn ₹ ₦ ₱ ₽ ₺ ₩. Layout derives from the actual resolution (baseline 1024×768; integer scale above 2048×1536).
* Keyboard-wedge scanners are told apart from typing by inter-key gaps (default ≤30 ms, ≥6 chars, Enter-terminated, configurable). Characters go to the entry line immediately (no typing latency); on Enter a burst is reclassified as a scan and removed from the line. 30 ms rather than ~5 ms because UEFI's USB keyboard driver delivers at its polling interval.
* Tier A input: SimpleTextInputEx on the ConIn handle (shared open, so the console splitter keeps working) with SimpleTextInput fallback; AbsolutePointer preferred, SimplePointer fallback, keyboard-only otherwise. Every action has a key; on-screen buttons and keypads map to the same keys.
* Event loop: `WaitForEvent` over keyboard, pointer and a 1 s periodic timer. No polling loops.

## M4 — ESC/POS

* `pos-escpos`: init, code page (`ESC t`: PC437, PC858 with €, WPC1252), alignment, bold, size, invert, feed, partial/full cut, drawer kick (`ESC p 0 25 250` = 50 ms on pin 2), EAN-13, Code 128 (set B, `{` escaped), QR (`GS ( k` model 2). Text is sanitised: control characters can never inject commands.
* Tier A transport: USB Printer class (interface 0x07) via `EFI_USB_IO_PROTOCOL` bulk OUT, status via class request GET_PORT_STATUS; fallback to `EFI_SERIAL_IO_PROTOCOL` (9600 8N1; port chosen by the `PosPrinterSerial` UEFI variable, else COM2 when several ports exist). The `uefi` crate already wraps UsbIo, so no custom protocol definition was needed.
* Bug found and fixed during QEMU bring-up: probing USB devices with *exclusive* opens disconnected the mass-storage driver from the boot stick. Probing now uses shared opens and only the printer interface is opened exclusively.
* Golden files: `pos-escpos/tests/golden/sample_receipt.bin`, `pos-app/tests/golden/receipt_sale_1.bin` (regenerate with `UPDATE_GOLDEN=1`).

## M5 — full flow, export/import, e2e

* Commit-before-confirm: `App::commit` dry-runs events on a clone of the state, commits them as one batch, then applies; only then are the receipt printed, the drawer kicked and the change shown. A failed commit leaves the payment screen untouched; a storage fault resets the machine.
* Manager overrides (line void, sale void, discounts, price override, refunds, no-sale, reports, imports, user admin) are journaled `Override` events in the same batch as the action. Failed override PINs count toward lockout.
* Export (manager): journal CSV (`J<lastseq>.CSV`, 8 MiB chunks, every row with its chain hash), one `Z<n>.CSV` per Z report, `CATALOG.CSV`; then `Exported` is journaled and the retain watermark advances. CSV fields are formula-injection-safe.
* Import: `CATALOG.CSV` (fully validated, one atomic `CatalogReplace` record) and `SETTINGS.TXT` (key=value; tax classes, rounding, cash increment, scanner timing, …).
* Host e2e (`pos-app/tests/host_e2e.rs`): full day (setup, cashier, scans incl. UPC-A normalisation and weighed goods, split tender, override, refund, Z with counted cash, export, reboot equality), foreign-machine adoption, and a power cut at every write of a sale (never printed or confirmed without a durable commit).
* QEMU e2e (`cargo xtask e2e`): OVMF + qemu-xhci + usb-kbd + usb-tablet + two usb-storage sticks, `-nic none`; QMP keystrokes perform setup, sign-in, float, catalog import from the second stick, a cash sale, then a second sale cut by QMP `quit`; reboot with the same NVRAM, sign in, export; the host then checks the exported journal (gap-free sequence, receipts start at 1 and are gap-free, hashes present) and the captured printer bytes (receipt #1, drawer kick).

## M6 — Secure Boot and release hardening

* `cargo xtask keys`: RSA-2048/SHA-256 PK → KEK → db hierarchy (PEM, DER `.cer` for firmware UIs, signed `.auth` for KeyTool-style enrollment). `keys/` is git-ignored; keep `*.key` offline.
* `cargo xtask sign`: `sbsign` with db, `sbverify`, then `out/pos-signed.img`.
* `cargo xtask sb-test`: enrolls *only* our keys (no Microsoft CA) into a Secure Boot OVMF varstore with `virt-fw-vars --sb`; verified that the signed image boots to the UI and the unsigned image is refused (`Access Denied`).
* Release lock-down: `--release --debug-console` is rejected by xtask; release builds contain no logger and no console output; panics reset the machine (the store is crash-safe).
* Reproducibility covers the unsigned binary; the signed binary = unsigned + Authenticode signature (`sbattach --remove` recovers the reproducible bytes for verification).
