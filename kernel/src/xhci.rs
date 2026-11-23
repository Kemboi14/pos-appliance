//! xHCI host controller driver: reset and BIOS handoff, command and event
//! rings, slot/endpoint contexts, root-port reset, control, bulk and
//! interrupt transfers, endpoint error recovery. Polled (no MSI): the main
//! loop drains the event ring; synchronous calls spin on it with timeouts.
//!
//! USB 2.0 hubs are handled by `usb.rs` (route strings and TTs are set
//! here); SuperSpeed hubs are not supported (see docs/TIER_B.md).

use crate::clock::now_us;
use crate::mm::{self, Dma};
use crate::pci;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::{Ordering, fence};
use pos_drivers::usb::{Endpoint, Setup, TransferType};

const RING_TRBS: usize = 256;
const EVENT_TRBS: usize = 256;
/// Largest data stage / normal TRB we issue (one TRB, no 64 KiB crossing).
pub const MAX_XFER: usize = 64 * 1024;

// TRB types.
const TRB_NORMAL: u32 = 1;
const TRB_SETUP: u32 = 2;
const TRB_DATA: u32 = 3;
const TRB_STATUS: u32 = 4;
const TRB_LINK: u32 = 6;
const TRB_ENABLE_SLOT: u32 = 9;
const TRB_DISABLE_SLOT: u32 = 10;
const TRB_ADDRESS_DEVICE: u32 = 11;
const TRB_CONFIGURE_EP: u32 = 12;
const TRB_EVALUATE_CTX: u32 = 13;
const TRB_RESET_EP: u32 = 14;
const TRB_STOP_EP: u32 = 15;
const TRB_SET_DEQUEUE: u32 = 16;
pub const EV_TRANSFER: u8 = 32;
pub const EV_COMMAND: u8 = 33;
pub const EV_PORT: u8 = 34;

// Completion codes.
pub const CC_SUCCESS: u8 = 1;
pub const CC_STALL: u8 = 6;
pub const CC_SHORT: u8 = 13;

// PORTSC bits.
const PORT_CCS: u32 = 1 << 0;
const PORT_PED: u32 = 1 << 1;
const PORT_PR: u32 = 1 << 4;
const PORT_PP: u32 = 1 << 9;
const PORT_CHANGE_BITS: u32 = 0x7f << 17;
const PORT_PRC: u32 = 1 << 21;
/// RW bits to preserve when writing PORTSC (everything else is RO, RW1C
/// or has a side effect when written as 1).
const PORT_PRESERVE: u32 = 0x0e00_c3e0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XferError {
    Stall,
    Timeout,
    Failed(u8),
    NoResources,
}

#[derive(Debug, Clone, Copy)]
pub struct Event {
    pub kind: u8,
    pub cc: u8,
    pub slot: u8,
    pub ep: u8,
    pub ptr: u64,
    /// Residual (transfer events) or port ID (port events).
    pub param: u32,
}

#[derive(Debug)]
pub struct Ring {
    mem: Dma,
    idx: usize,
    cycle: bool,
}

impl Ring {
    fn new() -> Option<Ring> {
        Some(Ring {
            mem: Dma::new(RING_TRBS * 16, 4096)?,
            idx: 0,
            cycle: true,
        })
    }

    fn phys(&self) -> u64 {
        self.mem.phys()
    }

    /// Current enqueue pointer with the cycle state in bit 0 (for Set TR
    /// Dequeue and context initialisation).
    fn dequeue_ptr(&self) -> u64 {
        (self.mem.phys() + (self.idx * 16) as u64) | u64::from(self.cycle)
    }

    /// Enqueue one TRB; returns its physical address. The control dword is
    /// written last, after a fence, so the controller never sees a TRB with
    /// a valid cycle bit but stale fields.
    fn push(&mut self, param: u64, status: u32, control: u32) -> u64 {
        if self.idx == RING_TRBS - 1 {
            let off = self.idx * 16;
            self.mem.write64(off, self.mem.phys());
            self.mem.write32(off + 8, 0);
            fence(Ordering::SeqCst);
            // Link TRB with Toggle Cycle.
            self.mem
                .write32(off + 12, TRB_LINK << 10 | 1 << 1 | u32::from(self.cycle));
            self.cycle = !self.cycle;
            self.idx = 0;
        }
        let off = self.idx * 16;
        self.mem.write64(off, param);
        self.mem.write32(off + 8, status);
        fence(Ordering::SeqCst);
        self.mem
            .write32(off + 12, (control & !1) | u32::from(self.cycle));
        self.idx += 1;
        self.mem.phys() + off as u64
    }
}

