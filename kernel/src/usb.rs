//! USB device layer over xHCI: enumeration and hot-plug on root ports, and
//! the class drivers the POS needs: HID boot keyboard (and keyboard-wedge
//! scanners), HID absolute pointer (touch screen / tablet), Mass Storage
//! BOT/SCSI (boot stick data partition and export stick), Printer class.
//!
//! Everything sits behind one `Rc<RefCell<Stack>>`; the HAL objects at the
//! bottom of this file are thin handles. Interrupts never touch the stack.

use crate::klog;
use crate::mm::Dma;
use crate::xhci::{Controller, MAX_XFER, XferError};
use alloc::collections::VecDeque;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use pos_drivers::hid::{BootKeyboard, PointerLayout};
use pos_drivers::part::{self, Slice};
use pos_drivers::scsi::{self, Command, CswStatus, Dir};
use pos_drivers::usb::{self as desc, Endpoint, Setup, TransferType};
use pos_hal::{BlockDevice, HalError, HalResult, InputEvent, PrinterStatus};

const BULK_TIMEOUT_US: u64 = 10_000_000;

#[derive(Debug)]
struct Keyboard {
    dci: u8,
    buf: Dma,
    kb: BootKeyboard,
}

#[derive(Debug)]
struct Pointer {
    dci: u8,
    buf: Dma,
    len: usize,
    layout: PointerLayout,
    last: Option<(u32, u32, bool)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Holds the POS data partition and is the disk Limine booted from.
    Boot,
    Other,
}

#[derive(Debug)]
pub struct Msc {
    iface: u8,
    in_ep: Endpoint,
    out_ep: Endpoint,
    tag: u32,
    cbw: Dma,
    csw: Dma,
    data: Dma,
    pub block_size: usize,
    pub blocks: u64,
    pub role: Role,
    sync_unsupported: bool,
}

#[derive(Debug)]
struct UsbPrinter {
    iface: u8,
    out_dci: u8,
    buf: Dma,
}

#[derive(Debug)]
struct Hub {
    dci: u8,
    buf: Dma,
    ports: u8,
}

#[derive(Debug)]
enum Func {
    Keyboard(Keyboard),
    Pointer(Pointer),
    Msc(Msc),
    Printer(UsbPrinter),
    Hub(Hub),
}

/// Where a device sits: root port, hub route string (4 bits per tier),
/// tier depth (0 = on a root port), speed ID, and transaction translator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Topo {
    root_port: u8,
    route: u32,
    depth: u8,
    speed: u8,
    tt: Option<(u8, u8)>,
}

impl Topo {
    /// Whether `other` is this position or below it.
    fn contains(&self, other: &Topo) -> bool {
        let mask = (1u32 << (4 * u32::from(self.depth))) - 1;
        other.root_port == self.root_port
            && other.depth >= self.depth
            && other.route & mask == self.route
    }
}

impl core::fmt::Display for Topo {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.root_port)?;
        for t in 0..self.depth {
            write!(f, ".{}", (self.route >> (4 * u32::from(t))) & 0xf)?;
        }
        Ok(())
    }
}

/// USB 2.0 hub class requests (recipient: port).
const HUB_SET_FEATURE: u8 = 3;
const HUB_CLEAR_FEATURE: u8 = 1;
const PORT_RESET: u16 = 4;
const PORT_POWER: u16 = 8;
const C_PORT_CONNECTION: u16 = 16;
const C_PORT_ENABLE: u16 = 17;
const C_PORT_RESET: u16 = 20;
/// Route strings have 5 tiers.
const MAX_DEPTH: u8 = 5;

fn hub_port_req(request: u8, feature: u16, port: u8) -> Setup {
    Setup {
        request_type: 0x23,
        request,
        value: feature,
        index: u16::from(port),
        length: 0,
    }
}

fn hub_port_status(port: u8) -> Setup {
    Setup {
        request_type: 0xa3,
        request: 0,
        value: 0,
        index: u16::from(port),
        length: 4,
    }
}

#[derive(Debug)]
struct Device {
    ctrl: usize,
    slot: u8,
    topo: Topo,
    /// Generation number so HAL handles notice a replug.
    id: u64,
    vid: u16,
    pid: u16,
    funcs: Vec<Func>,
}

#[derive(Debug)]
pub struct Stack {
    ctrls: Vec<Controller>,
    devices: Vec<Device>,
    events: VecDeque<InputEvent>,
    width: u32,
    height: u32,
    boot_disk: Option<[u8; 16]>,
    next_id: u64,
    /// Positions whose device failed to enumerate; retried after unplug.
    failed: Vec<(usize, Topo)>,
}

pub type Shared = Rc<RefCell<Stack>>;

fn bad(e: XferError) -> HalError {
    match e {
        XferError::Timeout => HalError::Timeout,
        _ => HalError::Io,
    }
}

