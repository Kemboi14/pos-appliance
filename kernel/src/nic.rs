//! Ethernet drivers, polled (no interrupts), for the store network:
//!
//! * virtio-net (modern, virtio 1.0 PCI transport) — QEMU/KVM and other
//!   hypervisors.
//! * Intel 8254x/8257x ("e1000", "e1000e": 82540EM, 82545EM, 82574L) with
//!   legacy descriptors. Tested in QEMU (`e1000`, `e1000e`). Newer PCH LAN
//!   parts (I217/I218/I219) share the register set but need PHY/ULP
//!   workarounds this driver does not have; they are listed in
//!   docs/TIER_B.md as unsupported until validated on hardware.
//!
//! Probed only when the till is configured into a store network.

use alloc::vec::Vec;
use core::sync::atomic::{Ordering, fence};

use pos_hal::{HalError, HalResult};

use crate::mm::{self, Dma};
use crate::pci::Function;

const BUF: usize = 2048;
const RING: usize = 64;

/// Volatile MMIO at a mapped virtual address.
#[derive(Debug, Clone, Copy)]
struct Mmio(u64);

impl Mmio {
    fn r8(self, off: u64) -> u8 {
        // SAFETY: `self.0 + off` lies in a BAR range mapped by `map_mmio`.
        unsafe { ((self.0 + off) as *const u8).read_volatile() }
    }
    fn w8(self, off: u64, v: u8) {
        // SAFETY: as in `r8`.
        unsafe { ((self.0 + off) as *mut u8).write_volatile(v) }
    }
    fn r16(self, off: u64) -> u16 {
        // SAFETY: as in `r8`; offsets are naturally aligned by the spec.
        unsafe { ((self.0 + off) as *const u16).read_volatile() }
    }
    fn w16(self, off: u64, v: u16) {
        // SAFETY: as in `r16`.
        unsafe { ((self.0 + off) as *mut u16).write_volatile(v) }
    }
    fn r32(self, off: u64) -> u32 {
        // SAFETY: as in `r16`.
        unsafe { ((self.0 + off) as *const u32).read_volatile() }
    }
    fn w32(self, off: u64, v: u32) {
        // SAFETY: as in `r16`.
        unsafe { ((self.0 + off) as *mut u32).write_volatile(v) }
    }
    fn w64(self, off: u64, v: u64) {
        self.w32(off, v as u32);
        self.w32(off + 4, (v >> 32) as u32);
    }
}

fn dma16(d: &Dma, off: usize) -> u16 {
    assert!(off + 2 <= d.len());
    // SAFETY: in bounds (checked); device-written memory, hence volatile.
    unsafe { d.virt().add(off).cast::<u16>().read_volatile() }
}

fn dma16w(d: &Dma, off: usize, v: u16) {
    assert!(off + 2 <= d.len());
    // SAFETY: in bounds (checked); volatile so the device sees the store.
    unsafe { d.virt().add(off).cast::<u16>().write_volatile(v) }
}

fn dma8(d: &Dma, off: usize) -> u8 {
    assert!(off < d.len());
    // SAFETY: in bounds (checked).
    unsafe { d.virt().add(off).read_volatile() }
}

fn dma8w(d: &Dma, off: usize, v: u8) {
    assert!(off < d.len());
    // SAFETY: in bounds (checked).
    unsafe { d.virt().add(off).write_volatile(v) }
}

fn copy_from(d: &Dma, off: usize, dst: &mut [u8]) {
    assert!(off + dst.len() <= d.len());
    for (i, b) in dst.iter_mut().enumerate() {
        *b = dma8(d, off + i);
    }
}

fn copy_to(d: &Dma, off: usize, src: &[u8]) {
    assert!(off + src.len() <= d.len());
    for (i, b) in src.iter().enumerate() {
        dma8w(d, off + i, *b);
    }
}

fn bar_mmio(f: &Function, n: u8) -> Option<Mmio> {
    let (base, size) = f.memory_bar(n)?;
    mm::map_mmio(base, size).map(Mmio)
}