#[derive(Debug)]
struct Slot {
    id: u8,
    out_ctx: Dma,
    rings: [Option<Ring>; 32],
}

#[derive(Debug)]
pub struct Controller {
    op: u64,
    rt: u64,
    db: u64,
    pub ports: u8,
    max_slots: u8,
    csz: usize,
    _dcbaa: Dma,
    dcbaa_virt: *mut u64,
    _scratch: Vec<Dma>,
    cmd: Ring,
    events: Dma,
    _erst: Dma,
    ev_idx: usize,
    ev_cycle: bool,
    slots: Vec<Slot>,
    pending: VecDeque<Event>,
    /// Control-transfer bounce buffer.
    ctrl_buf: Dma,
}

fn r32(a: u64) -> u32 {
    // SAFETY: `a` is inside the controller's mapped MMIO BAR.
    unsafe { (a as *const u32).read_volatile() }
}
fn w32(a: u64, v: u32) {
    // SAFETY: as above.
    unsafe { (a as *mut u32).write_volatile(v) }
}
fn w64(a: u64, v: u64) {
    w32(a, v as u32);
    w32(a + 4, (v >> 32) as u32);
}

fn wait_until(timeout_us: u64, mut f: impl FnMut() -> bool) -> bool {
    let end = now_us() + timeout_us;
    loop {
        if f() {
            return true;
        }
        if now_us() > end {
            return false;
        }
        core::hint::spin_loop();
    }
}

impl Controller {
    pub fn init(f: pci::Function) -> Option<Controller> {
        let (bar, size) = f.memory_bar(0)?;
        f.enable_mmio_dma();
        let mmio = mm::map_mmio(bar, size.min(1 << 20))?;
        let caplen = u64::from(r32(mmio) & 0xff);
        let hcs1 = r32(mmio + 0x04);
        let hcs2 = r32(mmio + 0x08);
        let hcc1 = r32(mmio + 0x10);
        let db = mmio + u64::from(r32(mmio + 0x14) & !0x3);
        let rt = mmio + u64::from(r32(mmio + 0x18) & !0x1f);
        let op = mmio + caplen;

        // BIOS -> OS handoff (USB Legacy Support capability).
        let mut xecp = u64::from(hcc1 >> 16) * 4;
        while xecp != 0 {
            let a = mmio + xecp;
            let v = r32(a);
            if v & 0xff == 1 {
                w32(a, v | 1 << 24);
                wait_until(1_000_000, || r32(a) & (1 << 16) == 0);
                // Disable SMIs on USB events.
                w32(a + 4, r32(a + 4) & 0xffff_1fee);
            }
            let next = u64::from((v >> 8) & 0xff) * 4;
            xecp = if next == 0 { 0 } else { xecp + next };
        }

        // Stop, then reset.
        w32(op, r32(op) & !1);
        if !wait_until(100_000, || r32(op + 4) & 1 != 0) {
            return None;
        }
        w32(op, 1 << 1);
        if !wait_until(1_000_000, || {
            r32(op) & (1 << 1) == 0 && r32(op + 4) & (1 << 11) == 0
        }) {
            return None;
        }

        let max_slots = (hcs1 & 0xff).min(32) as u8;
        let ports = ((hcs1 >> 24) & 0xff) as u8;
        let csz = if hcc1 & (1 << 2) != 0 { 64 } else { 32 };
        w32(op + 0x38, u32::from(max_slots));

        let dcbaa = Dma::new(256 * 8, 4096)?;
        let dcbaa_virt = dcbaa.virt().cast::<u64>();
        let nscratch = (((hcs2 >> 21) & 0x1f) << 5 | (hcs2 >> 27)) as usize;
        let mut scratch = Vec::new();
        if nscratch > 0 {
            let arr = Dma::new(nscratch * 8, 4096)?;
            for i in 0..nscratch {
                let page = Dma::new(4096, 4096)?;
                arr.write64(i * 8, page.phys());
                scratch.push(page);
            }
            dcbaa.write64(0, arr.phys());
            scratch.push(arr);
        }
        w64(op + 0x30, dcbaa.phys());

        let cmd = Ring::new()?;
        w64(op + 0x18, cmd.phys() | 1);

        let events = Dma::new(EVENT_TRBS * 16, 4096)?;
        let erst = Dma::new(64, 64)?;
        erst.write64(0, events.phys());
        erst.write32(8, EVENT_TRBS as u32);
        let ir0 = rt + 0x20;
        w32(ir0 + 0x08, 1); // ERSTSZ
        w64(ir0 + 0x18, events.phys()); // ERDP
        w64(ir0 + 0x10, erst.phys()); // ERSTBA (last)

        w32(op, 1); // run, no interrupts (polled)
        if !wait_until(100_000, || r32(op + 4) & 1 == 0) {
            return None;
        }

        let c = Controller {
            op,
            rt,
            db,
            ports,
            max_slots,
            csz,
            _dcbaa: dcbaa,
            dcbaa_virt,
            _scratch: scratch,
            cmd,
            events,
            _erst: erst,
            ev_idx: 0,
            ev_cycle: true,
            slots: Vec::new(),
            pending: VecDeque::new(),
            ctrl_buf: Dma::new(4096, 4096)?,
        };
        // Power ports that need it (PPC), then give devices time to connect.
        let mut powered = false;
        for p in 1..=c.ports {
            let v = c.portsc(p);
            if v & PORT_PP == 0 {
                c.write_portsc(p, (v & PORT_PRESERVE) | PORT_PP);
                powered = true;
            }
        }
        if powered {
            crate::clock::spin_ms(20);
        }
        Some(c)
    }