impl Stack {
    /// Bring up every xHCI controller and enumerate attached devices.
    pub fn init(width: u32, height: u32, boot_disk: Option<[u8; 16]>) -> Shared {
        let mut ctrls = Vec::new();
        for f in crate::pci::enumerate() {
            if (f.class, f.subclass, f.prog_if) == (0x0c, 0x03, 0x30) {
                match Controller::init(f) {
                    Some(c) => {
                        klog!(
                            "xhci {:02x}:{:02x}.{} {} ports",
                            f.bus,
                            f.dev,
                            f.func,
                            c.ports
                        );
                        ctrls.push(c);
                    }
                    None => klog!(
                        "xhci {:02x}:{:02x}.{} failed to start",
                        f.bus,
                        f.dev,
                        f.func
                    ),
                }
            }
        }
        let mut s = Stack {
            ctrls,
            devices: Vec::new(),
            events: VecDeque::new(),
            width,
            height,
            boot_disk,
            next_id: 1,
            failed: Vec::new(),
        };
        for c in 0..s.ctrls.len() {
            for p in 1..=s.ctrls[c].ports {
                if s.ctrls[c].connected(p) {
                    s.attach_root(c, p);
                }
            }
        }
        Rc::new(RefCell::new(s))
    }

    pub fn controllers(&self) -> usize {
        self.ctrls.len()
    }

    fn attach_root(&mut self, c: usize, port: u8) {
        let ctrl = &mut self.ctrls[c];
        ctrl.clear_port_changes(port);
        let topo = Topo {
            root_port: port,
            route: 0,
            depth: 0,
            speed: 0,
            tt: None,
        };
        match ctrl.reset_port(port) {
            Some(speed) => self.enumerate(c, Topo { speed, ..topo }),
            None => self.failed.push((c, topo)),
        }
    }

    fn is_failed(&self, c: usize, t: &Topo) -> bool {
        self.failed.iter().any(|(fc, ft)| {
            *fc == c && ft.root_port == t.root_port && ft.route == t.route && ft.depth == t.depth
        })
    }

    fn enumerate(&mut self, c: usize, topo: Topo) {
        let slot =
            match self.ctrls[c].address_device(topo.root_port, topo.route, topo.speed, topo.tt) {
                Ok(s) => s,
                Err(e) => {
                    klog!("usb {topo}: address failed {e:?}");
                    self.failed.push((c, topo));
                    return;
                }
            };
        match self.configure(c, slot, topo) {
            Ok(dev) => {
                klog!(
                    "usb {topo}: {:04x}:{:04x} slot {slot}, {} function(s)",
                    dev.vid,
                    dev.pid,
                    dev.funcs.len()
                );
                let hub = dev.funcs.iter().find_map(|f| match f {
                    Func::Hub(h) => Some(h.ports),
                    _ => None,
                });
                let id = dev.id;
                self.devices.push(dev);
                if let Some(ports) = hub {
                    for p in 1..=ports {
                        self.hub_port_changed(id, p);
                    }
                }
            }
            Err(e) => {
                klog!("usb {topo}: setup failed {e:?}");
                self.ctrls[c].disable_slot(slot);
                self.failed.push((c, topo));
            }
        }
    }

    /// Re-examine one downstream port of a hub: acknowledge changes, then
    /// attach a new device or remove a departed one.
    fn hub_port_changed(&mut self, hub_id: u64, port: u8) {
        let Some(hub) = self.devices.iter().find(|d| d.id == hub_id) else {
            return;
        };
        let (c, slot, ht) = (hub.ctrl, hub.slot, hub.topo);
        if ht.depth >= MAX_DEPTH || port > 15 {
            return;
        }
        let child = Topo {
            root_port: ht.root_port,
            route: ht.route | u32::from(port) << (4 * u32::from(ht.depth)),
            depth: ht.depth + 1,
            speed: 0,
            tt: None,
        };
        let ctrl = &mut self.ctrls[c];
        let mut st = [0u8; 4];
        if ctrl.control(slot, hub_port_status(port), &mut st).is_err() {
            return;
        }
        let status = u16::from_le_bytes([st[0], st[1]]);
        for f in [C_PORT_CONNECTION, C_PORT_ENABLE] {
            let _ = ctrl.control(slot, hub_port_req(HUB_CLEAR_FEATURE, f, port), &mut []);
        }
        let connected = status & 1 != 0;
        let present = self.devices.iter().any(|d| {
            d.ctrl == c
                && d.topo.root_port == child.root_port
                && d.topo.route == child.route
                && d.topo.depth == child.depth
        });
        if !connected {
            self.failed
                .retain(|(fc, ft)| !(*fc == c && child.contains(ft)));
            if present {
                self.detach_tree(c, child);
            }
            return;
        }
        if present || self.is_failed(c, &child) {
            return;
        }
        // Reset the port and wait for C_PORT_RESET.
        let ctrl = &mut self.ctrls[c];
        if ctrl
            .control(
                slot,
                hub_port_req(HUB_SET_FEATURE, PORT_RESET, port),
                &mut [],
            )
            .is_err()
        {
            return;
        }
        let mut status = 0u16;
        let mut done = false;
        for _ in 0..50 {
            crate::clock::spin_ms(10);
            if ctrl.control(slot, hub_port_status(port), &mut st).is_err() {
                break;
            }
            status = u16::from_le_bytes([st[0], st[1]]);
            let change = u16::from_le_bytes([st[2], st[3]]);
            if change & (1 << 4) != 0 {
                done = true;
                break;
            }
        }
        let _ = ctrl.control(
            slot,
            hub_port_req(HUB_CLEAR_FEATURE, C_PORT_RESET, port),
            &mut [],
        );
        if !done || status & 0b11 != 0b11 {
            self.failed.push((c, child));
            return;
        }
        crate::clock::spin_ms(10); // reset recovery
        let speed = if status & (1 << 9) != 0 {
            2 // low
        } else if status & (1 << 10) != 0 {
            3 // high
        } else {
            1 // full
        };
        // LS/FS behind a HS hub use that hub's TT; otherwise inherit.
        let tt = if ht.speed == 3 && speed <= 2 {
            Some((slot, port))
        } else {
            ht.tt
        };
        self.enumerate(c, Topo { speed, tt, ..child });
    }