// ------------------------------------------------------------ virtio-net

const VIRTIO_F_VERSION_1: u64 = 1 << 32;
const VIRTIO_NET_F_MAC: u64 = 1 << 5;
const VIRTIO_NET_F_STATUS: u64 = 1 << 16;
/// virtio_net_hdr with num_buffers (VERSION_1).
const VNET_HDR: usize = 12;

struct Queue {
    size: u16,
    desc: Dma,
    avail: Dma,
    used: Dma,
    bufs: Dma,
    notify: u64,
    last_used: u16,
    avail_idx: u16,
    free: Vec<u16>,
}

impl Queue {
    fn new(common: Mmio, index: u16, notify_base: Mmio, mult: u32) -> Option<Queue> {
        common.w16(0x16, index);
        let max = common.r16(0x18);
        if max == 0 {
            return None;
        }
        let size = max.min(RING as u16);
        common.w16(0x18, size);
        let n = usize::from(size);
        let q = Queue {
            size,
            desc: Dma::new(16 * n, 4096)?,
            avail: Dma::new(6 + 2 * n, 4096)?,
            used: Dma::new(6 + 8 * n, 4096)?,
            bufs: Dma::new(BUF * n, 4096)?,
            notify: notify_base.0 + u64::from(common.r16(0x1e)) * u64::from(mult),
            last_used: 0,
            avail_idx: 0,
            free: (0..size).collect(),
        };
        for i in 0..n {
            q.desc.write64(16 * i, q.bufs.phys() + (BUF * i) as u64);
        }
        common.w64(0x20, q.desc.phys());
        common.w64(0x28, q.avail.phys());
        common.w64(0x30, q.used.phys());
        common.w16(0x1c, 1);
        Some(q)
    }

    fn push(&mut self, id: u16, len: u32, device_writes: bool) {
        let d = 16 * usize::from(id);
        self.desc.write32(d + 8, len);
        dma16w(&self.desc, d + 12, if device_writes { 2 } else { 0 });
        let slot = 4 + 2 * usize::from(self.avail_idx % self.size);
        dma16w(&self.avail, slot, id);
        fence(Ordering::SeqCst);
        self.avail_idx = self.avail_idx.wrapping_add(1);
        dma16w(&self.avail, 2, self.avail_idx);
        fence(Ordering::SeqCst);
    }

    fn kick(&self, index: u16) {
        Mmio(self.notify).w16(0, index);
    }

    /// Next completed (descriptor id, length).
    fn pop(&mut self) -> Option<(u16, u32)> {
        fence(Ordering::SeqCst);
        if dma16(&self.used, 2) == self.last_used {
            return None;
        }
        let e = 4 + 8 * usize::from(self.last_used % self.size);
        let id = self.used.read32(e) as u16;
        let len = self.used.read32(e + 4);
        self.last_used = self.last_used.wrapping_add(1);
        Some((id, len))
    }
}

pub struct VirtioNet {
    common: Mmio,
    device: Mmio,
    rx: Queue,
    tx: Queue,
    mac: [u8; 6],
    status_feature: bool,
}

