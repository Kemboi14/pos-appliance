# Store network (M9)

Tills in one supermarket talk over the store's **wired Ethernet** LAN
(switches; Wi-Fi access points may share the LAN for other devices, but the
tills themselves are cabled — see "Why not Wi-Fi"). Every till keeps
working alone when the network or the head till is down and catches up
when it returns: the network is never on the path of a sale.

## Roles

* **Standalone** (default): no networking, exactly as before.
* **Head**: one till (or a back-office box running the same image) is the
  store's source of truth for the catalog, prices, users, settings and
  pictures, and the hub that collects every till's stock movements and
  sales and hands them on.
* **Lane**: every other till. Its catalog, users, settings and pictures come
  from the head; managing them locally is disabled.

Each till has a number (1–99), unique in the store.

## What is shared

| Data | Direction | How |
|---|---|---|
| Catalog, users, settings, pictures | head → lanes | state: the head sends a section again only when its version changes |
| Stock movements (sales, refunds, deliveries, counts, write-offs) | every till → head → every other till | feed of deltas, each tagged (origin till, origin sequence) |
| Sales summaries (receipt, total, cashier, time) | lanes → head | same feed; the head shows store-wide figures |

Stock movements are **deltas** (a count travels as its variance), so they
commute: every till converges on the same store-wide stock, whatever order
the movements arrive in. Each movement is applied once — receivers keep, per
origin, the highest sequence applied, and the journal records every received
movement, so the stock after a reboot is the same as before it.

If a lane has been away so long that the head no longer holds the movements
it missed, the head sends absolute stock levels instead (a *resync*); the
lane adds its own movements the head has not yet acknowledged.

## Security

* All tills of a store share a 256-bit **store network key**, created on
  the head and carried to lanes on a USB stick (`STORENET.KEY`) or typed
  as a 52-character code. Without it a device on the LAN can neither read
  nor inject anything.
* Frames are XChaCha20-Poly1305 with a fresh random 192-bit nonce; the
  frame header (version, sender till, length) is authenticated data. Keys
  are derived from the store key with BLAKE3 `derive_key` (separate keys per
  purpose).
* Messages are idempotent (state with versions; movements with
  per-origin sequence numbers), so a replayed frame changes nothing.
* No forward secrecy in v1: someone who later steals the store key can
  decrypt recorded traffic. Rotate the key (new `STORENET.KEY`) when a till
  is retired or lost.
* The till only connects out to the head and the head only accepts the
  store's port; there is no other listening service, no remote login and no
  remote management.

## Transport

TCP port 47100 from each lane to the head (address set on the lane, or
found by the head's UDP announcement on port 47101). IPv4 by DHCP or static.
Frames: `len:u32 | version:u8 | till:u8 | nonce:24 | ciphertext+tag`,
payload a postcard-encoded `Message`.

Tier A uses the firmware's Simple Network Protocol; Tier B has its own
drivers (virtio-net for QEMU, Intel e1000e for common POS mainboards).

## Why not Wi-Fi

A Wi-Fi client needs a chipset driver, its firmware blob and a WPA2/WPA3
supplicant — tens of thousands of lines per chip family, all of it
attack surface reachable from the air. Cabled tills are also what payment
and fiscal rules usually expect. A till that must be wireless can use a
Wi-Fi-to-Ethernet bridge (it then appears to the till as a cable).