    fn portsc_addr(&self, port: u8) -> u64 {
        self.op + 0x400 + 0x10 * u64::from(port - 1)
    }

    pub fn portsc(&self, port: u8) -> u32 {
        r32(self.portsc_addr(port))
    }

    fn write_portsc(&self, port: u8, v: u32) {
        w32(self.portsc_addr(port), v);
    }

    pub fn connected(&self, port: u8) -> bool {
        self.portsc(port) & PORT_CCS != 0
    }

    /// Acknowledge all change bits on a port.
    pub fn clear_port_changes(&self, port: u8) {
        let v = self.portsc(port);
        self.write_portsc(port, (v & PORT_PRESERVE) | (v & PORT_CHANGE_BITS));
    }

    /// Reset a connected port; returns its speed ID when enabled.
    pub fn reset_port(&self, port: u8) -> Option<u8> {
        let v = self.portsc(port);
        if v & PORT_CCS == 0 {
            return None;
        }
        self.write_portsc(port, (v & PORT_PRESERVE) | PORT_PR);
        let ok = wait_until(500_000, || self.portsc(port) & PORT_PRC != 0);
        self.clear_port_changes(port);
        let v = self.portsc(port);
        if !ok || v & PORT_PED == 0 {
            return None;
        }
        crate::clock::spin_ms(10); // reset recovery (USB 2.0 TRSTRCY)
        Some(((v >> 10) & 0xf) as u8)
    }

    fn doorbell(&self, slot: u8, target: u32) {
        fence(Ordering::SeqCst);
        w32(self.db + 4 * u64::from(slot), target);
    }

    /// Move completed events from the ring into `pending`.
    pub fn poll(&mut self) {
        let mut any = false;
        loop {
            let off = self.ev_idx * 16;
            let ctl = self.events.read32(off + 12);
            if (ctl & 1 != 0) != self.ev_cycle {
                break;
            }
            let ptr =
                u64::from(self.events.read32(off)) | u64::from(self.events.read32(off + 4)) << 32;
            let status = self.events.read32(off + 8);
            let kind = ((ctl >> 10) & 0x3f) as u8;
            self.pending.push_back(Event {
                kind,
                cc: (status >> 24) as u8,
                slot: (ctl >> 24) as u8,
                ep: ((ctl >> 16) & 0x1f) as u8,
                ptr,
                param: if kind == EV_PORT {
                    (ptr >> 24) as u32 & 0xff
                } else {
                    status & 0xff_ffff
                },
            });
            self.ev_idx += 1;
            if self.ev_idx == EVENT_TRBS {
                self.ev_idx = 0;
                self.ev_cycle = !self.ev_cycle;
            }
            any = true;
        }
        if any {
            // Update ERDP and clear Event Handler Busy.
            w64(
                self.rt + 0x20 + 0x18,
                (self.events.phys() + (self.ev_idx * 16) as u64) | 1 << 3,
            );
        }
        // Bound memory if nobody consumes some events (e.g. unbound slots).
        while self.pending.len() > 512 {
            self.pending.pop_front();
        }
    }

