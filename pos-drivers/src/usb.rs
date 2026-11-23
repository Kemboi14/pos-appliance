//! USB standard descriptors and requests.

use alloc::vec::Vec;

pub const DESC_DEVICE: u8 = 1;
pub const DESC_CONFIG: u8 = 2;
pub const DESC_INTERFACE: u8 = 4;
pub const DESC_ENDPOINT: u8 = 5;
pub const DESC_HID: u8 = 0x21;
pub const DESC_HID_REPORT: u8 = 0x22;

pub const CLASS_HID: u8 = 3;
pub const CLASS_PRINTER: u8 = 7;
pub const CLASS_MSC: u8 = 8;
pub const CLASS_HUB: u8 = 9;

/// An 8-byte SETUP packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Setup {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

impl Setup {
    pub fn to_u64(self) -> u64 {
        u64::from(self.request_type)
            | u64::from(self.request) << 8
            | u64::from(self.value) << 16
            | u64::from(self.index) << 32
            | u64::from(self.length) << 48
    }
    pub fn is_in(self) -> bool {
        self.request_type & 0x80 != 0
    }
    pub fn get_descriptor(kind: u8, index: u8, length: u16) -> Setup {
        Setup {
            request_type: 0x80,
            request: 6,
            value: u16::from(kind) << 8 | u16::from(index),
            index: 0,
            length,
        }
    }
    /// Class descriptor (e.g. HID report) addressed to an interface.
    pub fn get_interface_descriptor(kind: u8, iface: u8, length: u16) -> Setup {
        Setup {
            request_type: 0x81,
            request: 6,
            value: u16::from(kind) << 8,
            index: u16::from(iface),
            length,
        }
    }
    pub fn set_configuration(value: u8) -> Setup {
        Setup {
            request_type: 0x00,
            request: 9,
            value: u16::from(value),
            index: 0,
            length: 0,
        }
    }
    pub fn clear_halt(endpoint_address: u8) -> Setup {
        Setup {
            request_type: 0x02,
            request: 1,
            value: 0,
            index: u16::from(endpoint_address),
            length: 0,
        }
    }
    /// Class-specific request to an interface.
    pub fn class_iface(out_dir: bool, request: u8, value: u16, iface: u8, length: u16) -> Setup {
        Setup {
            request_type: if out_dir { 0x21 } else { 0xa1 },
            request,
            value,
            index: u16::from(iface),
            length,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceDescriptor {
    pub usb_version: u16,
    pub class: u8,
    pub max_packet0: u8,
    pub vendor: u16,
    pub product: u16,
    pub num_configs: u8,
}

pub fn parse_device(b: &[u8]) -> Option<DeviceDescriptor> {
    if b.len() < 18 || b[1] != DESC_DEVICE {
        return None;
    }
    Some(DeviceDescriptor {
        usb_version: u16::from_le_bytes([b[2], b[3]]),
        class: b[4],
        max_packet0: b[7],
        vendor: u16::from_le_bytes([b[8], b[9]]),
        product: u16::from_le_bytes([b[10], b[11]]),
        num_configs: b[17],
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferType {
    Control,
    Isochronous,
    Bulk,
    Interrupt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Endpoint {
    pub address: u8,
    pub kind: TransferType,
    pub max_packet: u16,
    pub interval: u8,
}

impl Endpoint {
    pub fn is_in(&self) -> bool {
        self.address & 0x80 != 0
    }
    pub fn number(&self) -> u8 {
        self.address & 0x0f
    }
    /// xHCI device context index: 2 * number + direction (IN = 1).
    pub fn dci(&self) -> u8 {
        self.number() * 2 + u8::from(self.is_in())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interface {
    pub number: u8,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub endpoints: Vec<Endpoint>,
    /// Length of the HID report descriptor, if this is a HID interface.
    pub hid_report_len: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Configuration {
    pub value: u8,
    pub interfaces: Vec<Interface>,
}

/// Parse a full configuration descriptor (with its interface, endpoint and
/// class descriptors). Only alternate setting 0 of each interface is kept.
pub fn parse_config(b: &[u8]) -> Option<Configuration> {
    if b.len() < 9 || b[1] != DESC_CONFIG {
        return None;
    }
    let total = usize::from(u16::from_le_bytes([b[2], b[3]])).min(b.len());
    let mut cfg = Configuration {
        value: b[5],
        interfaces: Vec::new(),
    };
    let mut i = 0;
    let mut in_alt0 = false;
    while i + 2 <= total {
        let len = usize::from(b[i]);
        if len < 2 || i + len > total {
            break;
        }
        let d = &b[i..i + len];
        match d[1] {
            DESC_INTERFACE if len >= 9 => {
                in_alt0 = d[3] == 0;
                if in_alt0 {
                    cfg.interfaces.push(Interface {
                        number: d[2],
                        class: d[5],
                        subclass: d[6],
                        protocol: d[7],
                        endpoints: Vec::new(),
                        hid_report_len: None,
                    });
                }
            }
            DESC_ENDPOINT if len >= 7 && in_alt0 => {
                if let Some(iface) = cfg.interfaces.last_mut() {
                    iface.endpoints.push(Endpoint {
                        address: d[2],
                        kind: match d[3] & 3 {
                            0 => TransferType::Control,
                            1 => TransferType::Isochronous,
                            2 => TransferType::Bulk,
                            _ => TransferType::Interrupt,
                        },
                        max_packet: u16::from_le_bytes([d[4], d[5]]) & 0x7ff,
                        interval: d[6],
                    });
                }
            }
            DESC_HID if len >= 9 && in_alt0 => {
                if let Some(iface) = cfg.interfaces.last_mut() {
                    // First class descriptor listed is normally the report.
                    if d[6] == DESC_HID_REPORT {
                        iface.hid_report_len = Some(u16::from_le_bytes([d[7], d[8]]));
                    }
                }
            }
            _ => {}
        }
        i += len;
    }
    Some(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    // QEMU usb-kbd configuration descriptor.
    const KBD_CFG: [u8; 34] = [
        9, 2, 34, 0, 1, 1, 0, 0xa0, 50, // config
        9, 4, 0, 0, 1, 3, 1, 1, 0, // interface 0: HID boot keyboard
        9, 0x21, 0x11, 0x01, 0, 1, 0x22, 63, 0, // HID, report 63 bytes
        7, 5, 0x81, 3, 8, 0, 7, // EP1 IN interrupt, 8 bytes, interval 7
    ];

    #[test]
    fn keyboard_config() {
        let c = parse_config(&KBD_CFG).unwrap();
        assert_eq!(c.value, 1);
        let i = &c.interfaces[0];
        assert_eq!((i.class, i.subclass, i.protocol), (3, 1, 1));
        assert_eq!(i.hid_report_len, Some(63));
        let e = i.endpoints[0];
        assert_eq!(
            (e.kind, e.max_packet, e.dci(), e.is_in()),
            (TransferType::Interrupt, 8, 3, true)
        );
    }

    #[test]
    fn alt_settings_ignored_and_truncation_safe() {
        let mut b = KBD_CFG.to_vec();
        b.extend_from_slice(&[9, 4, 0, 1, 1, 3, 0, 0, 0, 7, 5, 0x82, 3, 8, 0, 1]);
        b[2] = b.len() as u8;
        let c = parse_config(&b).unwrap();
        assert_eq!(c.interfaces.len(), 1);
        assert_eq!(c.interfaces[0].endpoints.len(), 1);
        for n in 0..b.len() {
            let _ = parse_config(&b[..n]);
        }
    }

    #[test]
    fn setup_encoding() {
        let s = Setup::get_descriptor(DESC_DEVICE, 0, 18);
        assert_eq!(s.to_u64(), 0x0012_0000_0100_0680);
    }
}