impl VirtioNet {
    fn init(f: &Function) -> Option<VirtioNet> {
        f.enable_mmio_dma();
        // Walk the capability list for the modern transport structures.
        let (mut common, mut notify, mut device, mut mult) = (None, None, None, 0u32);
        let mut cap = (f.read(0x34) & 0xfc) as u8;
        let mut guard = 0;
        while cap != 0 && guard < 48 {
            guard += 1;
            let h = f.read(cap);
            if h & 0xff == 0x09 {
                let cfg_type = (h >> 24) as u8;
                let bar = (f.read(cap + 4) & 0xff) as u8;
                let off = u64::from(f.read(cap + 8));
                let at = bar_mmio(f, bar).map(|m| Mmio(m.0 + off));
                match cfg_type {
                    1 => common = at,
                    2 => {
                        notify = at;
                        mult = f.read(cap + 16);
                    }
                    4 => device = at,
                    _ => {}
                }
            }
            cap = ((h >> 8) & 0xfc) as u8;
        }
        let (common, notify, device) = (common?, notify?, device?);
        common.w8(0x14, 0); // reset
        let mut spins = 0;
        while common.r8(0x14) != 0 && spins < 1_000_000 {
            spins += 1;
            core::hint::spin_loop();
        }
        common.w8(0x14, 1); // ACKNOWLEDGE
        common.w8(0x14, 1 | 2); // DRIVER
        let feat = |sel: u32| {
            common.w32(0x00, sel);
            u64::from(common.r32(0x04))
        };
        let offered = feat(0) | feat(1) << 32;
        if offered & VIRTIO_F_VERSION_1 == 0 {
            return None;
        }
        let want = offered & (VIRTIO_F_VERSION_1 | VIRTIO_NET_F_MAC | VIRTIO_NET_F_STATUS);
        common.w32(0x08, 0);
        common.w32(0x0c, want as u32);
        common.w32(0x08, 1);
        common.w32(0x0c, (want >> 32) as u32);
        common.w8(0x14, 1 | 2 | 8); // FEATURES_OK
        if common.r8(0x14) & 8 == 0 {
            return None;
        }
        let mut rx = Queue::new(common, 0, notify, mult)?;
        let tx = Queue::new(common, 1, notify, mult)?;
        common.w8(0x14, 1 | 2 | 8 | 4); // DRIVER_OK
        let mut mac = [0x02, 0, 0, 0, 0, 1];
        if want & VIRTIO_NET_F_MAC != 0 {
            for (i, b) in mac.iter_mut().enumerate() {
                *b = device.r8(i as u64);
            }
        }
        // Give every receive buffer to the device.
        while let Some(id) = rx.free.pop() {
            rx.push(id, BUF as u32, true);
        }
        rx.kick(0);
        Some(VirtioNet {
            common,
            device,
            rx,
            tx,
            mac,
            status_feature: want & VIRTIO_NET_F_STATUS != 0,
        })
    }
}

impl pos_hal::Nic for VirtioNet {
    fn mac(&self) -> [u8; 6] {
        self.mac
    }
    fn link_up(&mut self) -> bool {
        let _ = self.common;
        !self.status_feature || self.device.r16(6) & 1 != 0
    }
    fn transmit(&mut self, frame: &[u8]) -> HalResult<()> {
        while let Some((id, _)) = self.tx.pop() {
            self.tx.free.push(id);
        }
        if frame.len() + VNET_HDR > BUF {
            return Err(HalError::InvalidArgument);
        }
        let id = self.tx.free.pop().ok_or(HalError::Timeout)?;
        let off = BUF * usize::from(id);
        copy_to(&self.tx.bufs, off, &[0u8; VNET_HDR]);
        copy_to(&self.tx.bufs, off + VNET_HDR, frame);
        self.tx.push(id, (VNET_HDR + frame.len()) as u32, false);
        self.tx.kick(1);
        Ok(())
    }
    fn receive(&mut self, buf: &mut [u8]) -> Option<usize> {
        let (id, len) = self.rx.pop()?;
        let len = (len as usize).saturating_sub(VNET_HDR).min(buf.len());
        copy_from(
            &self.rx.bufs,
            BUF * usize::from(id) + VNET_HDR,
            &mut buf[..len],
        );
        self.rx.push(id, BUF as u32, true);
        self.rx.kick(0);
        (len > 0).then_some(len)
    }
}

// ------------------------------------------------------------ e1000

const CTRL: u64 = 0x0000;
const STATUS: u64 = 0x0008;
const EERD: u64 = 0x0014;
const IMC: u64 = 0x00d8;
const RCTL: u64 = 0x0100;
const TCTL: u64 = 0x0400;
const TIPG: u64 = 0x0410;
const RDBAL: u64 = 0x2800;
const RDLEN: u64 = 0x2808;
const RDH: u64 = 0x2810;
const RDT: u64 = 0x2818;
const TDBAL: u64 = 0x3800;
const TDLEN: u64 = 0x3808;
const TDH: u64 = 0x3810;
const TDT: u64 = 0x3818;
const MTA: u64 = 0x5200;
const RAL: u64 = 0x5400;
const RAH: u64 = 0x5404;

