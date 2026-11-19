//! Power-cut fault injection.
//!
//! `FaultDevice` models a USB stick with a volatile write cache:
//! * writes land in a cache, reads see the cache, `flush` makes them durable;
//! * at the cut (the N-th block write), that block is torn (old, new, a
//!   new-prefix/old-suffix mix, or garbage) and each earlier *unflushed* block
//!   independently survives or is lost (the cache may drain in any order);
//! * after the cut every operation fails.
//!
//! For each random workload we first count its block writes W, then rerun it
//! W times, cutting power at every write index 0..W, and check after recovery:
//! 1. every record acknowledged before the cut is present, in order;
//! 2. nothing else is present except a prefix of the batch in flight;
//! 3. the hash chain and snapshot verify (enforced by `open`), and
//! 4. the store keeps working: append more, reopen, and everything is there.

use pos_hal::{BlockDevice, HalError, HalResult};
use pos_store::{Recovered, Store, StoreError, Trust, format};
use std::cell::RefCell;
use std::rc::Rc;

const KEY: [u8; 32] = [7u8; 32];
const BS: usize = 512;

#[derive(Clone)]
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

struct Inner {
    durable: Vec<u8>,
    cache: Vec<(u64, Vec<u8>)>,
    writes: u64,
    cut_at: Option<u64>,
    dead: bool,
    rng: Rng,
}

#[derive(Clone)]
struct FaultDevice(Rc<RefCell<Inner>>);

impl std::fmt::Debug for FaultDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FaultDevice")
    }
}

impl FaultDevice {
    fn new(blocks: u64, seed: u64) -> Self {
        FaultDevice(Rc::new(RefCell::new(Inner {
            durable: vec![0; BS * blocks as usize],
            cache: Vec::new(),
            writes: 0,
            cut_at: None,
            dead: false,
            rng: Rng(seed | 1),
        })))
    }
    fn writes(&self) -> u64 {
        self.0.borrow().writes
    }
    fn arm(&self, at: Option<u64>) {
        let mut i = self.0.borrow_mut();
        i.cut_at = at;
        i.writes = 0;
    }
    fn dead(&self) -> bool {
        self.0.borrow().dead
    }
    /// Power back on: a fresh device over the durable bytes.
    fn reboot(&self) -> FaultDevice {
        let i = self.0.borrow();
        FaultDevice(Rc::new(RefCell::new(Inner {
            durable: i.durable.clone(),
            cache: Vec::new(),
            writes: 0,
            cut_at: None,
            dead: false,
            rng: i.rng.clone(),
        })))
    }
}

fn tear(rng: &mut Rng, old: &[u8], new: &[u8]) -> Vec<u8> {
    match rng.below(4) {
        0 => old.to_vec(),
        1 => new.to_vec(),
        2 => {
            let k = rng.below(old.len() as u64) as usize;
            let mut v = new[..k].to_vec();
            v.extend_from_slice(&old[k..]);
            v
        }
        _ => (0..old.len()).map(|_| rng.next() as u8).collect(),
    }
}

impl BlockDevice for FaultDevice {
    fn block_size(&self) -> usize {
        BS
    }
    fn block_count(&self) -> u64 {
        (self.0.borrow().durable.len() / BS) as u64
    }
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> HalResult<()> {
        let i = self.0.borrow();
        if i.dead {
            return Err(HalError::Io);
        }
        let n = buf.len() / BS;
        if !buf.len().is_multiple_of(BS) || lba as usize + n > i.durable.len() / BS {
            return Err(HalError::InvalidArgument);
        }
        for k in 0..n {
            let b = lba + k as u64;
            let src = i
                .cache
                .iter()
                .rev()
                .find(|(l, _)| *l == b)
                .map(|(_, d)| d.as_slice())
                .unwrap_or(&i.durable[b as usize * BS..(b as usize + 1) * BS]);
            buf[k * BS..(k + 1) * BS].copy_from_slice(src);
        }
        Ok(())
    }
    fn write(&mut self, lba: u64, buf: &[u8]) -> HalResult<()> {
        let mut i = self.0.borrow_mut();
        if i.dead {
            return Err(HalError::Io);
        }
        if !buf.len().is_multiple_of(BS) || lba as usize + buf.len() / BS > i.durable.len() / BS {
            return Err(HalError::InvalidArgument);
        }
        for k in 0..buf.len() / BS {
            let b = lba + k as u64;
            let data = buf[k * BS..(k + 1) * BS].to_vec();
            if i.cut_at == Some(i.writes) {
                // Power fails during this block write.
                let inner = &mut *i;
                let cache = std::mem::take(&mut inner.cache);
                for (l, d) in cache {
                    if inner.rng.below(2) == 0 {
                        inner.durable[l as usize * BS..(l as usize + 1) * BS].copy_from_slice(&d);
                    }
                }
                let old = inner.durable[b as usize * BS..(b as usize + 1) * BS].to_vec();
                let torn = tear(&mut inner.rng, &old, &data);
                inner.durable[b as usize * BS..(b as usize + 1) * BS].copy_from_slice(&torn);
                inner.dead = true;
                return Err(HalError::Io);
            }
            i.writes += 1;
            i.cache.push((b, data));
        }
        Ok(())
    }
    fn flush(&mut self) -> HalResult<()> {
        let mut i = self.0.borrow_mut();
        if i.dead {
            return Err(HalError::Io);
        }
        let inner = &mut *i;
        for (l, d) in std::mem::take(&mut inner.cache) {
            inner.durable[l as usize * BS..(l as usize + 1) * BS].copy_from_slice(&d);
        }
        Ok(())
    }
}

