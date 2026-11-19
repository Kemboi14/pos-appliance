//! Power-loss-safe, log-structured persistence on a raw block device.
//!
//! # On-disk layout (LBAs relative to the partition)
//!
//! ```text
//! 0          superblock copy A  (written when generation is even)
//! 1          superblock copy B  (written when generation is odd)
//! 2..        snapshot slot 0    (snap_blocks)
//! ..         snapshot slot 1    (snap_blocks)
//! log_start  record log (ring)  .. end of partition
//! ```
//!
//! **Records** are a 64-byte header plus payload, padded to whole blocks:
//! `magic "PREC", version, kind, seq, len, hash[32], crc32c`. `crc32c`
//! covers the header (with crc = 0) and the payload. `hash` is the hash
//! chain: `BLAKE3(prev_hash ‖ seq ‖ kind ‖ payload)`; the first record's
//! `prev_hash` is `BLAKE3("POS-GENESIS" ‖ store_id)`.
//!
//! **Commit** = one sequential write of the record blocks + `flush`. Nothing
//! is acknowledged before the flush returns.
//!
//! **Snapshots** go to the slot *not* referenced by the newest superblock,
//! are flushed, and only then is a superblock with generation+1 written to
//! the copy *not* holding the newest generation, and flushed. A torn
//! snapshot or superblock write therefore always leaves the previous
//! generation and its snapshot intact.
//!
//! **Recovery**: pick the valid superblock with the highest generation, load
//! its snapshot (verified by BLAKE3), then scan the log from the snapshot
//! position, accepting records while magic, sequence number, CRC and hash
//! chain all check out. The first failure is the torn tail; appends resume
//! there.
//!
//! **Ring**: when a record does not fit before the end of the log, a `WRAP`
//! record is written and the log continues at `log_start`. Blocks still needed
//! by either superblock's snapshot or not yet exported (the *retain* point)
//! are never overwritten: the store reports [`StoreError::Full`] instead.
//!
//! **Binding**: each superblock carries a keyed BLAKE3 MAC under a per-machine
//! key supplied by the platform. A store whose MAC does not verify is
//! reported as [`Trust::Foreign`] and refuses appends until it is re-bound.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod crc32c;
pub mod mem;

use alloc::vec;
use alloc::vec::Vec;
use crc32c::crc32c;
use pos_hal::{BlockDevice, HalError};

pub const SB_MAGIC: u64 = u64::from_le_bytes(*b"POSSUPER");
pub const REC_MAGIC: u32 = u32::from_le_bytes(*b"PREC");
pub const FORMAT_VERSION: u32 = 1;
pub const REC_VERSION: u16 = 1;
pub const HEADER_LEN: usize = 64;
pub const KIND_WRAP: u16 = 0xFFFF;
/// Largest record payload (a full catalog import is one record).
pub const MAX_PAYLOAD: usize = 16 * 1024 * 1024;

const SB_FLAG_BOUND: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreError {
    Hal(HalError),
    /// Both superblocks are blank: the partition was never formatted.
    Unformatted,
    /// No valid superblock, or the snapshot it references is damaged.
    Corrupt(&'static str),
    /// Geometry is too small or not a multiple of the block size.
    Geometry,
    /// The log is full of records that are still needed (export required).
    Full,
    /// Payload or snapshot larger than allowed.
    TooLarge,
    /// The store is not bound to this machine; appends are refused.
    NotTrusted,
    /// Re-binding with a new store id is only allowed on an empty store.
    NotEmpty,
}

impl From<HalError> for StoreError {
    fn from(e: HalError) -> Self {
        StoreError::Hal(e)
    }
}

pub type SResult<T> = Result<T, StoreError>;

/// A position in the hash-chained log: the record with sequence number
/// `seq` starts at `lba`, and `prev_hash` is the chain value before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pos {
    pub seq: u64,
    pub lba: u64,
    pub prev_hash: [u8; 32],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub block_size: usize,
    pub total_blocks: u64,
    pub snap_start: u64,
    pub snap_blocks: u64,
    pub log_start: u64,
    pub log_blocks: u64,
}

