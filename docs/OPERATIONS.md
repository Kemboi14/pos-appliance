# Operating the till

## First boot

1. "First-time setup": manager name, PIN (4–8 digits) twice. The store is bound to this machine.
2. Sign in (tap your name, or type your user number) and PIN; enter the opening float.
3. Manager menu (F10) → 9 "Import settings" (`SETTINGS.TXT`) and 8 "Import catalog" (`CATALOG.CSV`) from a FAT USB stick.
4. F10 → "Add cashier".

## Sale screen keys

| Key | Action |
|---|---|
| digits/letters + Enter | barcode, SKU or PLU lookup (scanner input is detected automatically) |
| `n*` then code | quantity `n` (e.g. `3*`, or `1.250*` for 1.25 kg of a weighed item) |
| Enter on empty line / F1 | pay |
| ↑ ↓ | select line |
| F2 | search by name/SKU |
| F3 | set quantity of the selected line (type it first) |
| F4 | void selected line (manager) |
| F5 / F6 | line / basket discount in percent (type it first; manager) |
| F7 | price override (type the new price first; manager) |
| F8 | void sale (manager) |
| F9 | refund (receipt number → pick lines → reason → payout; manager) |
| F10 | manager menu |
| F11 | no sale / open drawer (manager) |
| F12 | sign out |
| Esc | clear the entry line |

Payment: Enter/F1 cash (typed amount or exact), F2 card record (`last4/approval`), F3 voucher. Change and cash rounding are shown after the sale is committed. On the change screen, any key (or a scan) starts the next sale.

PageUp / PageDown move between quick-key pages.

## Touch screen

Everything can be done by touch alone; every on-screen button does the same as its key.

* **Sale screen:** quick keys on the right, in pages of 12 (◀ ▶ to change page). **123** swaps the quick keys for a keypad: type a quantity and tap **× Qty** before an item, or type an amount for F3/F5/F6/F7. Tap a line to select it.
* **Payment:** the keypad enters the cash tendered; the green **Cash** key takes it (or the exact amount if nothing is typed). Card, Voucher and Void sale are below.
* **Change screen:** tap anywhere for the next sale.
* **Names, codes, reasons and search** get an on-screen keyboard. Its ⇧ key cycles: off → next letter only → CAPS → off.
* **Sign-in and manager override** show a tile for each user (up to 12; with more, type the user number).
* Buttons light up while pressed and act when the finger lifts on the same button, so sliding off cancels. A second tap within 150 ms on the same spot is treated as panel bounce and ignored.

## CATALOG.CSV

```
sku,barcode,plu,name,price,tax_code,weighable,quick_key
MILK,4006381333931,,Milk 1L,1.20,A,0,0
APPLES,,04711,"Apples, red",3.99,Z,1,2
```

* `barcode`: EAN-8/UPC-A/EAN-13 (check digit verified; UPC-A is stored as EAN-13) or an internal code (≤20 alphanumerics).
* `plu`: 5 digits, used by price/weight-embedded labels (`2x PLU VALUE C`).
* `tax_code`: the one-letter code of a tax class. `weighable`: 0/1 (price per kg). `quick_key`: position on the on-screen grid, 0–255; 0–11 is page 1, 12–23 page 2, and so on, left to right and top to bottom in a 4×3 grid. Gaps are allowed (use them to group items per page).
* The file is validated completely; any error aborts the import with the line number and nothing changes.

## SETTINGS.TXT

See the module docs in `pos-app/src/settings.rs` for every key. Example:

```
store_name = CORNER SHOP
address = 12 Market Road
tax_id = P051234567X
currency = KES
tax_mode = inclusive
tax_calc = per_line
rounding = half_up
tax_class = 0,A,Standard,16
tax_class = 1,Z,Zero,0
cash_increment = 1
printer_columns = 42
footer = Thank you!
```

## Export

The export stick must be formatted **FAT32** (or FAT16). exFAT, the
default for sticks over 32 GB on Windows, is not readable by either tier.
Format it as FAT32 first.

F10 → 7 writes `J<seq>.CSV` (journal: seq, chain hash, time, kind, receipt, user, amount, detail), `Z<n>.CSV` per Z report, and `CATALOG.CSV`. Exported records may then be recycled by the ring. Export at least after every Z; the status bar shows `EXPORT NEEDED` from 80 % journal usage, and at 100 % sales are refused until an export.

## After a power cut

The till recovers automatically. A sale counts as made only if its receipt printed (or the "Receipt #n saved" screen appeared). If power failed while "Pay" was pressed, check F10 → "Reprint last receipt" to see whether it was committed.