/// Logical state = ordered list of record ids. Payload = id ‖ filler.
fn payload(id: u32, len: usize) -> Vec<u8> {
    let mut v = id.to_le_bytes().to_vec();
    v.extend((0..len).map(|k| (id as usize + k) as u8));
    v
}
fn id_of(p: &[u8]) -> u32 {
    u32::from_le_bytes(p[..4].try_into().unwrap())
}
fn encode_ids(ids: &[u32]) -> Vec<u8> {
    ids.iter().flat_map(|i| i.to_le_bytes()).collect()
}
fn decode_ids(b: &[u8]) -> Vec<u32> {
    b.chunks(4)
        .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
        .collect()
}

fn state_of(r: &Recovered<FaultDevice>) -> Vec<u32> {
    let mut ids = r.snapshot.as_deref().map(decode_ids).unwrap_or_default();
    ids.extend(r.records.iter().map(|x| id_of(&x.payload)));
    ids
}

#[derive(Default)]
struct Outcome {
    acked: Vec<u32>,
    in_flight: Vec<u32>,
    wraps: u32,
    fulls: u32,
    snapshots: u32,
}

/// Run a random workload against `dev`. Stops at the first error (power cut).
fn workload(dev: FaultDevice, seed: u64, ops: usize) -> Outcome {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut out = Outcome::default();
    let mut store = match Store::open(dev, &KEY) {
        Ok(r) => r.store,
        Err(_) => return out,
    };
    let mut next_id = 1u32;
    let mut last_lba = 0u64;
    for _ in 0..ops {
        let op = rng.below(20);
        let res: Result<(), StoreError> = if op < 13 {
            let id = next_id;
            next_id += 1;
            out.in_flight = vec![id];
            let max = if rng.below(10) == 0 { 3000 } else { 300 };
            let len = rng.below(max) as usize;
            store.append(1, &payload(id, len)).map(|c| {
                if c.lba < last_lba {
                    out.wraps += 1;
                }
                last_lba = c.lba;
            })
        } else if op < 16 {
            let n = 2 + rng.below(3) as u32;
            let ids: Vec<u32> = (0..n).map(|k| next_id + k).collect();
            next_id += n;
            out.in_flight = ids.clone();
            let ps: Vec<Vec<u8>> = ids
                .iter()
                .map(|&i| payload(i, rng.below(700) as usize))
                .collect();
            let recs: Vec<(u16, &[u8])> = ps.iter().map(|p| (2u16, p.as_slice())).collect();
            store.append_batch(&recs).map(|_| ())
        } else if op < 19 || rng.below(4) != 0 {
            out.in_flight.clear();
            out.snapshots += 1;
            store.write_snapshot(&encode_ids(&out.acked))
        } else {
            out.in_flight.clear();
            let head = store.head_pos();
            store.set_retain(head)
        };
        match res {
            Ok(()) => out.acked.extend(std::mem::take(&mut out.in_flight)),
            Err(StoreError::Full) => {
                out.fulls += 1;
                // What the app does: export (retain at head) + snapshot.
                out.in_flight.clear();
                let head = store.head_pos();
                let r = store
                    .set_retain(head)
                    .and_then(|_| store.write_snapshot(&encode_ids(&out.acked)));
                if r.is_err() {
                    return out;
                }
            }
            Err(_) => return out,
        }
    }
    out.in_flight.clear();
    out
}