impl Geometry {
    /// Default geometry: each snapshot slot gets 1/8 of the partition,
    /// clamped to [64 KiB, 32 MiB]; the rest is log.
    pub fn for_device(block_size: usize, total_blocks: u64) -> SResult<Geometry> {
        if block_size < 512 || !block_size.is_power_of_two() {
            return Err(StoreError::Geometry);
        }
        let bs = block_size as u64;
        let min = (64 * 1024) / bs;
        let max = (32 * 1024 * 1024) / bs;
        let snap_blocks = (total_blocks / 8).clamp(min, max);
        let log_start = 2 + 2 * snap_blocks;
        if total_blocks < log_start + 64 {
            return Err(StoreError::Geometry);
        }
        Ok(Geometry {
            block_size,
            total_blocks,
            snap_start: 2,
            snap_blocks,
            log_start,
            log_blocks: total_blocks - log_start,
        })
    }
    fn log_end(&self) -> u64 {
        self.log_start + self.log_blocks
    }
    fn blocks_for(&self, bytes: usize) -> u64 {
        bytes.div_ceil(self.block_size) as u64
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SnapInfo {
    valid: bool,
    slot: u8,
    len: u32,
    data_hash: [u8; 32],
    /// Log position right after the last record included in the snapshot.
    at: Pos,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Superblock {
    generation: u64,
    flags: u32,
    store_id: [u8; 16],
    geom: Geometry,
    snap: SnapInfo,
    retain: Pos,
    /// Bumped on every trusted open; mixed into every record so stale data
    /// from an interrupted earlier boot can never chain onto new records.
    epoch: u32,
    mac: [u8; 32],
}

// ---- little-endian codec helpers

struct W<'a> {
    b: &'a mut [u8],
    o: usize,
}
impl W<'_> {
    fn put(&mut self, d: &[u8]) {
        self.b[self.o..self.o + d.len()].copy_from_slice(d);
        self.o += d.len();
    }
    fn u8(&mut self, v: u8) {
        self.put(&[v]);
    }
    fn u16(&mut self, v: u16) {
        self.put(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.put(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.put(&v.to_le_bytes());
    }
    fn pos(&mut self, p: &Pos) {
        self.u64(p.seq);
        self.u64(p.lba);
        self.put(&p.prev_hash);
    }
}

struct R<'a> {
    b: &'a [u8],
    o: usize,
}
impl R<'_> {
    fn take<const N: usize>(&mut self) -> [u8; N] {
        let mut a = [0u8; N];
        a.copy_from_slice(&self.b[self.o..self.o + N]);
        self.o += N;
        a
    }
    fn u8(&mut self) -> u8 {
        self.take::<1>()[0]
    }
    fn u16(&mut self) -> u16 {
        u16::from_le_bytes(self.take())
    }
    fn u32(&mut self) -> u32 {
        u32::from_le_bytes(self.take())
    }
    fn u64(&mut self) -> u64 {
        u64::from_le_bytes(self.take())
    }
    fn pos(&mut self) -> Pos {
        Pos {
            seq: self.u64(),
            lba: self.u64(),
            prev_hash: self.take(),
        }
    }
}

/// Bytes covered by the MAC; the MAC and CRC follow.
const SB_BODY_LEN: usize = 8 + 4 + 4 + 8 + 16 + 4 + 8 * 5 + (1 + 1 + 4 + 32 + 48) + 48 + 4;
const SB_LEN: usize = SB_BODY_LEN + 32 + 4;

impl Superblock {
    fn encode_body(&self, b: &mut [u8]) {
        let mut w = W { b, o: 0 };
        w.u64(SB_MAGIC);
        w.u32(FORMAT_VERSION);
        w.u32(self.flags);
        w.u64(self.generation);
        w.put(&self.store_id);
        w.u32(self.geom.block_size as u32);
        w.u64(self.geom.total_blocks);
        w.u64(self.geom.snap_start);
        w.u64(self.geom.snap_blocks);
        w.u64(self.geom.log_start);
        w.u64(self.geom.log_blocks);
        w.u8(u8::from(self.snap.valid));
        w.u8(self.snap.slot);
        w.u32(self.snap.len);
        w.put(&self.snap.data_hash);
        w.pos(&self.snap.at);
        w.pos(&self.retain);
        w.u32(self.epoch);
        debug_assert_eq!(w.o, SB_BODY_LEN);
    }

