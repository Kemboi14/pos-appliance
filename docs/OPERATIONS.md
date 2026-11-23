# Operating the till

## Passwords

There are **no default passwords or PINs**. On first boot the till asks
for the manager's name and a 4–8 digit PIN; that manager is user number 1.
Managers add cashiers (and further managers) with their own PINs from the
manager menu. PINs are stored only as Argon2id hashes; a forgotten PIN
cannot be read back — another manager resets it by unlocking or re-adding
the user. Five wrong PINs lock a user (managers: a timed lockout).

## First boot

1. "First-time setup": manager name, PIN (4–8 digits) twice. The store is bound to this machine.
2. The sign-in screen shows the clock and a tile per user: tap your name (or type your user number), then your PIN.
3. The home screen shows the two apps, **Till** and **Inventory**, plus the manager menu and sign out. Keys: `1`/Enter Till, `2` Inventory, F10 menu, F12 sign out.
4. Manager menu (F10) → 9 "Import settings" (`SETTINGS.TXT`) and 8 "Import catalog" (`CATALOG.CSV`) from a FAT USB stick, then "Import pictures".
5. F10 → "Add cashier".
6. Open **Till**: enter the opening float. The **Home** button (top left, or the Home key) goes back to the home screen when no sale is open.

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

## Inventory

Open **Inventory** from the home screen. The top cards show products, low and out-of-stock counts and the stock's retail value.

* Tabs: **All**, **Low** (at or below the reorder level, or out), **Out**, **Not tracked** (F1–F4). **Search** (F5) or just type; a scanned barcode opens that product. **Export** (F6) writes `STOCK.CSV`.
* Tap a product (or Enter) for its page: picture, stock on hand, reorder level, last cost, last delivery and count.
  * **Receive** (F1): quantity delivered, then unit cost (optional).
  * **Count** (F2): the quantity on the shelf replaces the book stock; the difference is journaled as the variance.
  * **Write off** (F3): quantity and reason (damaged, expired, theft/loss, returned to supplier, other).
  * **Reorder level** (F4): flag the product as low at this quantity (0 = never).
* A product is *tracked* from its first delivery or count. Sales take tracked stock off automatically and refunds put it back; untracked products (services, bags) are sold without stock. Stock may go negative — the till never refuses a sale over it; count the product.
* Stock changes need the **stock** permission (managers). A cashier gets the manager-override prompt.
* Every stock change is a journal event like a sale (in the hash chain, exported in the journal CSV).

`STOCK.CSV`: `sku,name,status,on_hand,reorder_at,last_cost,retail_value`.

## Pictures

Manager menu → "Import pictures from USB" reads these files from the root of a FAT stick (JPEG or PNG; up to 12 megapixels each):

| File | Used for |
|---|---|
| `IMG_<SKU>.JPG` (or `.PNG`) | product picture: quick keys, inventory list and page |
| `USER_<number>.JPG` | user photo on the sign-in and home screens |
| `LOGO.PNG` | store logo in the top bar (transparency kept) |
| `WALLPAPER.JPG` | background of the sign-in and home screens (cropped to 16:9) |

Characters a file name cannot hold (`/ \ : * ? " < > |`) are written `_` in `IMG_<SKU>`. Pictures are shrunk on import (products and users 128 px, logo 512×256, wallpaper 640×360), stored losslessly in the journal and survive reboots. Importing again replaces them. Picture storage is limited to about half a snapshot slot; the import says which files were skipped and why.

## Store network (several tills)

Tills are connected by **network cable** to the store's switch (see `docs/NETWORK.md` for why not Wi-Fi). One till is the **head**; the others are **lanes**.

1. On the head: manager menu → "Network setup" → `2` Head → till number (e.g. 1) → IP address (empty = DHCP, or `192.168.1.10/24 gw 192.168.1.1`). The screen shows the **store network code**; with a USB stick inserted it is also written to `STORENET.KEY`. Keep it safe: it is the store's network password.
2. On each lane: "Network setup" → `3` Lane → its own till number → type the code (or leave empty to read `STORENET.KEY` from the stick) → IP address → head's address (empty = find automatically).
3. The lane then receives the head's catalog, prices, users (the head's PINs apply on every till), settings and pictures, and the store-wide stock. The lane's own first manager is replaced, so sign in with a head user.

Every sale, refund, delivery, count and write-off on any till updates the stock on all tills within seconds. A till keeps selling when the cable or the head is down and catches up when it is back. The status bar shows `Till n · address · online`; the head's home screen shows tills online and the store's sales today; "Network status" in the manager menu shows details. On lanes, catalog/user/settings/picture changes are refused ("Managed by the head till") — make them on the head.

Every till in one store must have a different number. If a till is lost or retired, set up the head again with a new key (choose Standalone, then Head) and re-join the lanes.

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
