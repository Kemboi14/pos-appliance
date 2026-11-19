//! External FAT media for export/import: any SimpleFileSystem volume that is
//! *not* on the boot disk (so the ESP is never written).

use alloc::vec;
use alloc::vec::Vec;
use pos_hal::{HalError, HalResult};
use uefi::CString16;
use uefi::boot;
use uefi::proto::media::file::{File, FileAttribute, FileInfo, FileMode};
use uefi::proto::media::fs::SimpleFileSystem;

#[derive(Debug)]
pub struct UsbMedia {
    boot_disk: Option<Vec<u8>>,
}

impl UsbMedia {
    pub fn new() -> UsbMedia {
        UsbMedia {
            boot_disk: crate::block::boot_disk_prefix(),
        }
    }

    fn volume(&self) -> HalResult<uefi::proto::media::file::Directory> {
        // Rescan each time: sticks are hot-plugged, and firmware boot
        // managers usually connect only the boot device. Connecting every
        // USB device recursively binds mass-storage, partition and FAT
        // drivers to any stick present now (already-bound devices just
        // report ALREADY_STARTED; the printer is held exclusively by us).
        if let Ok(hs) = boot::find_handles::<uefi::proto::usb::io::UsbIo>() {
            for h in hs {
                let _ = boot::connect_controller(h, &[], None, true);
            }
        }
        let hs = boot::find_handles::<SimpleFileSystem>().map_err(|_| HalError::NotPresent)?;
        for h in hs {
            let prefix = crate::block::disk_prefix_of(h);
            if prefix.is_some() && prefix == self.boot_disk {
                continue;
            }
            // Shared open: FAT is a leaf driver, but exclusive opens are
            // reserved for devices we own outright.
            if let Some(mut fs) = crate::open_shared::<SimpleFileSystem>(h)
                && let Ok(root) = fs.open_volume()
            {
                return Ok(root);
            }
        }
        Err(HalError::NotPresent)
    }
}

fn name16(n: &str) -> HalResult<CString16> {
    CString16::try_from(n).map_err(|_| HalError::InvalidArgument)
}

impl pos_hal::ExternalMedia for UsbMedia {
    fn is_present(&mut self) -> bool {
        self.volume().is_ok()
    }

    fn write_file(&mut self, name: &str, data: &[u8]) -> HalResult<()> {
        let mut root = self.volume()?;
        let n = name16(name)?;
        // Replace: delete any existing file first (FAT has no truncate here).
        if let Ok(f) = root.open(&n, FileMode::ReadWrite, FileAttribute::empty()) {
            f.delete().map_err(|_| HalError::Io)?;
        }
        let f = root
            .open(&n, FileMode::CreateReadWrite, FileAttribute::ARCHIVE)
            .map_err(|_| HalError::Io)?;
        let mut f = f.into_regular_file().ok_or(HalError::Io)?;
        f.write(data).map_err(|e| {
            if e.status() == uefi::Status::VOLUME_FULL {
                HalError::NoSpace
            } else {
                HalError::Io
            }
        })?;
        f.flush().map_err(|_| HalError::Io)?;
        root.flush().map_err(|_| HalError::Io)?;
        Ok(())
    }

    fn read_file(&mut self, name: &str, max_len: usize) -> HalResult<Vec<u8>> {
        let mut root = self.volume()?;
        let n = name16(name)?;
        let f = root
            .open(&n, FileMode::Read, FileAttribute::empty())
            .map_err(|_| HalError::NotFound)?;
        let mut f = f.into_regular_file().ok_or(HalError::NotFound)?;
        let info = f.get_boxed_info::<FileInfo>().map_err(|_| HalError::Io)?;
        let size = usize::try_from(info.file_size()).map_err(|_| HalError::NoSpace)?;
        if size > max_len {
            return Err(HalError::NoSpace);
        }
        let mut buf = vec![0u8; size];
        let mut off = 0;
        while off < size {
            let n = f.read(&mut buf[off..]).map_err(|_| HalError::Io)?;
            if n == 0 {
                break;
            }
            off += n;
        }
        buf.truncate(off);
        Ok(buf)
    }
}