    fn mac_for(&self, key: &[u8; 32]) -> [u8; 32] {
        let mut body = [0u8; SB_BODY_LEN];
        self.encode_body(&mut body);
        *blake3::keyed_hash(key, &body).as_bytes()
    }

    fn encode(&self, block_size: usize) -> Vec<u8> {
        let mut b = vec![0u8; block_size];
        self.encode_body(&mut b[..SB_BODY_LEN]);
        b[SB_BODY_LEN..SB_BODY_LEN + 32].copy_from_slice(&self.mac);
        let crc = crc32c(&b[..SB_BODY_LEN + 32]);
        b[SB_BODY_LEN + 32..SB_LEN].copy_from_slice(&crc.to_le_bytes());
        b
    }

    fn decode(b: &[u8]) -> Option<Superblock> {
        if b.len() < SB_LEN {
            return None;
        }
        let crc = u32::from_le_bytes(b[SB_BODY_LEN + 32..SB_LEN].try_into().ok()?);
        if crc32c(&b[..SB_BODY_LEN + 32]) != crc {
            return None;
        }
        let mut r = R { b, o: 0 };
        if r.u64() != SB_MAGIC || r.u32() != FORMAT_VERSION {
            return None;
        }
        let flags = r.u32();
        let generation = r.u64();
        let store_id = r.take::<16>();
        let geom = Geometry {
            block_size: r.u32() as usize,
            total_blocks: r.u64(),
            snap_start: r.u64(),
            snap_blocks: r.u64(),
            log_start: r.u64(),
            log_blocks: r.u64(),
        };
        let snap = SnapInfo {
            valid: r.u8() != 0,
            slot: r.u8(),
            len: r.u32(),
            data_hash: r.take(),
            at: r.pos(),
        };
        let retain = r.pos();
        let epoch = r.u32();
        let mac = b[SB_BODY_LEN..SB_BODY_LEN + 32].try_into().ok()?;
        let sb = Superblock {
            generation,
            flags,
            store_id,
            geom,
            snap,
            retain,
            epoch,
            mac,
        };
        sb.sane().then_some(sb)
    }

    fn sane(&self) -> bool {
        let g = &self.geom;
        let in_log = |p: &Pos| p.lba >= g.log_start && p.lba < g.log_end() && p.seq >= 1;
        g.block_size >= 512
            && g.block_size.is_power_of_two()
            && g.snap_start == 2
            && g.log_start == 2 + 2 * g.snap_blocks
            && g.log_start.checked_add(g.log_blocks) == Some(g.total_blocks)
            && self.snap.slot < 2
            && u64::from(self.snap.len) <= g.snap_blocks * g.block_size as u64
            && in_log(&self.snap.at)
            && in_log(&self.retain)
    }
}

fn genesis_hash(store_id: &[u8; 16]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"POS-GENESIS");
    h.update(store_id);
    *h.finalize().as_bytes()
}

/// Chain hash of a record: `BLAKE3(prev ‖ seq ‖ kind ‖ epoch ‖ payload)`.
pub fn chain_hash(prev: &[u8; 32], seq: u64, kind: u16, epoch: u32, payload: &[u8]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(prev);
    h.update(&seq.to_le_bytes());
    h.update(&kind.to_le_bytes());
    h.update(&epoch.to_le_bytes());
    h.update(payload);
    *h.finalize().as_bytes()
}

