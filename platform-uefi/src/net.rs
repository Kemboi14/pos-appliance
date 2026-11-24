//! Ethernet through the firmware's Simple Network Protocol.
//!
//! Opened only when the till is configured as a head or lane; a standalone
//! till never touches the network. The SNP is opened exclusively, which
//! disconnects the firmware's own network stack (MNP/IP/PXE) from the card,
//! so nothing but this app's encrypted store traffic uses it.

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use pos_hal::{HalError, HalResult};
use uefi::boot::{self, ScopedProtocol};
use uefi::proto::network::snp::{NetworkState, ReceiveFlags, SimpleNetwork};

const MAX_IN_FLIGHT: usize = 64;

pub struct UefiNic {
    snp: ScopedProtocol<SimpleNetwork>,
    mac: [u8; 6],
    /// Frames handed to the firmware; freed when it recycles them.
    in_flight: Vec<Box<[u8]>>,
}

impl core::fmt::Debug for UefiNic {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "UefiNic({:02x?})", self.mac)
    }
}

impl UefiNic {
    /// Open the first usable card: the Wi-Fi one first when `prefer_wifi`
    /// (joined through `wifi::UefiWifi`), otherwise wired cards first.
    pub fn init(prefer_wifi: bool) -> Option<UefiNic> {
        let find = || {
            boot::find_handles::<SimpleNetwork>()
                .ok()
                .filter(|v| !v.is_empty())
        };
        let handles = match find() {
            Some(h) => h,
            None => {
                // Boot managers often connect only the boot device: bind
                // drivers to every PCI device, then look again.
                if let Ok(hs) = boot::find_handles::<uefi::proto::device_path::DevicePath>() {
                    for h in hs {
                        let _ = boot::connect_controller(h, &[], None, true);
                    }
                }
                find()?
            }
        };
        let radios = crate::wifi::UefiWifi::nic_handles();
        let mut handles = handles;
        handles.sort_by_key(|h| radios.contains(h) != prefer_wifi);
        for h in handles {
            let Ok(mut snp) = boot::open_protocol_exclusive::<SimpleNetwork>(h) else {
                continue;
            };
            if snp.mode().state == NetworkState::STOPPED && snp.start().is_err() {
                continue;
            }
            if snp.mode().state == NetworkState::STARTED && snp.initialize(0, 0).is_err() {
                continue;
            }
            let _ = snp.receive_filters(
                ReceiveFlags::UNICAST | ReceiveFlags::BROADCAST,
                ReceiveFlags::empty(),
                false,
                None,
            );
            let a = snp.mode().current_address;
            let mac = [a.0[0], a.0[1], a.0[2], a.0[3], a.0[4], a.0[5]];
            return Some(UefiNic {
                snp,
                mac,
                in_flight: Vec::new(),
            });
        }
        None
    }

    fn recycle(&mut self) {
        while let Ok(Some(p)) = self.snp.get_recycled_transmit_buffer_status() {
            let p = p.as_ptr().cast_const();
            self.in_flight.retain(|b| b.as_ptr() != p);
        }
    }
}

impl pos_hal::Nic for UefiNic {
    fn mac(&self) -> [u8; 6] {
        self.mac
    }

    fn link_up(&mut self) -> bool {
        let _ = self.snp.get_interrupt_status();
        let m = self.snp.mode();
        !bool::from(m.media_present_supported) || bool::from(m.media_present)
    }

    fn transmit(&mut self, frame: &[u8]) -> HalResult<()> {
        self.recycle();
        if self.in_flight.len() >= MAX_IN_FLIGHT {
            return Err(HalError::Timeout);
        }
        // The firmware may DMA from the buffer until it recycles it.
        let mut b: Box<[u8]> = frame.into();
        let r = self.snp.transmit(0, &mut b, None, None, None);
        match r {
            Ok(()) => {
                self.in_flight.push(b);
                Ok(())
            }
            Err(_) => Err(HalError::Io),
        }
    }

    fn receive(&mut self, buf: &mut [u8]) -> Option<usize> {
        let mut tmp = vec![0u8; buf.len().max(1600)];
        match self.snp.receive(&mut tmp, None, None, None, None) {
            Ok(n) if n <= buf.len() => {
                buf[..n].copy_from_slice(&tmp[..n]);
                Some(n)
            }
            _ => None,
        }
    }
}