    fn configure(&mut self, c: usize, slot: u8, topo: Topo) -> Result<Device, XferError> {
        let speed = topo.speed;
        let ctrl = &mut self.ctrls[c];
        let mut d8 = [0u8; 8];
        ctrl.control(
            slot,
            Setup::get_descriptor(desc::DESC_DEVICE, 0, 8),
            &mut d8,
        )?;
        if speed <= 2 && d8[7] != 8 && d8[7] != 0 {
            ctrl.set_ep0_mps(slot, u16::from(d8[7]))?;
        }
        let mut d18 = [0u8; 18];
        ctrl.control(
            slot,
            Setup::get_descriptor(desc::DESC_DEVICE, 0, 18),
            &mut d18,
        )?;
        let dd = desc::parse_device(&d18).ok_or(XferError::Failed(0))?;
        let mut c9 = [0u8; 9];
        ctrl.control(
            slot,
            Setup::get_descriptor(desc::DESC_CONFIG, 0, 9),
            &mut c9,
        )?;
        let total = u16::from_le_bytes([c9[2], c9[3]]).clamp(9, 4096);
        let mut cfg_raw = vec![0u8; usize::from(total)];
        ctrl.control(
            slot,
            Setup::get_descriptor(desc::DESC_CONFIG, 0, total),
            &mut cfg_raw,
        )?;
        let cfg = desc::parse_config(&cfg_raw).ok_or(XferError::Failed(0))?;
        let is_hub = dd.class == desc::CLASS_HUB
            || cfg.interfaces.iter().any(|i| i.class == desc::CLASS_HUB);
        if is_hub && (speed >= 4 || topo.depth + 1 >= MAX_DEPTH) {
            klog!("usb {topo}: SuperSpeed or too deeply nested hub not supported");
            return Err(XferError::NoResources);
        }
        ctrl.control(slot, Setup::set_configuration(cfg.value), &mut [])?;

        let mut funcs = Vec::new();
        let mut eps = Vec::new();
        let mut hub_info = None;
        for i in &cfg.interfaces {
            let int_in = i
                .endpoints
                .iter()
                .find(|e| e.kind == TransferType::Interrupt && e.is_in())
                .copied();
            let bulk_in = i
                .endpoints
                .iter()
                .find(|e| e.kind == TransferType::Bulk && e.is_in())
                .copied();
            let bulk_out = i
                .endpoints
                .iter()
                .find(|e| e.kind == TransferType::Bulk && !e.is_in())
                .copied();
            match (i.class, i.subclass, i.protocol) {
                (desc::CLASS_HUB, _, _) => {
                    let Some(ep) = int_in else { continue };
                    let mut hd = [0u8; 9];
                    let get = Setup {
                        request_type: 0xa0,
                        request: 6,
                        value: 0x29 << 8,
                        index: 0,
                        length: 9,
                    };
                    ctrl.control(slot, get, &mut hd)?;
                    let ports = hd[2].min(15);
                    let ttt = (u16::from_le_bytes([hd[3], hd[4]]) >> 5) as u8 & 3;
                    let pwr_ms = u64::from(hd[5]) * 2;
                    hub_info = Some((ports, ttt, pwr_ms));
                    eps.push(ep);
                    funcs.push(Func::Hub(Hub {
                        dci: ep.dci(),
                        buf: Dma::new(64, 64).ok_or(XferError::NoResources)?,
                        ports,
                    }));
                }
                (desc::CLASS_HID, 1, 1) => {
                    let Some(ep) = int_in else { continue };
                    let _ = ctrl.control(
                        slot,
                        Setup::class_iface(true, 0x0b, 0, i.number, 0),
                        &mut [],
                    );
                    let _ = ctrl.control(
                        slot,
                        Setup::class_iface(true, 0x0a, 0, i.number, 0),
                        &mut [],
                    );
                    eps.push(ep);
                    funcs.push(Func::Keyboard(Keyboard {
                        dci: ep.dci(),
                        buf: Dma::new(64, 64).ok_or(XferError::NoResources)?,
                        kb: BootKeyboard::default(),
                    }));
                }
                (desc::CLASS_HID, _, _) => {
                    let (Some(ep), Some(rlen)) = (int_in, i.hid_report_len) else {
                        continue;
                    };
                    let mut rd = vec![0u8; usize::from(rlen.min(4096))];
                    let setup = Setup::get_interface_descriptor(
                        desc::DESC_HID_REPORT,
                        i.number,
                        rlen.min(4096),
                    );
                    if ctrl.control(slot, setup, &mut rd).is_err() {
                        continue;
                    }
                    let Some(layout) = pos_drivers::hid::parse_pointer(&rd) else {
                        continue;
                    };
                    let _ = ctrl.control(
                        slot,
                        Setup::class_iface(true, 0x0a, 0, i.number, 0),
                        &mut [],
                    );
                    let len = usize::from(ep.max_packet.clamp(1, 1024));
                    eps.push(ep);
                    funcs.push(Func::Pointer(Pointer {
                        dci: ep.dci(),
                        buf: Dma::new(len, 4096).ok_or(XferError::NoResources)?,
                        len,
                        layout,
                        last: None,
                    }));
                }
                (desc::CLASS_MSC, scsi::SUBCLASS_SCSI, scsi::PROTOCOL_BOT) => {
                    let (Some(i_ep), Some(o_ep)) = (bulk_in, bulk_out) else {
                        continue;
                    };
                    eps.push(i_ep);
                    eps.push(o_ep);
                    funcs.push(Func::Msc(Msc {
                        iface: i.number,
                        in_ep: i_ep,
                        out_ep: o_ep,
                        tag: 0,
                        cbw: Dma::new(64, 64).ok_or(XferError::NoResources)?,
                        csw: Dma::new(64, 64).ok_or(XferError::NoResources)?,
                        data: Dma::new(MAX_XFER, MAX_XFER).ok_or(XferError::NoResources)?,
                        block_size: 0,
                        blocks: 0,
                        role: Role::Other,
                        sync_unsupported: false,
                    }));
                }
                (desc::CLASS_PRINTER, 1, 1..=3) => {
                    let Some(o_ep) = bulk_out else { continue };
                    eps.push(o_ep);
                    funcs.push(Func::Printer(UsbPrinter {
                        iface: i.number,
                        out_dci: o_ep.dci(),
                        buf: Dma::new(MAX_XFER, MAX_XFER).ok_or(XferError::NoResources)?,
                    }));
                }
                _ => {}
            }
        }
        if !eps.is_empty() {
            ctrl.configure_endpoints(slot, &eps, hub_info.map(|(p, t, _)| (p, t)))?;
        }
        if let Some((ports, _, pwr_ms)) = hub_info {
            for p in 1..=ports {
                let _ = ctrl.control(slot, hub_port_req(HUB_SET_FEATURE, PORT_POWER, p), &mut []);
            }
            crate::clock::spin_ms(pwr_ms.max(100));
        }
        for f in &mut funcs {
            match f {
                Func::Hub(h) => {
                    ctrl.queue_normal(slot, h.dci, &h.buf, 2)?;
                }
                Func::Keyboard(k) => {
                    ctrl.queue_normal(slot, k.dci, &k.buf, 8)?;
                }
                Func::Pointer(p) => {
                    ctrl.queue_normal(slot, p.dci, &p.buf, p.len)?;
                }
                Func::Msc(m) => {
                    let mut dev = MscDev {
                        ctrl: &mut *ctrl,
                        slot,
                        m,
                    };
                    if let Err(e) = dev.start() {
                        klog!("usb {topo}: mass storage not ready ({e:?})");
                        continue;
                    }
                    let guid = part::read_table(&mut dev).ok().and_then(|t| t.disk_guid);
                    if guid.is_some() && guid == self.boot_disk {
                        dev.m.role = Role::Boot;
                    }
                    klog!(
                        "usb {topo}: disk {} x {} bytes, {:?}",
                        dev.m.blocks,
                        dev.m.block_size,
                        dev.m.role
                    );
                }
                Func::Printer(_) => {}
            }
        }
        let id = self.next_id;
        self.next_id += 1;
        Ok(Device {
            ctrl: c,
            slot,
            topo,
            id,
            vid: dd.vendor,
            pid: dd.product,
            funcs,
        })
    }