fn encode_record(
    bs: usize,
    seq: u64,
    kind: u16,
    epoch: u32,
    hash: &[u8; 32],
    payload: &[u8],
) -> Vec<u8> {
    let total = (HEADER_LEN + payload.len()).div_ceil(bs) * bs;
    let mut b = vec![0u8; total];
    {
        let mut w = W {
            b: &mut b[..HEADER_LEN],
            o: 0,
        };
        w.u32(REC_MAGIC);
        w.u16(REC_VERSION);
        w.u16(kind);
        w.u64(seq);
        w.u32(payload.len() as u32);
        w.u32(epoch);
        w.put(hash);
        // crc at [56..60], reserved [60..64]
    }
    b[HEADER_LEN..HEADER_LEN + payload.len()].copy_from_slice(payload);
    let mut c = crc32c::Crc32c::default();
    c.update(&b[..HEADER_LEN]);
    c.update(payload);
    let crc = c.finish();
    b[56..60].copy_from_slice(&crc.to_le_bytes());
    b
}

/// A record read back from the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub seq: u64,
    pub kind: u16,
    pub hash: [u8; 32],
    pub lba: u64,
    pub payload: Vec<u8>,
}

/// Returned by a successful commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Committed {
    pub seq: u64,
    pub hash: [u8; 32],
    pub lba: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    /// MAC verified with this machine's key.
    Bound,
    /// Fresh, empty, never bound (factory image). Bind before use.
    Unbound,
    /// MAC does not verify (other machine, or tampered). Requires the
    /// manager-authorised recovery flow, then [`Store::bind`].
    Foreign,
}

#[derive(Debug)]
pub struct Store<D: BlockDevice> {
    dev: D,
    g: Geometry,
    sb: Superblock,
    /// Snapshot position of the other (older) valid superblock, if any.
    other_snap_at: Option<Pos>,
    head: Pos,
    key: [u8; 32],
    trust: Trust,
    since_snapshot: u64,
    /// Set after any failed write: the on-disk tail is unknown until the
    /// store is reopened (which recovers and bumps the epoch).
    poisoned: bool,
}

#[derive(Debug)]
pub struct Recovered<D: BlockDevice> {
    pub store: Store<D>,
    /// Snapshot payload, if one has been taken.
    pub snapshot: Option<Vec<u8>>,
    /// Records after the snapshot, in order (WRAP records omitted).
    pub records: Vec<Record>,
    /// True if a damaged/torn tail was found and ignored.
    pub truncated: bool,
}

#[derive(Debug)]
pub struct OpenError<D> {
    pub error: StoreError,
    pub dev: D,
}

/// Bytes for LBA 0 and 1 of a freshly formatted, unbound store. Used by the
/// image builder; the rest of the partition may be anything.
pub fn format_image(block_size: usize, total_blocks: u64) -> SResult<Vec<u8>> {
    let g = Geometry::for_device(block_size, total_blocks)?;
    let sb = fresh_sb(g, [0u8; 16]);
    let mut out = Superblock {
        generation: 0,
        ..sb
    }
    .encode(block_size);
    out.extend_from_slice(
        &Superblock {
            generation: 1,
            ..sb
        }
        .encode(block_size),
    );
    Ok(out)
}

fn fresh_sb(g: Geometry, store_id: [u8; 16]) -> Superblock {
    let start = Pos {
        seq: 1,
        lba: g.log_start,
        prev_hash: genesis_hash(&store_id),
    };
    Superblock {
        generation: 1,
        flags: 0,
        store_id,
        geom: g,
        snap: SnapInfo {
            valid: false,
            slot: 0,
            len: 0,
            data_hash: [0; 32],
            at: start,
        },
        retain: start,
        epoch: 0,
        mac: [0; 32],
    }
}

/// Write two blank (unbound) superblocks. Destroys any existing store.
pub fn format<D: BlockDevice>(dev: &mut D) -> SResult<()> {
    let bs = dev.block_size();
    let img = format_image(bs, dev.block_count())?;
    dev.write(0, &img[..bs])?;
    dev.flush()?;
    dev.write(1, &img[bs..])?;
    dev.flush()?;
    Ok(())
}