    fn take(&mut self, pred: impl Fn(&Event) -> bool) -> Option<Event> {
        let i = self.pending.iter().position(pred)?;
        self.pending.remove(i)
    }

    fn wait_event(&mut self, timeout_us: u64, pred: impl Fn(&Event) -> bool) -> Option<Event> {
        let end = now_us() + timeout_us;
        loop {
            self.poll();
            if let Some(e) = self.take(&pred) {
                return Some(e);
            }
            if now_us() > end {
                return None;
            }
            core::hint::spin_loop();
        }
    }

    /// Next port-status-change event, if any.
    pub fn take_port_event(&mut self) -> Option<u8> {
        self.poll();
        self.take(|e| e.kind == EV_PORT).map(|e| e.param as u8)
    }

    /// Completed transfer on an endpoint, if any (interrupt endpoints).
    pub fn take_transfer(&mut self, slot: u8, ep: u8) -> Option<Event> {
        self.take(|e| e.kind == EV_TRANSFER && e.slot == slot && e.ep == ep)
    }

    fn command(&mut self, param: u64, status: u32, control: u32) -> Result<Event, XferError> {
        let trb = self.cmd.push(param, status, control);
        self.doorbell(0, 0);
        let ev = self
            .wait_event(1_000_000, |e| e.kind == EV_COMMAND && e.ptr == trb)
            .ok_or(XferError::Timeout)?;
        if ev.cc == CC_SUCCESS {
            Ok(ev)
        } else {
            Err(XferError::Failed(ev.cc))
        }
    }

    fn slot(&mut self, id: u8) -> Option<&mut Slot> {
        self.slots.iter_mut().find(|s| s.id == id)
    }

    fn input_ctx(&self) -> Option<Dma> {
        Dma::new(33 * self.csz, 4096)
    }

    /// Enable Slot + Address Device for a freshly reset device.
    /// `route` is the hub route string (0 on a root port); `tt` is the
    /// (hub slot, hub port) transaction translator for a low/full-speed
    /// device behind a high-speed hub.
    pub fn address_device(
        &mut self,
        root_port: u8,
        route: u32,
        speed: u8,
        tt: Option<(u8, u8)>,
    ) -> Result<u8, XferError> {
        let ev = self.command(0, 0, TRB_ENABLE_SLOT << 10)?;
        let id = ev.slot;
        if id == 0 || id > self.max_slots {
            return Err(XferError::NoResources);
        }
        let out_ctx = Dma::new(32 * self.csz, 4096).ok_or(XferError::NoResources)?;
        let ring = Ring::new().ok_or(XferError::NoResources)?;
        let input = self.input_ctx().ok_or(XferError::NoResources)?;
        let csz = self.csz;
        input.write32(4, 0b11); // add slot + EP0
        input.write32(csz, (route & 0xf_ffff) | u32::from(speed) << 20 | 1 << 27);
        input.write32(csz + 4, u32::from(root_port) << 16);
        if let Some((hub_slot, hub_port)) = tt {
            input.write32(csz + 8, u32::from(hub_slot) | u32::from(hub_port) << 8);
        }
        let mps0: u32 = match speed {
            1 | 2 => 8, // full/low speed: refined from the device descriptor
            3 => 64,
            _ => 512,
        };
        let ep0 = 2 * csz;
        input.write32(ep0 + 4, 3 << 1 | 4 << 3 | mps0 << 16);
        input.write64(ep0 + 8, ring.dequeue_ptr());
        input.write32(ep0 + 16, 8);
        // SAFETY: id <= max_slots < 256 entries in the DCBAA.
        unsafe {
            self.dcbaa_virt
                .add(usize::from(id))
                .write_volatile(out_ctx.phys())
        };
        let mut rings: [Option<Ring>; 32] = [const { None }; 32];
        rings[1] = Some(ring);
        self.slots.push(Slot { id, out_ctx, rings });
        if let Err(e) = self.command(
            input.phys(),
            0,
            TRB_ADDRESS_DEVICE << 10 | u32::from(id) << 24,
        ) {
            self.disable_slot(id);
            return Err(e);
        }
        crate::clock::spin_ms(2); // SET_ADDRESS recovery
        Ok(id)
    }

    /// Update EP0's max packet size (full-speed devices).
    pub fn set_ep0_mps(&mut self, id: u8, mps: u16) -> Result<(), XferError> {
        let input = self.input_ctx().ok_or(XferError::NoResources)?;
        input.write32(4, 0b10);
        let ep0 = 2 * self.csz;
        input.write32(ep0 + 4, 3 << 1 | 4 << 3 | u32::from(mps) << 16);
        self.command(
            input.phys(),
            0,
            TRB_EVALUATE_CTX << 10 | u32::from(id) << 24,
        )
        .map(|_| ())
    }

