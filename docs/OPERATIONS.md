# Operating the till

## Passwords

There are **no default passwords or PINs**. On first boot the till asks
for the manager's name and a 4–8 digit PIN; that manager is user number 1.
Managers add users with their own PINs in **Settings → Users** (or menu →
"Add user"). PINs are stored only as Argon2id hashes; a forgotten PIN
cannot be read back — a manager sets a new one (Settings → Users → the
user → PIN). Anyone can change their own PIN (Settings → My PIN). Five
wrong PINs lock a user (managers: a timed lockout).

## First boot

1. "First-time setup": manager name, PIN (4–8 digits) twice. The store is bound to this machine and the manager is **signed in straight away**.
2. "Name your store" (optional; Enter keeps it for later).
3. Later sign-ins: the sign-in screen shows the clock and a tile per user. Tap your name, or type your **name** (any case, first name or its start) or your **user number**, then your PIN.
4. The home screen shows the apps your role may use (**Till**, **Inventory**, **Reports**, **Settings**), plus the menu and sign out. Keys: `1`/Enter Till, `2` Inventory, `3` Settings, `4` Reports, F10 menu, F12 sign out.
5. Menu (F10) → 9 "Import settings" (`SETTINGS.TXT`) and 8 "Import catalog" (`CATALOG.CSV`) from a FAT USB stick, then "Import pictures".
6. Settings → Users → "+ Add user": name, role, PIN twice.
7. Open **Till**: enter the opening float. The **Home** button (top left, or the Home key) goes back to the home screen when no sale is open.

## Roles and Settings

| Role | By default may |
|---|---|
| Manager | everything (fixed) |
| Supervisor | everything except users, configuration, catalog and imports: voids, refunds, discounts, price overrides, no sale, Z report, stock, unlock users, export — and authorise these for cashiers |
| Cashier | sell, open a shift, reprint, X report |
| Stock clerk | stock changes, catalog pictures, export, reprint (no Till app) |

Anything a user's role lacks gets the override prompt: someone whose role
has it enters their PIN (journaled as `OVERRIDE`).

**Settings** (home screen, or menu → "Settings"):

* **My PIN** — anyone.
* **Users** — role (tap to cycle), name, PIN reset, disable/enable, unlock. Needs *users* (managers).
* **Roles & permissions** — per role, tap a permission to allow it or require an override; "Restore the default permissions". *Users* and *configure* stay manager-only so nobody can grant themselves more. Needs *configure*.
* **Shift sales goal** — the home screen shows a progress bar (and a little celebration when it is reached; the till announces it on the sale that crosses it). 0 = off.
* **Automatic sign-out** — after N idle minutes (never during a sale). 0 = never.
* **Store name**, **Import settings**, **Store network**, **Network status**, **About this till**.

**Reports** (needs *X report*): the shift at a glance — sales, receipts,
average sale, refunds; sales by hour (tap a bar, or ←/→, for that hour's
figures); best sellers; cashiers; takings per payment type (cash net of
change). F1 prints an X report. The figures survive a reboot and reset when
a shift opens.

**Paying:** above the keypad are quick-cash buttons — *Exact*, then the next
round amounts (e.g. 2.40 due → 5, 10, 20). One tap records the cash and
shows the change.

**Menu** (F10): grouped into *Sales & receipts*, *USB stick* and *Team &
store*. Digits 1–9, 0 still pick the numbered items; items your role can't
do alone say "needs approval".

**Display & sound** (Settings; this till only, needs *configure*):
dark or light theme; accent colour; text size (normal, large, extra
large); a left-handed sale screen (keys on the left); pictures on quick
keys; a beep on every scan (a low tone when nothing matched); a key click;
a clock screen saver after N idle minutes (not during a sale; any key or
tap wakes it without doing anything else). Sound uses the PC speaker,
which many modern PCs don't have — "Test the speaker" tells you.

Role permissions, the goal and the sign-out time are one `POLICY` journal
event; on a store network the head hands them to the lanes like users.

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
* Stock changes need the **stock** permission (managers, supervisors, stock clerks by default). Others get the override prompt.
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

Tills are connected by **network cable** to the store's switch, or on Tier A by **Wi-Fi** where the PC's firmware has a Wi-Fi driver (Settings → Wi-Fi; see `docs/NETWORK.md`). Join Wi-Fi first, then set up the store network. One till is the **head**; the others are **lanes**.

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