impl<D: BlockDevice> Store<D> {
    /// Open and recover the store. `key` is this machine's binding key.
    pub fn open(mut dev: D, key: &[u8; 32]) -> Result<Recovered<D>, OpenError<D>> {
        match Self::open_inner(&mut dev, key) {
            Ok((sb, other, snapshot, records, head, truncated)) => {
                let trust = if sb.flags & SB_FLAG_BOUND != 0 {
                    if sb.mac_for(key) == sb.mac {
                        Trust::Bound
                    } else {
                        Trust::Foreign
                    }
                } else if !sb.snap.valid && records.is_empty() {
                    Trust::Unbound
                } else {
                    // Unbound but holding data: never produced by this code.
                    Trust::Foreign
                };
                let g = sb.geom;
                let since = records.len() as u64;
                let mut store = Store {
                    dev,
                    g,
                    sb,
                    other_snap_at: other,
                    head,
                    key: *key,
                    trust,
                    since_snapshot: since,
                    poisoned: false,
                };
                if trust == Trust::Bound {
                    let mut nsb = store.sb;
                    nsb.epoch = nsb.epoch.wrapping_add(1);
                    if let Err(error) = store.write_sb(nsb) {
                        return Err(OpenError {
                            error,
                            dev: store.dev,
                        });
                    }
                }
                Ok(Recovered {
                    store,
                    snapshot,
                    records,
                    truncated,
                })
            }
            Err(error) => Err(OpenError { error, dev }),
        }
    }

    #[allow(clippy::type_complexity)]
    fn open_inner(
        dev: &mut D,
        _key: &[u8; 32],
    ) -> SResult<(
        Superblock,
        Option<Pos>,
        Option<Vec<u8>>,
        Vec<Record>,
        Pos,
        bool,
    )> {
        let bs = dev.block_size();
        let mut a = vec![0u8; bs];
        let mut b = vec![0u8; bs];
        dev.read(0, &mut a)?;
        dev.read(1, &mut b)?;
        let sa = Superblock::decode(&a);
        let sb = Superblock::decode(&b);
        let mut cands: Vec<Superblock> = [sa, sb].into_iter().flatten().collect();
        cands.retain(|s| s.geom.block_size == bs && s.geom.total_blocks <= dev.block_count());
        cands.sort_by_key(|s| core::cmp::Reverse(s.generation));
        if cands.is_empty() {
            if a.iter().all(|&x| x == 0) && b.iter().all(|&x| x == 0) {
                return Err(StoreError::Unformatted);
            }
            return Err(StoreError::Corrupt("no valid superblock"));
        }
        // Newest superblock whose snapshot verifies wins.
        let mut last_err = StoreError::Corrupt("snapshot");
        for (i, s) in cands.iter().enumerate() {
            match read_snapshot(dev, s) {
                Ok(snapshot) => {
                    let other = if i == 0 {
                        cands.get(1).map(|o| o.snap.at)
                    } else {
                        None
                    };
                    let (records, head, truncated) =
                        scan(dev, &s.geom, s.snap.at, u64::MAX, |_| true)?;
                    return Ok((*s, other, snapshot, records, head, truncated));
                }
                Err(e) => last_err = e,
            }
        }
        Err(last_err)
    }

    pub fn trust(&self) -> Trust {
        self.trust
    }
    pub fn store_id(&self) -> [u8; 16] {
        self.sb.store_id
    }
    pub fn geometry(&self) -> Geometry {
        self.g
    }
    /// Sequence number the next record will get.
    pub fn next_seq(&self) -> u64 {
        self.head.seq
    }
    /// Chain hash of the last committed record.
    pub fn head_hash(&self) -> [u8; 32] {
        self.head.prev_hash
    }
    pub fn records_since_snapshot(&self) -> u64 {
        self.since_snapshot
    }
    pub fn device(&mut self) -> &mut D {
        &mut self.dev
    }

    /// Oldest position that must not be overwritten.
    fn protect(&self) -> Pos {
        let mut p = self.sb.retain;
        for q in [Some(self.sb.snap.at), self.other_snap_at]
            .into_iter()
            .flatten()
        {
            if q.seq < p.seq {
                p = q;
            }
        }
        p
    }