    pub fn disable_slot(&mut self, id: u8) {
        let _ = self.command(0, 0, TRB_DISABLE_SLOT << 10 | u32::from(id) << 24);
        // SAFETY: as in `address_device`.
        unsafe { self.dcbaa_virt.add(usize::from(id)).write_volatile(0) };
        self.slots.retain(|s| s.id != id);
        self.pending.retain(|e| e.slot != id || e.kind == EV_PORT);
    }

    /// Add `eps` to a configured device (one Configure Endpoint command).
    /// `hub` = (ports, TT think time) marks the device as a hub.
    pub fn configure_endpoints(
        &mut self,
        id: u8,
        eps: &[Endpoint],
        hub: Option<(u8, u8)>,
    ) -> Result<(), XferError> {
        let csz = self.csz;
        let input = self.input_ctx().ok_or(XferError::NoResources)?;
        // Start from the current slot context (route, speed, root port, TT).
        let mut slot_ctx = [0u32; 4];
        {
            let s = self.slot(id).ok_or(XferError::NoResources)?;
            for (k, v) in slot_ctx.iter_mut().enumerate() {
                *v = s.out_ctx.read32(k * 4);
            }
        }
        let speed = ((slot_ctx[0] >> 20) & 0xf) as u8;
        let mut add = 1u32;
        let mut max_dci = 1u8;
        for ep in eps {
            let dci = ep.dci();
            if !(2..32).contains(&dci) {
                continue;
            }
            add |= 1 << dci;
            max_dci = max_dci.max(dci);
            let ring = Ring::new().ok_or(XferError::NoResources)?;
            let kind: u32 = match (ep.kind, ep.is_in()) {
                (TransferType::Bulk, false) => 2,
                (TransferType::Interrupt, false) => 3,
                (TransferType::Bulk, true) => 6,
                (TransferType::Interrupt, true) => 7,
                _ => continue,
            };
            let interval: u32 = match (ep.kind, speed) {
                (TransferType::Interrupt, 1 | 2) => {
                    // bInterval in ms -> 2^n * 125 us.
                    let frames = u32::from(ep.interval.max(1)) * 8;
                    (31 - frames.leading_zeros()).clamp(3, 10)
                }
                (TransferType::Interrupt, _) => u32::from(ep.interval.clamp(1, 16)) - 1,
                _ => 0,
            };
            let mps = u32::from(ep.max_packet.max(1));
            let base = (usize::from(dci) + 1) * csz;
            input.write32(base, interval << 16);
            input.write32(base + 4, 3 << 1 | kind << 3 | mps << 16);
            input.write64(base + 8, ring.dequeue_ptr());
            let avg = if ep.kind == TransferType::Interrupt {
                mps
            } else {
                1024
            };
            input.write32(
                base + 16,
                avg | (if ep.kind == TransferType::Interrupt {
                    mps << 16
                } else {
                    0
                }),
            );
            self.slot(id).ok_or(XferError::NoResources)?.rings[usize::from(dci)] = Some(ring);
        }
        input.write32(4, add);
        let mut dw0 = (slot_ctx[0] & 0x07ff_ffff) | u32::from(max_dci) << 27;
        let mut dw1 = slot_ctx[1] & 0x00ff_ffff;
        let mut dw2 = slot_ctx[2] & 0xffff;
        if let Some((ports, ttt)) = hub {
            dw0 |= 1 << 26;
            dw1 |= u32::from(ports) << 24;
            dw2 |= u32::from(ttt & 3) << 16;
        }
        input.write32(csz, dw0);
        input.write32(csz + 4, dw1);
        input.write32(csz + 8, dw2);
        self.command(
            input.phys(),
            0,
            TRB_CONFIGURE_EP << 10 | u32::from(id) << 24,
        )
        .map(|_| ())
    }