fn fresh_device(seed: u64, blocks: u64) -> FaultDevice {
    let mut d = FaultDevice::new(blocks, seed);
    format(&mut d).unwrap();
    let mut s = Store::open(d.clone(), &KEY).unwrap().store;
    s.bind(&KEY, Some([seed as u8; 16])).unwrap();
    d.arm(None);
    d
}

fn check(seed: u64, cut: u64, out: &Outcome, rec: &Recovered<FaultDevice>) {
    let got = state_of(rec);
    let n = out.acked.len();
    assert!(
        got.len() >= n && got[..n] == out.acked[..],
        "seed {seed} cut {cut}: acknowledged records lost\n acked {:?}\n got   {:?}",
        out.acked,
        got
    );
    let extra = &got[n..];
    assert!(
        extra.len() <= out.in_flight.len() && *extra == out.in_flight[..extra.len()],
        "seed {seed} cut {cut}: unexpected records {extra:?} (in flight {:?})",
        out.in_flight
    );
}

fn run_seed(seed: u64, blocks: u64, ops: usize) -> (u64, u32, u32, u32) {
    // Dry run to count writes.
    let base = fresh_device(seed, blocks);
    let probe = base.reboot();
    probe.arm(None);
    let full = workload(probe.clone(), seed, ops);
    let total = probe.writes();
    assert!(!full.acked.is_empty());
    let stats = (full.wraps, full.fulls, full.snapshots);
    let mut runs = 0;
    for cut in 0..total {
        let dev = base.reboot();
        dev.arm(Some(cut));
        let out = workload(dev.clone(), seed, ops);
        assert!(dev.dead(), "seed {seed}: cut {cut} not reached");
        // Power back on.
        let rec = Store::open(dev.reboot(), &KEY)
            .unwrap_or_else(|e| panic!("seed {seed} cut {cut}: open failed: {:?}", e.error));
        assert_eq!(rec.store.trust(), Trust::Bound);
        check(seed, cut, &out, &rec);
        // Store must remain usable: append, reopen, verify again.
        let mut expect = state_of(&rec);
        let mut s = rec.store;
        for k in 0..3u32 {
            let id = 1_000_000 + k;
            if let Err(StoreError::Full) = s.append(9, &payload(id, 100)) {
                let head = s.head_pos();
                s.set_retain(head).unwrap();
                s.write_snapshot(&encode_ids(&expect)).unwrap();
                s.write_snapshot(&encode_ids(&expect)).unwrap();
                s.append(9, &payload(id, 100)).unwrap();
            }
            expect.push(id);
        }
        let dev2 = s.device().reboot();
        let rec2 =
            Store::open(dev2, &KEY).unwrap_or_else(|_| panic!("seed {seed} cut {cut}: reopen"));
        assert_eq!(
            state_of(&rec2),
            expect,
            "seed {seed} cut {cut}: post-recovery appends"
        );
        runs += 1;
    }
    (runs, stats.0, stats.1, stats.2)
}