    /// (used, total) log blocks. `used` counts everything from the oldest
    /// protected record to the head.
    pub fn usage(&self) -> (u64, u64) {
        let p = self.protect();
        let n = self.g.log_blocks;
        let used = if self.head.seq == p.seq {
            0
        } else {
            (self.head.lba + n - p.lba) % n
        };
        (
            if used == 0 && self.head.seq != p.seq {
                n
            } else {
                used
            },
            n,
        )
    }

    /// Bind (or re-bind) the store to `key`. A fresh store also gets the new
    /// random `store_id`; changing the id of a store that holds data is
    /// refused (it would break the hash chain).
    pub fn bind(&mut self, key: &[u8; 32], store_id: Option<[u8; 16]>) -> SResult<()> {
        let mut sb = self.sb;
        if let Some(id) = store_id
            && id != sb.store_id {
                if sb.snap.valid || self.head.seq != 1 {
                    return Err(StoreError::NotEmpty);
                }
                sb = fresh_sb(self.g, id);
                sb.generation = self.sb.generation;
            }
        sb.flags |= SB_FLAG_BOUND;
        sb.epoch = self.sb.epoch.wrapping_add(1);
        self.key = *key;
        if self.head.seq == 1 && !sb.snap.valid {
            self.head = sb.snap.at;
            self.other_snap_at = None;
        }
        // Write both copies so a stale copy cannot resurface.
        self.write_sb(sb)?;
        self.write_sb(sb)?;
        self.other_snap_at = Some(self.sb.snap.at);
        self.trust = Trust::Bound;
        Ok(())
    }

    fn write_sb(&mut self, mut sb: Superblock) -> SResult<()> {
        sb.generation = self.sb.generation + 1;
        sb.mac = if sb.flags & SB_FLAG_BOUND != 0 {
            sb.mac_for(&self.key)
        } else {
            [0; 32]
        };
        let bytes = sb.encode(self.g.block_size);
        self.guard(|d| {
            d.write(sb.generation % 2, &bytes)?;
            d.flush()
        })?;
        self.other_snap_at = Some(self.sb.snap.at);
        self.sb = sb;
        Ok(())
    }

    fn guard(&mut self, f: impl FnOnce(&mut D) -> Result<(), HalError>) -> SResult<()> {
        if self.poisoned {
            return Err(StoreError::Hal(HalError::Io));
        }
        f(&mut self.dev).map_err(|e| {
            self.poisoned = true;
            StoreError::Hal(e)
        })
    }

    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Commit one record durably. Returns only after the flush succeeded.
    pub fn append(&mut self, kind: u16, payload: &[u8]) -> SResult<Committed> {
        Ok(self.append_batch(&[(kind, payload)])?[0])
    }

