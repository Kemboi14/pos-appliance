//! In-memory block device (host tests, image building).

use alloc::vec;
use alloc::vec::Vec;
use pos_hal::{BlockDevice, HalError, HalResult};

#[derive(Debug, Clone)]
pub struct MemDevice {
    pub block_size: usize,
    pub data: Vec<u8>,
}

impl MemDevice {
    pub fn new(block_size: usize, blocks: u64) -> Self {
        MemDevice {
            block_size,
            data: vec![0u8; block_size * blocks as usize],
        }
    }
    fn range(&self, lba: u64, len: usize) -> HalResult<core::ops::Range<usize>> {
        if len == 0 || !len.is_multiple_of(self.block_size) {
            return Err(HalError::InvalidArgument);
        }
        let start = usize::try_from(lba)
            .ok()
            .and_then(|l| l.checked_mul(self.block_size))
            .ok_or(HalError::InvalidArgument)?;
        let end = start.checked_add(len).ok_or(HalError::InvalidArgument)?;
        if end > self.data.len() {
            return Err(HalError::InvalidArgument);
        }
        Ok(start..end)
    }
}

impl BlockDevice for MemDevice {
    fn block_size(&self) -> usize {
        self.block_size
    }
    fn block_count(&self) -> u64 {
        (self.data.len() / self.block_size) as u64
    }
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> HalResult<()> {
        let r = self.range(lba, buf.len())?;
        buf.copy_from_slice(&self.data[r]);
        Ok(())
    }
    fn write(&mut self, lba: u64, buf: &[u8]) -> HalResult<()> {
        let r = self.range(lba, buf.len())?;
        self.data[r].copy_from_slice(buf);
        Ok(())
    }
    fn flush(&mut self) -> HalResult<()> {
        Ok(())
    }
}