/// PCI device ids driven with legacy descriptors.
const E1000_IDS: &[u16] = &[
    0x100e, // 82540EM (QEMU "e1000")
    0x100f, // 82545EM
    0x1004, 0x1026, 0x10d3, // 82574L (QEMU "e1000e")
    0x10f6, 0x150c,
];

pub struct E1000 {
    r: Mmio,
    rx_ring: Dma,
    tx_ring: Dma,
    rx_bufs: Dma,
    tx_bufs: Dma,
    rx_cur: usize,
    tx_cur: usize,
    mac: [u8; 6],
}

impl E1000 {
    fn init(f: &Function) -> Option<E1000> {
        f.enable_mmio_dma();
        let r = bar_mmio(f, 0)?;
        r.w32(IMC, 0xffff_ffff);
        r.w32(CTRL, r.r32(CTRL) | 1 << 26); // RST
        crate::clock::spin_ms(10);
        r.w32(IMC, 0xffff_ffff);
        // Set link up, auto-speed.
        r.w32(CTRL, (r.r32(CTRL) | 1 << 6 | 1 << 5) & !(1 << 3 | 1 << 31));
        let mut mac = [0u8; 6];
        let lo = r.r32(RAL);
        let hi = r.r32(RAH);
        if hi & (1 << 31) != 0 && (lo != 0 || hi & 0xffff != 0) {
            mac[..4].copy_from_slice(&lo.to_le_bytes());
            mac[4..].copy_from_slice(&(hi as u16).to_le_bytes());
        } else {
            for w in 0..3u32 {
                r.w32(EERD, 1 | w << 8);
                let mut v = 0;
                for _ in 0..100_000 {
                    v = r.r32(EERD);
                    if v & (1 << 4) != 0 {
                        break;
                    }
                }
                let word = (v >> 16) as u16;
                mac[2 * w as usize..2 * w as usize + 2].copy_from_slice(&word.to_le_bytes());
            }
            r.w32(RAL, u32::from_le_bytes([mac[0], mac[1], mac[2], mac[3]]));
            r.w32(
                RAH,
                u32::from(u16::from_le_bytes([mac[4], mac[5]])) | 1 << 31,
            );
        }
        for i in 0..128u64 {
            r.w32(MTA + 4 * i, 0);
        }
        let rx_ring = Dma::new(16 * RING, 4096)?;
        let tx_ring = Dma::new(16 * RING, 4096)?;
        let rx_bufs = Dma::new(BUF * RING, 4096)?;
        let tx_bufs = Dma::new(BUF * RING, 4096)?;
        for i in 0..RING {
            rx_ring.write64(16 * i, rx_bufs.phys() + (BUF * i) as u64);
            tx_ring.write64(16 * i, tx_bufs.phys() + (BUF * i) as u64);
            // Transmit descriptors start "done" (DD) so they are free.
            dma8w(&tx_ring, 16 * i + 12, 1);
        }
        r.w64(RDBAL, rx_ring.phys());
        r.w32(RDLEN, (16 * RING) as u32);
        r.w32(RDH, 0);
        r.w32(RDT, (RING - 1) as u32);
        // EN, broadcast accept, strip CRC, 2048-byte buffers.
        r.w32(RCTL, 1 << 1 | 1 << 15 | 1 << 26);
        r.w64(TDBAL, tx_ring.phys());
        r.w32(TDLEN, (16 * RING) as u32);
        r.w32(TDH, 0);
        r.w32(TDT, 0);
        r.w32(TIPG, 0x0060_200a);
        // EN, pad short packets, collision threshold/distance.
        r.w32(TCTL, 1 << 1 | 1 << 3 | 0x0f << 4 | 0x40 << 12);
        Some(E1000 {
            r,
            rx_ring,
            tx_ring,
            rx_bufs,
            tx_bufs,
            rx_cur: 0,
            tx_cur: 0,
            mac,
        })
    }
}