    /// Commit several records with one sequential write and one flush. After
    /// a power cut a *prefix* of the batch may survive.
    pub fn append_batch(&mut self, recs: &[(u16, &[u8])]) -> SResult<Vec<Committed>> {
        if self.trust != Trust::Bound {
            return Err(StoreError::NotTrusted);
        }
        let epoch = self.sb.epoch;
        let bs = self.g.block_size;
        let mut seq = self.head.seq;
        let mut prev = self.head.prev_hash;
        let mut lba = self.head.lba;
        // Planned writes: (lba, bytes).
        let mut writes: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut out = Vec::with_capacity(recs.len());
        let mut need_blocks: u64 = 0;
        for &(kind, payload) in recs {
            if kind == KIND_WRAP || payload.len() > MAX_PAYLOAD {
                return Err(StoreError::TooLarge);
            }
            let nblk = self.g.blocks_for(HEADER_LEN + payload.len());
            if nblk + 1 >= self.g.log_blocks {
                return Err(StoreError::TooLarge);
            }
            if lba + nblk > self.g.log_end() {
                // Not enough room before the end: WRAP marker, then restart.
                let h = chain_hash(&prev, seq, KIND_WRAP, epoch, &[]);
                writes.push((lba, encode_record(bs, seq, KIND_WRAP, epoch, &h, &[])));
                need_blocks += self.g.log_end() - lba;
                prev = h;
                seq += 1;
                lba = self.g.log_start;
            }
            let h = chain_hash(&prev, seq, kind, epoch, payload);
            writes.push((lba, encode_record(bs, seq, kind, epoch, &h, payload)));
            out.push(Committed { seq, hash: h, lba });
            need_blocks += nblk;
            prev = h;
            seq += 1;
            lba += nblk;
            if lba == self.g.log_end() {
                lba = self.g.log_start;
            }
        }
        let (used, total) = self.usage();
        // Keep at least one block between head and tail.
        if used + need_blocks >= total {
            return Err(StoreError::Full);
        }
        // Merge contiguous writes so each batch is as few sequential I/Os as
        // possible (one, unless it wraps).
        let mut merged: Vec<(u64, Vec<u8>)> = Vec::new();
        for (l, b) in writes {
            match merged.last_mut() {
                Some((ml, mb)) if *ml + (mb.len() / bs) as u64 == l => mb.extend_from_slice(&b),
                _ => merged.push((l, b)),
            }
        }
        self.guard(|d| {
            for (l, b) in &merged {
                d.write(*l, b)?;
            }
            d.flush()
        })?;
        self.head = Pos {
            seq,
            lba,
            prev_hash: prev,
        };
        self.since_snapshot += recs.len() as u64;
        Ok(out)
    }

    /// Persist a snapshot of the state as of the last committed record.
    pub fn write_snapshot(&mut self, data: &[u8]) -> SResult<()> {
        if self.trust != Trust::Bound {
            return Err(StoreError::NotTrusted);
        }
        let bs = self.g.block_size;
        let nblk = self.g.blocks_for(data.len());
        if nblk > self.g.snap_blocks || data.len() > u32::MAX as usize {
            return Err(StoreError::TooLarge);
        }
        let slot = if self.sb.snap.valid {
            1 - self.sb.snap.slot
        } else {
            0
        };
        let start = self.g.snap_start + u64::from(slot) * self.g.snap_blocks;
        let mut buf = vec![0u8; nblk as usize * bs];
        buf[..data.len()].copy_from_slice(data);
        self.guard(|d| {
            if !buf.is_empty() {
                d.write(start, &buf)?;
            }
            d.flush()
        })?;
        let mut sb = self.sb;
        sb.snap = SnapInfo {
            valid: true,
            slot,
            len: data.len() as u32,
            data_hash: *blake3::hash(data).as_bytes(),
            at: self.head,
        };
        self.write_sb(sb)?;
        self.since_snapshot = 0;
        Ok(())
    }

    /// Mark everything before `pos` as exported: those records may now be
    /// overwritten once no snapshot needs them. `pos` must be a record
    /// boundary returned by [`Store::scan`] (or the head).
    pub fn set_retain(&mut self, pos: Pos) -> SResult<()> {
        if self.trust != Trust::Bound {
            return Err(StoreError::NotTrusted);
        }
        if pos.seq < self.sb.retain.seq || pos.seq > self.head.seq {
            return Err(StoreError::Corrupt("retain position out of range"));
        }
        let mut sb = self.sb;
        sb.retain = pos;
        self.write_sb(sb)?;
        self.write_sb(sb)
    }

    /// Current head as a retain position (everything committed so far).
    pub fn head_pos(&self) -> Pos {
        self.head
    }

    /// Verify and visit every retained record from the retain point
    /// (oldest unexported) onwards, checking the hash chain end to end.
    /// `f` returns `false` to stop early.
    pub fn scan_retained(&mut self, mut f: impl FnMut(&Record) -> bool) -> SResult<Pos> {
        let start = self.sb.retain;
        let head_seq = self.head.seq;
        let (_, end, _) = scan(&mut self.dev, &self.g, start, head_seq, |r| f(r))?;
        Ok(end)
    }
}