    /// Remove the device at `at` and everything behind it (hubs).
    fn detach_tree(&mut self, c: usize, at: Topo) {
        let mut i = 0;
        while i < self.devices.len() {
            let d = &self.devices[i];
            if d.ctrl == c && at.contains(&d.topo) {
                let d = self.devices.remove(i);
                klog!("usb {}: {:04x}:{:04x} removed", d.topo, d.vid, d.pid);
                self.ctrls[c].disable_slot(d.slot);
            } else {
                i += 1;
            }
        }
    }

    /// Handle hot-plug and drain HID reports. Called from the main loop.
    pub fn service(&mut self, now_us: u64) {
        for c in 0..self.ctrls.len() {
            while let Some(port) = self.ctrls[c].take_port_event() {
                if port == 0 || port > self.ctrls[c].ports {
                    continue;
                }
                let connected = self.ctrls[c].connected(port);
                let at = Topo {
                    root_port: port,
                    route: 0,
                    depth: 0,
                    speed: 0,
                    tt: None,
                };
                let present = self
                    .devices
                    .iter()
                    .any(|d| d.ctrl == c && d.topo.root_port == port && d.topo.depth == 0);
                self.ctrls[c].clear_port_changes(port);
                if !connected {
                    self.failed
                        .retain(|(fc, ft)| !(*fc == c && ft.root_port == port));
                } else if self.is_failed(c, &at) {
                    continue;
                }
                match (connected, present) {
                    (true, false) => self.attach_root(c, port),
                    (false, true) => self.detach_tree(c, at),
                    _ => {}
                }
            }
        }
        let (w, h) = (self.width, self.height);
        let mut dead = Vec::new();
        let mut hub_changes: Vec<(u64, u8)> = Vec::new();
        for d in &mut self.devices {
            let ctrl = &mut self.ctrls[d.ctrl];
            for f in &mut d.funcs {
                match f {
                    Func::Keyboard(k) => {
                        while let Some(ev) = ctrl.take_transfer(d.slot, k.dci) {
                            if ev.cc == crate::xhci::CC_SUCCESS || ev.cc == crate::xhci::CC_SHORT {
                                let mut r = [0u8; 8];
                                k.buf.copy_out(&mut r);
                                for (key, mods) in k.kb.report(&r, now_us) {
                                    self.events.push_back(InputEvent::Key {
                                        key,
                                        mods,
                                        at_us: now_us,
                                    });
                                }
                            } else {
                                k.kb.reset();
                                ctrl.recover(d.slot, k.dci);
                            }
                            if ctrl.queue_normal(d.slot, k.dci, &k.buf, 8).is_err() {
                                dead.push(d.id);
                            }
                        }
                        if let Some((key, mods)) = k.kb.repeat(now_us) {
                            self.events.push_back(InputEvent::Key {
                                key,
                                mods,
                                at_us: now_us,
                            });
                        }
                    }
                    Func::Pointer(p) => {
                        while let Some(ev) = ctrl.take_transfer(d.slot, p.dci) {
                            if ev.cc == crate::xhci::CC_SUCCESS || ev.cc == crate::xhci::CC_SHORT {
                                let n = p.len.saturating_sub(ev.param as usize);
                                let mut r = vec![0u8; n];
                                p.buf.copy_out(&mut r);
                                if let Some(s) = p.layout.decode(&r, w, h) {
                                    let cur = (s.x, s.y, s.pressed);
                                    if p.last != Some(cur) {
                                        p.last = Some(cur);
                                        self.events.push_back(InputEvent::Pointer {
                                            x: s.x,
                                            y: s.y,
                                            pressed: s.pressed,
                                            at_us: now_us,
                                        });
                                    }
                                }
                            } else {
                                ctrl.recover(d.slot, p.dci);
                            }
                            if ctrl.queue_normal(d.slot, p.dci, &p.buf, p.len).is_err() {
                                dead.push(d.id);
                            }
                        }
                    }
                    Func::Hub(hb) => {
                        while let Some(ev) = ctrl.take_transfer(d.slot, hb.dci) {
                            if ev.cc == crate::xhci::CC_SUCCESS || ev.cc == crate::xhci::CC_SHORT {
                                let mut bm = [0u8; 2];
                                hb.buf.copy_out(&mut bm);
                                let bits = u16::from_le_bytes(bm);
                                for p in 1..=hb.ports {
                                    if bits & (1 << p) != 0 {
                                        hub_changes.push((d.id, p));
                                    }
                                }
                            } else {
                                ctrl.recover(d.slot, hb.dci);
                            }
                            if ctrl.queue_normal(d.slot, hb.dci, &hb.buf, 2).is_err() {
                                dead.push(d.id);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        for (hub, port) in hub_changes {
            self.hub_port_changed(hub, port);
        }
        for id in dead {
            if let Some(d) = self.devices.iter().find(|d| d.id == id) {
                let (c, t) = (d.ctrl, d.topo);
                self.detach_tree(c, t);
            }
        }
    }

    fn has_pointer(&self) -> bool {
        self.devices
            .iter()
            .any(|d| d.funcs.iter().any(|f| matches!(f, Func::Pointer(_))))
    }

    /// Run `f` on the first mass-storage function matching `pick`.
    fn with_msc<T>(
        &mut self,
        pick: impl Fn(u64, &Msc) -> bool,
        f: impl FnOnce(u64, &mut MscDev<'_>) -> HalResult<T>,
    ) -> HalResult<T> {
        for d in &mut self.devices {
            let id = d.id;
            for func in &mut d.funcs {
                if let Func::Msc(m) = func
                    && m.block_size != 0
                    && pick(id, m)
                {
                    let mut dev = MscDev {
                        ctrl: &mut self.ctrls[d.ctrl],
                        slot: d.slot,
                        m,
                    };
                    return f(id, &mut dev);
                }
            }
        }
        Err(HalError::NotPresent)
    }

    /// Device generation and data-partition window on the boot stick.
    pub fn boot_partition(&mut self) -> HalResult<(u64, u64, u64)> {
        self.with_msc(
            |_, m| m.role == Role::Boot,
            |id, dev| {
                let t = part::read_table(dev)?;
                let p = t
                    .partitions
                    .iter()
                    .find(|p| p.type_guid == part::POS_DATA_TYPE)
                    .ok_or(HalError::NotFound)?;
                Ok((id, p.first_lba, p.blocks))
            },
        )
    }
}

/// A mass-storage function borrowed together with its controller.
#[derive(Debug)]
pub struct MscDev<'a> {
    ctrl: &'a mut Controller,
    slot: u8,
    m: &'a mut Msc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScsiError {
    Xfer(XferError),
    Check(Option<u8>),
}

impl MscDev<'_> {
    fn clear_halt(&mut self, ep: Endpoint) {
        let _ = self
            .ctrl
            .control(self.slot, Setup::clear_halt(ep.address), &mut []);
    }

    /// BOT reset recovery (USB MSC BOT 5.3.4).
    fn reset_recovery(&mut self) {
        let iface = self.m.iface;
        let _ = self.ctrl.control(
            self.slot,
            Setup::class_iface(true, scsi::REQ_BOT_RESET, 0, iface, 0),
            &mut [],
        );
        let (i, o) = (self.m.in_ep, self.m.out_ep);
        self.ctrl.recover(self.slot, i.dci());
        self.ctrl.recover(self.slot, o.dci());
        self.clear_halt(i);
        self.clear_halt(o);
    }

    /// One CBW / data / CSW exchange. Data goes through the 64 KiB bounce
    /// buffer: `out` is copied in before, `inp` copied out after.
    fn transport(
        &mut self,
        c: &Command,
        out: Option<&[u8]>,
        inp: Option<&mut [u8]>,
    ) -> Result<CswStatus, XferError> {
        self.m.tag = self.m.tag.wrapping_add(1);
        let tag = self.m.tag;
        let (i_dci, o_dci) = (self.m.in_ep.dci(), self.m.out_ep.dci());
        self.m.cbw.copy_in(&scsi::cbw(tag, 0, c));
        if let Err(e) = self.ctrl.bulk(
            self.slot,
            o_dci,
            &self.m.cbw,
            scsi::CBW_LEN,
            BULK_TIMEOUT_US,
        ) {
            self.reset_recovery();
            return Err(e);
        }
        let len = c.data_len as usize;
        if len > 0 {
            let r = match c.dir {
                Dir::Out => {
                    if let Some(src) = out {
                        self.m.data.copy_in(&src[..len]);
                    }
                    self.ctrl
                        .bulk(self.slot, o_dci, &self.m.data, len, BULK_TIMEOUT_US)
                }
                _ => self
                    .ctrl
                    .bulk(self.slot, i_dci, &self.m.data, len, BULK_TIMEOUT_US),
            };
            match r {
                Ok(_) => {}
                Err(XferError::Stall) => {
                    let ep = if c.dir == Dir::Out {
                        self.m.out_ep
                    } else {
                        self.m.in_ep
                    };
                    self.clear_halt(ep);
                }
                Err(e) => {
                    self.reset_recovery();
                    return Err(e);
                }
            }
        }
        let mut csw = self.ctrl.bulk(
            self.slot,
            i_dci,
            &self.m.csw,
            scsi::CSW_LEN,
            BULK_TIMEOUT_US,
        );
        if csw == Err(XferError::Stall) {
            self.clear_halt(self.m.in_ep);
            csw = self.ctrl.bulk(
                self.slot,
                i_dci,
                &self.m.csw,
                scsi::CSW_LEN,
                BULK_TIMEOUT_US,
            );
        }
        let mut raw = [0u8; scsi::CSW_LEN];
        match csw {
            Ok(n) if n == scsi::CSW_LEN => self.m.csw.copy_out(&mut raw),
            Ok(_) => {
                self.reset_recovery();
                return Err(XferError::Failed(0));
            }
            Err(e) => {
                self.reset_recovery();
                return Err(e);
            }
        }
        let Some(s) = scsi::parse_csw(&raw, tag) else {
            self.reset_recovery();
            return Err(XferError::Failed(0));
        };
        if s.status == CswStatus::PhaseError {
            self.reset_recovery();
        }
        if s.status == CswStatus::Passed
            && c.dir == Dir::In
            && let Some(dst) = inp
        {
            self.m.data.copy_out(&mut dst[..len]);
        }
        Ok(s.status)
    }

    fn scsi(
        &mut self,
        c: &Command,
        out: Option<&[u8]>,
        inp: Option<&mut [u8]>,
    ) -> Result<(), ScsiError> {
        match self.transport(c, out, inp).map_err(ScsiError::Xfer)? {
            CswStatus::Passed => Ok(()),
            _ => {
                let mut sense = [0u8; 18];
                let key = match self.transport(&scsi::request_sense(), None, Some(&mut sense)) {
                    Ok(CswStatus::Passed) => scsi::sense_key(&sense),
                    _ => None,
                };
                Err(ScsiError::Check(key))
            }
        }
    }

    /// INQUIRY, wait for the unit to become ready, read the capacity.
    fn start(&mut self) -> Result<(), ScsiError> {
        let mut inq = [0u8; 36];
        self.scsi(&scsi::inquiry(), None, Some(&mut inq))?;
        if inq[0] & 0x1f != 0 {
            return Err(ScsiError::Check(None)); // not a direct-access device
        }
        let mut ready = false;
        for _ in 0..50 {
            if self.scsi(&scsi::test_unit_ready(), None, None).is_ok() {
                ready = true;
                break;
            }
            crate::clock::spin_ms(100);
        }
        if !ready {
            return Err(ScsiError::Check(Some(scsi::SENSE_NOT_READY)));
        }
        let mut cap = [0u8; 8];
        self.scsi(&scsi::read_capacity10(), None, Some(&mut cap))?;
        let (blocks, bs) = match scsi::parse_capacity10(&cap) {
            Some(v) => v,
            None => {
                let mut c16 = [0u8; 32];
                self.scsi(&scsi::read_capacity16(), None, Some(&mut c16))?;
                scsi::parse_capacity16(&c16).ok_or(ScsiError::Check(None))?
            }
        };
        if !matches!(bs, 512 | 4096) {
            return Err(ScsiError::Check(None));
        }
        self.m.blocks = blocks;
        self.m.block_size = bs as usize;
        Ok(())
    }

    fn hal(e: ScsiError) -> HalError {
        match e {
            ScsiError::Xfer(x) => bad(x),
            ScsiError::Check(Some(scsi::SENSE_DATA_PROTECT)) => HalError::WriteProtected,
            ScsiError::Check(Some(scsi::SENSE_UNIT_ATTENTION | scsi::SENSE_NOT_READY)) => {
                HalError::MediaChanged
            }
            ScsiError::Check(_) => HalError::Io,
        }
    }

    fn rw(
        &mut self,
        write: bool,
        lba: u64,
        out: Option<&[u8]>,
        mut inp: Option<&mut [u8]>,
    ) -> HalResult<()> {
        let bs = self.m.block_size;
        let len = out.map_or_else(|| inp.as_ref().map_or(0, |b| b.len()), <[u8]>::len);
        if bs == 0 || len == 0 || !len.is_multiple_of(bs) {
            return Err(HalError::InvalidArgument);
        }
        let n = (len / bs) as u64;
        if lba.checked_add(n).is_none_or(|e| e > self.m.blocks) {
            return Err(HalError::InvalidArgument);
        }
        let per = (MAX_XFER / bs) as u64;
        let mut done = 0u64;
        while done < n {
            let k = (n - done).min(per);
            let range = (done as usize * bs)..((done + k) as usize * bs);
            let c = scsi::rw(write, lba + done, k as u16, bs as u32);
            let mut last = Ok(());
            // One retry after reset recovery covers transient stick errors.
            for _ in 0..2 {
                last = match (&out, &mut inp) {
                    (Some(src), _) => self.scsi(&c, Some(&src[range.clone()]), None),
                    (None, Some(dst)) => self.scsi(&c, None, Some(&mut dst[range.clone()])),
                    _ => Ok(()),
                };
                if last.is_ok() {
                    break;
                }
            }
            last.map_err(Self::hal)?;
            done += k;
        }
        Ok(())
    }
}

impl BlockDevice for MscDev<'_> {
    fn block_size(&self) -> usize {
        self.m.block_size
    }
    fn block_count(&self) -> u64 {
        self.m.blocks
    }
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> HalResult<()> {
        self.rw(false, lba, None, Some(buf))
    }
    fn write(&mut self, lba: u64, buf: &[u8]) -> HalResult<()> {
        self.rw(true, lba, Some(buf), None)
    }
    /// SYNCHRONIZE CACHE. A stick that rejects it as an illegal request has
    /// no volatile cache to flush (common for cheap sticks); every other
    /// failure is reported, so the store never acknowledges a commit that a
    /// device said it could not make durable.
    fn flush(&mut self) -> HalResult<()> {
        if self.m.sync_unsupported {
            return Ok(());
        }
        match self.scsi(&scsi::synchronize_cache(), None, None) {
            Ok(()) => Ok(()),
            Err(ScsiError::Check(Some(0x05))) => {
                self.m.sync_unsupported = true;
                klog!("usb disk: SYNCHRONIZE CACHE unsupported (write-through device)");
                Ok(())
            }
            Err(e) => Err(Self::hal(e)),
        }
    }
}

// ---------------------------------------------------------------- HAL handles

#[derive(Debug)]
pub struct UsbInput {
    pub stack: Shared,
}

impl pos_hal::Input for UsbInput {
    fn poll(&mut self) -> Option<InputEvent> {
        let mut s = self.stack.try_borrow_mut().ok()?;
        if s.events.is_empty() {
            s.service(crate::clock::now_us());
        }
        s.events.pop_front()
    }
    fn has_pointer(&self) -> bool {
        self.stack.try_borrow().is_ok_and(|s| s.has_pointer())
    }
}

/// The POS data partition on the boot stick. Fails with `MediaChanged` if
/// the stick was unplugged (even if it comes back: the store re-opens on
/// the next boot).
#[derive(Debug)]
pub struct BootDisk {
    stack: Shared,
    id: u64,
    first: u64,
    blocks: u64,
    block_size: usize,
}

impl BootDisk {
    pub fn open(stack: &Shared) -> HalResult<BootDisk> {
        let mut s = stack.try_borrow_mut().map_err(|_| HalError::Io)?;
        let (id, first, blocks) = s.boot_partition()?;
        let block_size = s.with_msc(|i, _| i == id, |_, d| Ok(d.block_size()))?;
        drop(s);
        Ok(BootDisk {
            stack: stack.clone(),
            id,
            first,
            blocks,
            block_size,
        })
    }

    fn with<T>(
        &mut self,
        f: impl FnOnce(&mut Slice<&mut MscDev<'_>>) -> HalResult<T>,
    ) -> HalResult<T> {
        let id = self.id;
        let (first, blocks) = (self.first, self.blocks);
        let mut s = self.stack.try_borrow_mut().map_err(|_| HalError::Io)?;
        s.with_msc(
            |i, _| i == id,
            |_, d| {
                let mut sl = Slice {
                    dev: d,
                    first,
                    blocks,
                };
                f(&mut sl)
            },
        )
        .map_err(|e| {
            if e == HalError::NotPresent {
                HalError::MediaChanged
            } else {
                e
            }
        })
    }
}

impl BlockDevice for BootDisk {
    fn block_size(&self) -> usize {
        self.block_size
    }
    fn block_count(&self) -> u64 {
        self.blocks
    }
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> HalResult<()> {
        self.with(|d| d.read(lba, buf))
    }
    fn write(&mut self, lba: u64, buf: &[u8]) -> HalResult<()> {
        self.with(|d| d.write(lba, buf))
    }
    fn flush(&mut self) -> HalResult<()> {
        self.with(|d| d.flush())
    }
}

/// Export/import stick: the first non-boot mass-storage device with a FAT
/// volume (GPT, MBR or superfloppy).
#[derive(Debug)]
pub struct UsbMedia {
    pub stack: Shared,
}

fn with_fat<T>(
    stack: &Shared,
    f: impl FnOnce(&mut pos_drivers::fat::Fat<'_>) -> HalResult<T>,
) -> HalResult<T> {
    let mut s = stack.try_borrow_mut().map_err(|_| HalError::Io)?;
    s.service(crate::clock::now_us());
    s.with_msc(
        |_, m| m.role == Role::Other,
        |_, d| {
            let table = part::read_table(d)?;
            let cands: Vec<(u64, u64)> = if table.partitions.is_empty() {
                vec![(0, d.block_count())]
            } else {
                table
                    .partitions
                    .iter()
                    .filter(|p| p.type_guid != part::ESP_TYPE && p.type_guid != part::POS_DATA_TYPE)
                    .map(|p| (p.first_lba, p.blocks))
                    .collect()
            };
            for (first, blocks) in cands {
                let mut sl = Slice {
                    dev: &mut *d,
                    first,
                    blocks,
                };
                if let Ok(mut fs) = pos_drivers::fat::Fat::mount(&mut sl) {
                    return f(&mut fs);
                }
            }
            Err(HalError::NotFound)
        },
    )
}

impl pos_hal::ExternalMedia for UsbMedia {
    fn is_present(&mut self) -> bool {
        with_fat(&self.stack, |_| Ok(())).is_ok()
    }
    fn write_file(&mut self, name: &str, data: &[u8]) -> HalResult<()> {
        let when = crate::clock::wall().ok();
        with_fat(&self.stack, |fs| fs.write_file(name, data, when))
    }
    fn read_file(&mut self, name: &str, max_len: usize) -> HalResult<Vec<u8>> {
        with_fat(&self.stack, |fs| fs.read_file(name, max_len))
    }
}

/// USB Printer class if one is attached, else the COM2 serial fallback.
#[derive(Debug)]
pub struct KernelPrinter {
    pub stack: Shared,
    pub serial: crate::serial::SerialPrinter,
}

fn find_printer(devices: &mut [Device]) -> Option<(usize, u8, &mut UsbPrinter)> {
    devices.iter_mut().find_map(|d| {
        let (c, slot) = (d.ctrl, d.slot);
        d.funcs.iter_mut().find_map(|f| match f {
            Func::Printer(p) => Some((c, slot, p)),
            _ => None,
        })
    })
}

impl pos_hal::Printer for KernelPrinter {
    fn write(&mut self, bytes: &[u8]) -> HalResult<()> {
        let Ok(mut guard) = self.stack.try_borrow_mut() else {
            return self.serial.write(bytes);
        };
        let Stack { ctrls, devices, .. } = &mut *guard;
        let Some((c, slot, p)) = find_printer(devices) else {
            drop(guard);
            return self.serial.write(bytes);
        };
        for chunk in bytes.chunks(MAX_XFER) {
            p.buf.copy_in(chunk);
            ctrls[c]
                .bulk(slot, p.out_dci, &p.buf, chunk.len(), BULK_TIMEOUT_US)
                .map_err(bad)?;
        }
        Ok(())
    }
    fn status(&mut self) -> PrinterStatus {
        let Ok(mut guard) = self.stack.try_borrow_mut() else {
            return self.serial.status();
        };
        let Stack { ctrls, devices, .. } = &mut *guard;
        let Some((c, slot, p)) = find_printer(devices) else {
            drop(guard);
            return self.serial.status();
        };
        let mut st = [0u8; 1];
        match ctrls[c].control(slot, Setup::class_iface(false, 1, 0, p.iface, 1), &mut st) {
            Ok(_) if st[0] & 0x20 != 0 => PrinterStatus::PaperOut,
            Ok(_) if st[0] & 0x08 == 0 => PrinterStatus::Error,
            Ok(_) => PrinterStatus::Ready,
            Err(_) => PrinterStatus::Error,
        }
    }
    fn describe(&self) -> String {
        if let Ok(s) = self.stack.try_borrow() {
            for d in &s.devices {
                if d.funcs.iter().any(|f| matches!(f, Func::Printer(_))) {
                    return alloc::format!("USB printer {:04x}:{:04x}", d.vid, d.pid);
                }
            }
        }
        self.serial.describe()
    }
}