impl pos_hal::Nic for E1000 {
    fn mac(&self) -> [u8; 6] {
        self.mac
    }
    fn link_up(&mut self) -> bool {
        self.r.r32(STATUS) & 2 != 0
    }
    fn transmit(&mut self, frame: &[u8]) -> HalResult<()> {
        if frame.len() > BUF {
            return Err(HalError::InvalidArgument);
        }
        let d = 16 * self.tx_cur;
        fence(Ordering::SeqCst);
        if dma8(&self.tx_ring, d + 12) & 1 == 0 {
            return Err(HalError::Timeout); // ring full
        }
        copy_to(&self.tx_bufs, BUF * self.tx_cur, frame);
        dma16w(&self.tx_ring, d + 8, frame.len() as u16);
        dma8w(&self.tx_ring, d + 10, 0);
        dma8w(&self.tx_ring, d + 11, 1 | 2 | 8); // EOP, IFCS, RS
        dma8w(&self.tx_ring, d + 12, 0);
        fence(Ordering::SeqCst);
        self.tx_cur = (self.tx_cur + 1) % RING;
        self.r.w32(TDT, self.tx_cur as u32);
        Ok(())
    }
    fn receive(&mut self, buf: &mut [u8]) -> Option<usize> {
        let d = 16 * self.rx_cur;
        fence(Ordering::SeqCst);
        let status = dma8(&self.rx_ring, d + 12);
        if status & 1 == 0 {
            return None;
        }
        let len = usize::from(dma16(&self.rx_ring, d + 8)).min(buf.len());
        let errors = dma8(&self.rx_ring, d + 13);
        let eop = status & 2 != 0;
        if eop && errors == 0 {
            copy_from(&self.rx_bufs, BUF * self.rx_cur, &mut buf[..len]);
        }
        dma8w(&self.rx_ring, d + 12, 0);
        fence(Ordering::SeqCst);
        self.r.w32(RDT, self.rx_cur as u32);
        self.rx_cur = (self.rx_cur + 1) % RING;
        (eop && errors == 0 && len > 0).then_some(len)
    }
}

// ------------------------------------------------------------ probe

pub enum KernelNic {
    Virtio(VirtioNet),
    E1000(E1000),
}

impl KernelNic {
    pub fn probe() -> Option<KernelNic> {
        let fns = crate::pci::enumerate();
        for f in &fns {
            if f.vendor == 0x1af4
                && (f.device == 0x1041 || f.device == 0x1000)
                && let Some(n) = VirtioNet::init(f)
            {
                return Some(KernelNic::Virtio(n));
            }
        }
        for f in &fns {
            if f.vendor == 0x8086
                && E1000_IDS.contains(&f.device)
                && let Some(n) = E1000::init(f)
            {
                return Some(KernelNic::E1000(n));
            }
        }
        None
    }

    pub fn describe(&self) -> &'static str {
        match self {
            KernelNic::Virtio(_) => "virtio-net",
            KernelNic::E1000(_) => "Intel e1000",
        }
    }

    fn inner(&mut self) -> &mut dyn pos_hal::Nic {
        match self {
            KernelNic::Virtio(n) => n,
            KernelNic::E1000(n) => n,
        }
    }
}

impl pos_hal::Nic for KernelNic {
    fn mac(&self) -> [u8; 6] {
        match self {
            KernelNic::Virtio(n) => n.mac,
            KernelNic::E1000(n) => n.mac,
        }
    }
    fn link_up(&mut self) -> bool {
        self.inner().link_up()
    }
    fn transmit(&mut self, frame: &[u8]) -> HalResult<()> {
        self.inner().transmit(frame)
    }
    fn receive(&mut self, buf: &mut [u8]) -> Option<usize> {
        self.inner().receive(buf)
    }
}