fn read_snapshot<D: BlockDevice>(dev: &mut D, s: &Superblock) -> SResult<Option<Vec<u8>>> {
    if !s.snap.valid {
        return Ok(None);
    }
    let g = &s.geom;
    let len = s.snap.len as usize;
    let nblk = g.blocks_for(len);
    let start = g.snap_start + u64::from(s.snap.slot) * g.snap_blocks;
    let mut buf = vec![0u8; nblk as usize * g.block_size];
    if !buf.is_empty() {
        dev.read(start, &mut buf)?;
    }
    buf.truncate(len);
    if *blake3::hash(&buf).as_bytes() != s.snap.data_hash {
        return Err(StoreError::Corrupt("snapshot hash"));
    }
    Ok(Some(buf))
}

/// Scan records starting at `start` until an invalid record or `stop_seq`.
/// Returns (records without WRAPs, position after the last valid record,
/// whether a damaged record — rather than clean blank space — ended the scan).
fn scan<D: BlockDevice>(
    dev: &mut D,
    g: &Geometry,
    start: Pos,
    stop_seq: u64,
    mut keep: impl FnMut(&Record) -> bool,
) -> SResult<(Vec<Record>, Pos, bool)> {
    let bs = g.block_size;
    let mut pos = start;
    let mut out = Vec::new();
    let mut first = vec![0u8; bs];
    let mut steps: u64 = 0;
    loop {
        if pos.seq >= stop_seq || steps > g.log_blocks {
            return Ok((out, pos, false));
        }
        steps += 1;
        if pos.lba >= g.log_end() {
            pos.lba = g.log_start;
        }
        dev.read(pos.lba, &mut first)?;
        let mut r = R { b: &first, o: 0 };
        let magic = r.u32();
        let ver = r.u16();
        let kind = r.u16();
        let seq = r.u64();
        let len = r.u32() as usize;
        let epoch = r.u32();
        let hash: [u8; 32] = r.take();
        let crc = r.u32();
        // A blank block or an older lap's record means "end of log", not damage.
        if magic != REC_MAGIC || seq != pos.seq {
            let damaged = magic == REC_MAGIC && seq > pos.seq;
            return Ok((
                out,
                pos,
                damaged || (magic != 0 && magic != REC_MAGIC && seq_like_damage(&first)),
            ));
        }
        if ver != REC_VERSION || len > MAX_PAYLOAD {
            return Ok((out, pos, true));
        }
        let nblk = g.blocks_for(HEADER_LEN + len);
        if pos.lba + nblk > g.log_end() {
            return Ok((out, pos, true));
        }
        let mut full = vec![0u8; nblk as usize * bs];
        full[..bs].copy_from_slice(&first);
        if nblk > 1 {
            dev.read(pos.lba + 1, &mut full[bs..])?;
        }
        let payload = &full[HEADER_LEN..HEADER_LEN + len];
        let mut c = crc32c::Crc32c::default();
        let mut hdr = [0u8; HEADER_LEN];
        hdr.copy_from_slice(&full[..HEADER_LEN]);
        hdr[56..60].fill(0);
        c.update(&hdr);
        c.update(payload);
        if c.finish() != crc || chain_hash(&pos.prev_hash, seq, kind, epoch, payload) != hash {
            return Ok((out, pos, true));
        }
        if kind == KIND_WRAP {
            pos = Pos {
                seq: seq + 1,
                lba: g.log_start,
                prev_hash: hash,
            };
            continue;
        }
        let rec = Record {
            seq,
            kind,
            hash,
            lba: pos.lba,
            payload: payload.to_vec(),
        };
        let cont = keep(&rec);
        out.push(rec);
        pos = Pos {
            seq: seq + 1,
            lba: pos.lba + nblk,
            prev_hash: hash,
        };
        if pos.lba == g.log_end() {
            pos.lba = g.log_start;
        }
        if !cont {
            return Ok((out, pos, false));
        }
    }
}

/// Heuristic for reporting only: non-zero garbage where a record header
/// should be.
fn seq_like_damage(b: &[u8]) -> bool {
    b.iter().any(|&x| x != 0)
}
