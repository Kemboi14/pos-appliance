//! The POS data partition as a `pos_hal::BlockDevice` over UEFI BlockIO.
//!
//! The partition is identified by its GPT *type* GUID
//! (`138f131d-afdf-4718-a368-f3126495b979`, via EFI_PARTITION_INFO_PROTOCOL)
//! and must sit on the same physical disk as the ESP we booted from, so a
//! second POS stick plugged in for export can never be mistaken for ours.
//! Firmware without PartitionInfo (pre-UEFI 2.7) falls back to the unique
//! partition GUID written by `cargo xtask image`, read from the HD() device
//! path node.

use alloc::vec;
use alloc::vec::Vec;
use pos_hal::{HalError, HalResult};
use uefi::boot::{self, ScopedProtocol};
use uefi::proto::device_path::{DevicePath, DevicePathNodeEnum, DeviceType};
use uefi::proto::loaded_image::LoadedImage;
use uefi::proto::media::block::BlockIO;
use uefi::proto::media::partition::PartitionInfo;
use uefi::{Guid, Handle, guid};

pub const POS_DATA_TYPE: Guid = guid!("138f131d-afdf-4718-a368-f3126495b979");
pub const POS_DATA_UNIQUE: Guid = guid!("c4e8a2d6-7b1f-4e9c-a3d5-6f8b0c2e4a17");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindError {
    NoBootDevice,
    NotFound,
    OpenFailed,
}

/// Device-path bytes of the disk (everything before the first MEDIA node).
pub fn disk_prefix_of(h: Handle) -> Option<Vec<u8>> {
    let dp = crate::open_shared::<DevicePath>(h)?;
    let mut out = Vec::new();
    for node in dp.node_iter() {
        if node.device_type() == DeviceType::MEDIA {
            break;
        }
        // SAFETY: a node is `length` bytes starting at its header.
        let bytes = unsafe {
            core::slice::from_raw_parts(
                core::ptr::from_ref(node).cast::<u8>(),
                usize::from(node.length()),
            )
        };
        out.extend_from_slice(bytes);
    }
    Some(out)
}

fn hd_signature(h: Handle) -> Option<Guid> {
    let dp = crate::open_shared::<DevicePath>(h)?;
    for node in dp.node_iter() {
        if let Ok(DevicePathNodeEnum::MediaHardDrive(hd)) = node.as_enum()
            && let uefi::proto::device_path::media::PartitionSignature::Guid(g) =
                hd.partition_signature()
        {
            return Some(g);
        }
    }
    None
}

pub fn boot_disk_prefix() -> Option<Vec<u8>> {
    let li = crate::open_shared::<LoadedImage>(boot::image_handle())?;
    disk_prefix_of(li.device()?)
}

#[derive(Debug)]
pub struct PartitionDevice {
    bio: ScopedProtocol<BlockIO>,
    media_id: u32,
    block_size: usize,
    blocks: u64,
    align: usize,
}

pub fn find_data_partition() -> Result<PartitionDevice, FindError> {
    let boot_disk = boot_disk_prefix().ok_or(FindError::NoBootDevice)?;
    let handles = boot::find_handles::<BlockIO>().map_err(|_| FindError::NotFound)?;
    for h in handles {
        if disk_prefix_of(h).as_deref() != Some(boot_disk.as_slice()) {
            continue;
        }
        let is_ours = match crate::open_shared::<PartitionInfo>(h) {
            Some(pi) => pi.gpt_partition_entry().is_some_and(|e| {
                let t = { e.partition_type_guid };
                t.0 == POS_DATA_TYPE
            }),
            None => hd_signature(h) == Some(POS_DATA_UNIQUE),
        };
        if !is_ours {
            continue;
        }
        let bio = boot::open_protocol_exclusive::<BlockIO>(h).map_err(|_| FindError::OpenFailed)?;
        let m = bio.media();
        if !m.is_media_present() || m.is_read_only() {
            return Err(FindError::OpenFailed);
        }
        return Ok(PartitionDevice {
            media_id: m.media_id(),
            block_size: m.block_size() as usize,
            blocks: m.last_block() + 1,
            align: (m.io_align() as usize).max(1),
            bio,
        });
    }
    Err(FindError::NotFound)
}

impl PartitionDevice {
    fn check(&self, lba: u64, len: usize) -> HalResult<()> {
        if len == 0 || !len.is_multiple_of(self.block_size) {
            return Err(HalError::InvalidArgument);
        }
        let n = (len / self.block_size) as u64;
        if lba.checked_add(n).is_none_or(|e| e > self.blocks) {
            return Err(HalError::InvalidArgument);
        }
        Ok(())
    }
    fn aligned(&self, p: *const u8) -> bool {
        (p as usize).is_multiple_of(self.align)
    }
}

/// Heap buffer with the BlockIO alignment the controller requires.
struct Bounce {
    buf: Vec<u8>,
    off: usize,
    len: usize,
}
impl Bounce {
    fn new(len: usize, align: usize) -> Bounce {
        let buf = vec![0u8; len + align];
        let off = buf.as_ptr().align_offset(align);
        Bounce { buf, off, len }
    }
    fn slice(&mut self) -> &mut [u8] {
        &mut self.buf[self.off..self.off + self.len]
    }
}

fn map(e: uefi::Status) -> HalError {
    match e {
        uefi::Status::WRITE_PROTECTED => HalError::WriteProtected,
        uefi::Status::MEDIA_CHANGED | uefi::Status::NO_MEDIA => HalError::MediaChanged,
        uefi::Status::INVALID_PARAMETER | uefi::Status::BAD_BUFFER_SIZE => {
            HalError::InvalidArgument
        }
        _ => HalError::Io,
    }
}

impl pos_hal::BlockDevice for PartitionDevice {
    fn block_size(&self) -> usize {
        self.block_size
    }
    fn block_count(&self) -> u64 {
        self.blocks
    }
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> HalResult<()> {
        self.check(lba, buf.len())?;
        if self.aligned(buf.as_ptr()) {
            self.bio
                .read_blocks(self.media_id, lba, buf)
                .map_err(|e| map(e.status()))
        } else {
            let mut b = Bounce::new(buf.len(), self.align);
            self.bio
                .read_blocks(self.media_id, lba, b.slice())
                .map_err(|e| map(e.status()))?;
            buf.copy_from_slice(b.slice());
            Ok(())
        }
    }
    fn write(&mut self, lba: u64, buf: &[u8]) -> HalResult<()> {
        self.check(lba, buf.len())?;
        if self.aligned(buf.as_ptr()) {
            self.bio
                .write_blocks(self.media_id, lba, buf)
                .map_err(|e| map(e.status()))
        } else {
            let mut b = Bounce::new(buf.len(), self.align);
            b.slice().copy_from_slice(buf);
            self.bio
                .write_blocks(self.media_id, lba, b.slice())
                .map_err(|e| map(e.status()))
        }
    }
    fn flush(&mut self) -> HalResult<()> {
        self.bio.flush_blocks().map_err(|e| map(e.status()))
    }
}