#[test]
fn power_cut_at_every_write_index() {
    // Small device (log ≈ 340 blocks) so workloads wrap the ring, fill it
    // and take snapshots repeatedly.
    let seeds: u64 = std::env::var("POS_FAULT_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(48);
    let (mut runs, mut wraps, mut fulls, mut snaps) = (0, 0, 0, 0);
    for seed in 0..seeds {
        let (r, w, f, s) = run_seed(seed, 400, 160);
        runs += r;
        wraps += w;
        fulls += f;
        snaps += s;
    }
    eprintln!(
        "fault injection: {runs} power-cut runs, all recovered consistently \
         (workloads: {wraps} ring wraps, {fulls} log-full events, {snaps} snapshots)"
    );
    assert!(runs >= 10_000, "only {runs} runs");
    assert!(
        wraps > 0 && fulls > 0 && snaps > 0,
        "workload did not exercise wrap/full/snapshot"
    );
}

#[test]
fn cut_during_format_or_bind_is_detected_not_misread() {
    for cut in 0..8 {
        let mut d = FaultDevice::new(600, 99);
        d.arm(Some(cut));
        let _ = format(&mut d).and_then(|_| {
            let mut s = Store::open(d.clone(), &KEY).map_err(|e| e.error)?.store;
            s.bind(&KEY, Some([1; 16]))
        });
        match Store::open(d.reboot(), &KEY) {
            Ok(r) => {
                assert!(r.records.is_empty() && r.snapshot.is_none());
                assert!(matches!(r.store.trust(), Trust::Bound | Trust::Unbound));
            }
            Err(e) => assert!(matches!(
                e.error,
                StoreError::Unformatted | StoreError::Corrupt(_)
            )),
        }
    }
}

#[test]
fn foreign_key_is_detected_and_appends_refused() {
    let d = fresh_device(5, 600);
    let mut s = Store::open(d.reboot(), &KEY).unwrap().store;
    s.append(1, b"hello").unwrap();
    let dev = s.device().reboot();
    let mut r = Store::open(dev, &[9u8; 32]).unwrap();
    assert_eq!(r.store.trust(), Trust::Foreign);
    assert_eq!(r.records.len(), 1);
    assert_eq!(r.store.append(1, b"x"), Err(StoreError::NotTrusted));
    // Manager-authorised re-bind (same store id) makes it usable again.
    r.store.bind(&[9u8; 32], None).unwrap();
    r.store.append(1, b"y").unwrap();
    // Changing the store id of a non-empty store is refused.
    assert_eq!(
        r.store.bind(&[9u8; 32], Some([3; 16])),
        Err(StoreError::NotEmpty)
    );
}

#[test]
fn tampering_with_history_breaks_the_chain() {
    let d = fresh_device(6, 600);
    let mut s = Store::open(d.reboot(), &KEY).unwrap().store;
    let mut lbas = vec![];
    for i in 0..5u32 {
        lbas.push(s.append(1, &payload(i, 50)).unwrap().lba);
    }
    let dev = s.device().reboot();
    // Flip one payload byte of record 3 and fix up its CRC: the chain hash
    // still exposes it, so recovery stops before it.
    {
        let mut inner = dev.0.borrow_mut();
        let off = lbas[2] as usize * BS + 64 + 6;
        inner.durable[off] ^= 0xFF;
        let len = u32::from_le_bytes(
            inner.durable[lbas[2] as usize * BS + 16..][..4]
                .try_into()
                .unwrap(),
        ) as usize;
        let mut hdr = inner.durable[lbas[2] as usize * BS..][..64].to_vec();
        hdr[56..60].fill(0);
        let mut c = pos_store::crc32c::Crc32c::default();
        c.update(&hdr);
        c.update(&inner.durable[lbas[2] as usize * BS + 64..][..len]);
        let crc = c.finish().to_le_bytes();
        inner.durable[lbas[2] as usize * BS + 56..][..4].copy_from_slice(&crc);
    }
    let r = Store::open(dev, &KEY).unwrap();
    assert_eq!(r.records.len(), 2);
    assert!(r.truncated);
}

#[test]
fn full_log_refuses_instead_of_overwriting() {
    let d = fresh_device(7, 600);
    let mut s = Store::open(d.reboot(), &KEY).unwrap().store;
    let mut n = 0u32;
    loop {
        match s.append(1, &payload(n, 400)) {
            Ok(_) => n += 1,
            Err(StoreError::Full) => break,
            Err(e) => panic!("{e:?}"),
        }
    }
    // Everything written is still there.
    let r = Store::open(s.device().reboot(), &KEY).unwrap();
    assert_eq!(r.records.len() as u32, n);
    // Snapshot alone does not free space while the records are unexported…
    let mut s = r.store;
    s.write_snapshot(&encode_ids(&(0..n).collect::<Vec<_>>()))
        .unwrap();
    assert_eq!(s.append(1, &payload(n, 400)), Err(StoreError::Full));
    // …but snapshot + export (retain) does, after the older superblock's
    // snapshot position is also superseded.
    let head = s.head_pos();
    s.set_retain(head).unwrap();
    s.write_snapshot(&encode_ids(&(0..n).collect::<Vec<_>>()))
        .unwrap();
    s.append(1, &payload(n, 400)).unwrap();
    // Export scan of retained records verifies the chain.
    let mut seen = 0;
    s.scan_retained(|_| {
        seen += 1;
        true
    })
    .unwrap();
    assert_eq!(seen, 1);
}
