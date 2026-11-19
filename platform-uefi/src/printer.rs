//! Receipt printer: USB Printer class (interface class 0x07) through
//! EFI_USB_IO_PROTOCOL bulk OUT transfers; fallback to a serial port through
//! EFI_SERIAL_IO_PROTOCOL.
//!
//! No UEFI printer class driver exists, so the printer's UsbIo interface is
//! unclaimed and we open it exclusively. Serial selection: the UEFI variable
//! `PosPrinterSerial` (one byte: index into the SerialIo handle list) if set,
//! otherwise the second port when there are several (COM1 is usually the
//! firmware console), otherwise the first.

use alloc::format;
use alloc::string::String;
use pos_hal::{HalError, HalResult, PrinterStatus};
use uefi::boot::{self, ScopedProtocol};
use uefi::proto::console::serial::{IoMode, Serial};
use uefi::proto::usb::io::{ControlTransfer, UsbIo};

const USB_CLASS_PRINTER: u8 = 0x07;
const BULK_TIMEOUT_MS: usize = 5_000;

pub enum Port {
    Usb {
        io: ScopedProtocol<UsbIo>,
        ep_out: u8,
        iface: u16,
        vid: u16,
        pid: u16,
    },
    Serial(ScopedProtocol<Serial>, usize),
    None,
}

pub struct UefiPrinter {
    port: Port,
}

impl core::fmt::Debug for UefiPrinter {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&pos_hal::Printer::describe(self))
    }
}

fn find_usb() -> Option<Port> {
    for h in boot::find_handles::<UsbIo>().ok()? {
        // Probe with a shared open: an exclusive open would disconnect the
        // class driver (keyboard, mass storage — including our boot stick).
        let is_printer = crate::open_shared::<UsbIo>(h)
            .and_then(|mut io| io.interface_descriptor().ok())
            .is_some_and(|d| d.interface_class == USB_CLASS_PRINTER);
        if !is_printer {
            continue;
        }
        // No UEFI driver claims printer interfaces, so this disconnects
        // nothing and keeps other agents off the pipe.
        let Ok(mut io) = boot::open_protocol_exclusive::<UsbIo>(h) else {
            continue;
        };
        let Ok(ifd) = io.interface_descriptor() else {
            continue;
        };
        let mut ep_out = None;
        for i in 0..ifd.num_endpoints {
            if let Ok(ep) = io.endpoint_descriptor(i) {
                // Bulk (attributes 0b10), OUT (direction bit clear).
                if ep.attributes & 0x03 == 0x02 && ep.endpoint_address & 0x80 == 0 {
                    ep_out = Some(ep.endpoint_address);
                    break;
                }
            }
        }
        let Some(ep_out) = ep_out else { continue };
        let (vid, pid) = io
            .device_descriptor()
            .map(|d| (d.id_vendor, d.id_product))
            .unwrap_or((0, 0));
        return Some(Port::Usb {
            io,
            ep_out,
            iface: u16::from(ifd.interface_number),
            vid,
            pid,
        });
    }
    None
}

fn serial_index(count: usize) -> usize {
    let mut buf = [0u8; 1];
    let name = uefi::cstr16!("PosPrinterSerial");
    match uefi::runtime::get_variable(name, &crate::keyvar::VENDOR, &mut buf) {
        Ok((v, _)) if !v.is_empty() && usize::from(v[0]) < count => usize::from(v[0]),
        _ if count > 1 => 1,
        _ => 0,
    }
}

fn find_serial() -> Option<Port> {
    let hs = boot::find_handles::<Serial>().ok()?;
    if hs.is_empty() {
        return None;
    }
    let idx = serial_index(hs.len());
    let mut s = boot::open_protocol_exclusive::<Serial>(hs[idx]).ok()?;
    // 9600 8N1 is the factory default of most serial receipt printers.
    let mode = IoMode {
        baud_rate: 9600,
        data_bits: 8,
        ..*s.io_mode()
    };
    let _ = s.set_attributes(&mode);
    Some(Port::Serial(s, idx))
}

impl UefiPrinter {
    pub fn init() -> UefiPrinter {
        let port = find_usb().or_else(find_serial).unwrap_or(Port::None);
        UefiPrinter { port }
    }
}

impl pos_hal::Printer for UefiPrinter {
    fn write(&mut self, bytes: &[u8]) -> HalResult<()> {
        match &mut self.port {
            Port::Usb { io, ep_out, .. } => {
                for chunk in bytes.chunks(4096) {
                    let mut off = 0;
                    while off < chunk.len() {
                        let n = io
                            .sync_bulk_send(*ep_out, &chunk[off..], BULK_TIMEOUT_MS)
                            .map_err(|_| HalError::Io)?;
                        if n == 0 {
                            return Err(HalError::Timeout);
                        }
                        off += n;
                    }
                }
                Ok(())
            }
            Port::Serial(s, _) => {
                for chunk in bytes.chunks(256) {
                    s.write_exact(chunk).map_err(|_| HalError::Timeout)?;
                }
                Ok(())
            }
            Port::None => Err(HalError::NotPresent),
        }
    }

    fn status(&mut self) -> PrinterStatus {
        match &mut self.port {
            Port::Usb { io, iface, .. } => {
                // Printer class GET_PORT_STATUS: bit 5 paper empty, bit 3
                // not-error (1 = OK).
                let mut st = [0u8; 1];
                let r = io.control_transfer(
                    0xA1,
                    1,
                    0,
                    *iface << 8,
                    ControlTransfer::DataIn(&mut st),
                    1000,
                );
                match r {
                    Ok(()) if st[0] & 0x20 != 0 => PrinterStatus::PaperOut,
                    Ok(()) if st[0] & 0x08 == 0 => PrinterStatus::Error,
                    _ => PrinterStatus::Ready,
                }
            }
            Port::Serial(..) => PrinterStatus::Ready,
            Port::None => PrinterStatus::NotPresent,
        }
    }

    fn describe(&self) -> String {
        match &self.port {
            Port::Usb { vid, pid, .. } => format!("USB printer {vid:04x}:{pid:04x}"),
            Port::Serial(_, i) => format!("serial printer (port {i})"),
            Port::None => String::from("no printer"),
        }
    }
}
