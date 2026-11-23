//! Partition discovery: GPT (with header CRC check) and MBR.

use crate::guid;
use alloc::vec;
use alloc::vec::Vec;
use pos_hal::{BlockDevice, HalError, HalResult};

pub const POS_DATA_TYPE: [u8; 16] = guid("138f131d-afdf-4718-a368-f3126495b979");
pub const ESP_TYPE: [u8; 16] = guid("C12A7328-F81F-11D2-BA4B-00A0C93EC93B");
pub const BASIC_DATA_TYPE: [u8; 16] = guid("EBD0A0A2-B9E5-4433-87C0-68B6B72699C7");

/// CRC-32 (IEEE 802.3, reflected), as used by GPT.
pub fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c ^= u32::from(b);
        for _ in 0..8 {
            c = if c & 1 != 0 {
                (c >> 1) ^ 0xedb8_8320
            } else {
                c >> 1
            };
        }
    }
    !c
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Partition {
    pub type_guid: [u8; 16],
    pub first_lba: u64,
    pub blocks: u64,
    /// MBR partition type byte (0 for GPT partitions).
    pub mbr_type: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    /// GPT disk GUID, or `None` for MBR/superfloppy media.
    pub disk_guid: Option<[u8; 16]>,
    pub partitions: Vec<Partition>,
}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn le64(b: &[u8], o: usize) -> u64 {
    u64::from(le32(b, o)) | u64::from(le32(b, o + 4)) << 32
}

/// Read the partition table. A GPT with a bad header or entry-array CRC is
/// rejected (we don't fall back to the backup header: the POS never writes
/// partition tables, so a damaged primary means damaged media).
pub fn read_table(dev: &mut dyn BlockDevice) -> HalResult<Table> {
    let bs = dev.block_size();
    if bs < 512 {
        return Err(HalError::InvalidArgument);
    }
    let mut lba0 = vec![0u8; bs];
    dev.read(0, &mut lba0)?;
    let mut lba1 = vec![0u8; bs];
    dev.read(1, &mut lba1)?;
    if &lba1[0..8] == b"EFI PART" {
        return parse_gpt(dev, &lba1);
    }
    if lba0[510] != 0x55 || lba0[511] != 0xaa {
        return Ok(Table {
            disk_guid: None,
            partitions: Vec::new(),
        });
    }
    let mut parts = Vec::new();
    for i in 0..4 {
        let e = &lba0[446 + i * 16..462 + i * 16];
        let kind = e[4];
        let first = u64::from(le32(e, 8));
        let count = u64::from(le32(e, 12));
        if kind != 0 && kind != 0xee && count > 0 {
            parts.push(Partition {
                type_guid: [0; 16],
                first_lba: first,
                blocks: count,
                mbr_type: kind,
            });
        }
    }
    Ok(Table {
        disk_guid: None,
        partitions: parts,
    })
}

fn parse_gpt(dev: &mut dyn BlockDevice, h: &[u8]) -> HalResult<Table> {
    let hsize = le32(h, 12) as usize;
    if !(92..=h.len()).contains(&hsize) {
        return Err(HalError::Io);
    }
    let mut hdr = h[..hsize].to_vec();
    hdr[16..20].fill(0);
    if crc32(&hdr) != le32(h, 16) {
        return Err(HalError::Io);
    }
    let mut disk_guid = [0u8; 16];
    disk_guid.copy_from_slice(&h[56..72]);
    let entries_lba = le64(h, 72);
    let count = le32(h, 80) as usize;
    let esize = le32(h, 84) as usize;
    if esize < 128 || count > 1024 {
        return Err(HalError::Io);
    }
    let bs = dev.block_size();
    let bytes = count * esize;
    let blocks = bytes.div_ceil(bs);
    let mut arr = vec![0u8; blocks * bs];
    dev.read(entries_lba, &mut arr)?;
    if crc32(&arr[..bytes]) != le32(h, 88) {
        return Err(HalError::Io);
    }
    let mut parts = Vec::new();
    for i in 0..count {
        let e = &arr[i * esize..(i + 1) * esize];
        let mut t = [0u8; 16];
        t.copy_from_slice(&e[0..16]);
        if t == [0; 16] {
            continue;
        }
        let first = le64(e, 32);
        let last = le64(e, 40);
        if last < first {
            continue;
        }
        parts.push(Partition {
            type_guid: t,
            first_lba: first,
            blocks: last - first + 1,
            mbr_type: 0,
        });
    }
    Ok(Table {
        disk_guid: Some(disk_guid),
        partitions: parts,
    })
}

/// A window onto `[first, first + blocks)` of a device.
#[derive(Debug)]
pub struct Slice<D> {
    pub dev: D,
    pub first: u64,
    pub blocks: u64,
}

impl<D: BlockDevice> Slice<D> {
    fn check(&self, lba: u64, len: usize) -> HalResult<u64> {
        let bs = self.dev.block_size();
        if len == 0 || !len.is_multiple_of(bs) {
            return Err(HalError::InvalidArgument);
        }
        let n = (len / bs) as u64;
        if lba.checked_add(n).is_none_or(|end| end > self.blocks) {
            return Err(HalError::InvalidArgument);
        }
        Ok(self.first + lba)
    }
}

impl<D: BlockDevice> BlockDevice for Slice<D> {
    fn block_size(&self) -> usize {
        self.dev.block_size()
    }
    fn block_count(&self) -> u64 {
        self.blocks
    }
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> HalResult<()> {
        let abs = self.check(lba, buf.len())?;
        self.dev.read(abs, buf)
    }
    fn write(&mut self, lba: u64, buf: &[u8]) -> HalResult<()> {
        let abs = self.check(lba, buf.len())?;
        self.dev.write(abs, buf)
    }
    fn flush(&mut self) -> HalResult<()> {
        self.dev.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_check_value() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }
}