    /// Control transfer on EP0. `data` is the data stage buffer (IN: filled,
    /// OUT: sent), at most 4 KiB. Returns bytes transferred.
    pub fn control(&mut self, id: u8, setup: Setup, data: &mut [u8]) -> Result<usize, XferError> {
        let len = usize::from(setup.length)
            .min(data.len())
            .min(self.ctrl_buf.len());
        let is_in = setup.is_in();
        if !is_in && len > 0 {
            self.ctrl_buf.copy_in(&data[..len]);
        }
        let buf = self.ctrl_buf.phys();
        let ring = self
            .slot(id)
            .and_then(|s| s.rings[1].as_mut())
            .ok_or(XferError::NoResources)?;
        let trt = match (len, is_in) {
            (0, _) => 0,
            (_, false) => 2,
            (_, true) => 3,
        };
        ring.push(setup.to_u64(), 8, TRB_SETUP << 10 | 1 << 6 | trt << 16);
        if len > 0 {
            ring.push(buf, len as u32, TRB_DATA << 10 | u32::from(is_in) << 16);
        }
        let status_in = len == 0 || !is_in;
        let trb = ring.push(0, 0, TRB_STATUS << 10 | 1 << 5 | u32::from(status_in) << 16);
        self.doorbell(id, 1);
        let ev = self.wait_event(1_000_000, |e| e.kind == EV_TRANSFER && e.ptr == trb);
        match ev {
            Some(e) if e.cc == CC_SUCCESS || e.cc == CC_SHORT => {
                if is_in && len > 0 {
                    self.ctrl_buf.copy_out(&mut data[..len]);
                }
                Ok(len)
            }
            Some(e) => {
                self.recover(id, 1);
                Err(if e.cc == CC_STALL {
                    XferError::Stall
                } else {
                    XferError::Failed(e.cc)
                })
            }
            None => {
                self.recover(id, 1);
                Err(XferError::Timeout)
            }
        }
    }

    /// Queue a Normal TRB without waiting (interrupt IN polling). Returns
    /// the TRB address.
    pub fn queue_normal(
        &mut self,
        id: u8,
        dci: u8,
        buf: &Dma,
        len: usize,
    ) -> Result<u64, XferError> {
        let ring = self
            .slot(id)
            .and_then(|s| s.rings.get_mut(usize::from(dci))?.as_mut())
            .ok_or(XferError::NoResources)?;
        let trb = ring.push(
            buf.phys(),
            len.min(MAX_XFER) as u32,
            TRB_NORMAL << 10 | 1 << 5 | 1 << 2,
        );
        self.doorbell(id, u32::from(dci));
        Ok(trb)
    }

    /// Synchronous bulk transfer of `len` bytes using `buf` (DMA,
    /// <= 64 KiB, not crossing 64 KiB). Returns bytes transferred.
    pub fn bulk(
        &mut self,
        id: u8,
        dci: u8,
        buf: &Dma,
        len: usize,
        timeout_us: u64,
    ) -> Result<usize, XferError> {
        let len = len.min(MAX_XFER).min(buf.len());
        let trb = self.queue_normal(id, dci, buf, len)?;
        match self.wait_event(timeout_us, |e| e.kind == EV_TRANSFER && e.ptr == trb) {
            Some(e) if e.cc == CC_SUCCESS || e.cc == CC_SHORT => {
                Ok(len.saturating_sub(e.param as usize))
            }
            Some(e) if e.cc == CC_STALL => {
                self.recover(id, dci);
                Err(XferError::Stall)
            }
            Some(e) => {
                self.recover(id, dci);
                Err(XferError::Failed(e.cc))
            }
            None => {
                self.recover(id, dci);
                Err(XferError::Timeout)
            }
        }
    }

    /// Bring an endpoint back to Running-ready after a stall, error or
    /// timeout: Reset Endpoint (halted) or Stop Endpoint (running), then
    /// point its dequeue pointer past everything queued so far.
    pub fn recover(&mut self, id: u8, dci: u8) {
        let csz = self.csz;
        let Some(s) = self.slot(id) else { return };
        let state = s.out_ctx.read32(usize::from(dci) * csz) & 0x7;
        let Some(deq) = s.rings[usize::from(dci)].as_ref().map(Ring::dequeue_ptr) else {
            return;
        };
        let target = u32::from(id) << 24 | u32::from(dci) << 16;
        match state {
            2 => {
                let _ = self.command(0, 0, TRB_RESET_EP << 10 | target);
            }
            1 => {
                let _ = self.command(0, 0, TRB_STOP_EP << 10 | target);
            }
            _ => {}
        }
        let _ = self.command(deq, 0, TRB_SET_DEQUEUE << 10 | target);
        self.pending
            .retain(|e| !(e.kind == EV_TRANSFER && e.slot == id && e.ep == dci));
    }
}
