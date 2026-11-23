//! USB Mass Storage Bulk-Only Transport framing and the SCSI commands a
//! block device needs.

pub const CBW_LEN: usize = 31;
pub const CSW_LEN: usize = 13;
const CBW_SIG: u32 = 0x4342_5355;
const CSW_SIG: u32 = 0x5342_5355;

/// MSC class: interface subclass SCSI transparent, protocol BOT.
pub const SUBCLASS_SCSI: u8 = 0x06;
pub const PROTOCOL_BOT: u8 = 0x50;
/// Class request: Bulk-Only Mass Storage Reset.
pub const REQ_BOT_RESET: u8 = 0xff;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    In,
    Out,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Command {
    pub cdb: [u8; 16],
    pub cdb_len: u8,
    pub dir: Dir,
    pub data_len: u32,
}

pub fn cbw(tag: u32, lun: u8, c: &Command) -> [u8; CBW_LEN] {
    let mut b = [0u8; CBW_LEN];
    b[0..4].copy_from_slice(&CBW_SIG.to_le_bytes());
    b[4..8].copy_from_slice(&tag.to_le_bytes());
    b[8..12].copy_from_slice(&c.data_len.to_le_bytes());
    b[12] = if c.dir == Dir::In { 0x80 } else { 0 };
    b[13] = lun & 0x0f;
    b[14] = c.cdb_len;
    b[15..31].copy_from_slice(&c.cdb);
    b
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CswStatus {
    Passed,
    Failed,
    PhaseError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Csw {
    pub residue: u32,
    pub status: CswStatus,
}

/// Validate a CSW against the CBW tag. `None` = invalid (reset recovery).
pub fn parse_csw(b: &[u8], tag: u32) -> Option<Csw> {
    if b.len() != CSW_LEN || b[0..4] != CSW_SIG.to_le_bytes() || b[4..8] != tag.to_le_bytes() {
        return None;
    }
    let status = match b[12] {
        0 => CswStatus::Passed,
        1 => CswStatus::Failed,
        2 => CswStatus::PhaseError,
        _ => return None,
    };
    Some(Csw {
        residue: u32::from_le_bytes([b[8], b[9], b[10], b[11]]),
        status,
    })
}

fn cmd(bytes: &[u8], dir: Dir, data_len: u32) -> Command {
    let mut cdb = [0u8; 16];
    cdb[..bytes.len()].copy_from_slice(bytes);
    Command {
        cdb,
        cdb_len: bytes.len() as u8,
        dir,
        data_len,
    }
}

pub fn test_unit_ready() -> Command {
    cmd(&[0x00, 0, 0, 0, 0, 0], Dir::None, 0)
}

pub fn request_sense() -> Command {
    cmd(&[0x03, 0, 0, 0, 18, 0], Dir::In, 18)
}

pub fn inquiry() -> Command {
    cmd(&[0x12, 0, 0, 0, 36, 0], Dir::In, 36)
}

pub fn read_capacity10() -> Command {
    cmd(&[0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0], Dir::In, 8)
}

pub fn read_capacity16() -> Command {
    cmd(
        &[0x9e, 0x10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 32, 0, 0],
        Dir::In,
        32,
    )
}

/// READ/WRITE(10) when the LBA fits in 32 bits, else (16).
pub fn rw(write: bool, lba: u64, blocks: u16, block_size: u32) -> Command {
    let len = u32::from(blocks) * block_size;
    let dir = if write { Dir::Out } else { Dir::In };
    let [b0, b1] = blocks.to_be_bytes();
    match u32::try_from(lba) {
        Ok(l) => {
            let [l0, l1, l2, l3] = l.to_be_bytes();
            let op = if write { 0x2a } else { 0x28 };
            cmd(&[op, 0, l0, l1, l2, l3, 0, b0, b1, 0], dir, len)
        }
        Err(_) => {
            let l = lba.to_be_bytes();
            let op = if write { 0x8a } else { 0x88 };
            cmd(
                &[
                    op, 0, l[0], l[1], l[2], l[3], l[4], l[5], l[6], l[7], 0, 0, b0, b1, 0, 0,
                ],
                dir,
                len,
            )
        }
    }
}

/// SYNCHRONIZE CACHE(10) over the whole medium.
pub fn synchronize_cache() -> Command {
    cmd(&[0x35, 0, 0, 0, 0, 0, 0, 0, 0, 0], Dir::None, 0)
}

/// (block count, block size) from READ CAPACITY(10). A last-LBA of
/// 0xFFFFFFFF means "use READ CAPACITY(16)" and returns `None`.
pub fn parse_capacity10(b: &[u8]) -> Option<(u64, u32)> {
    if b.len() < 8 {
        return None;
    }
    let last = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    let bs = u32::from_be_bytes([b[4], b[5], b[6], b[7]]);
    (last != u32::MAX && bs != 0).then_some((u64::from(last) + 1, bs))
}

pub fn parse_capacity16(b: &[u8]) -> Option<(u64, u32)> {
    if b.len() < 12 {
        return None;
    }
    let mut l = [0u8; 8];
    l.copy_from_slice(&b[0..8]);
    let last = u64::from_be_bytes(l);
    let bs = u32::from_be_bytes([b[8], b[9], b[10], b[11]]);
    (bs != 0).then_some((last.checked_add(1)?, bs))
}

/// Sense key from fixed-format REQUEST SENSE data.
pub fn sense_key(b: &[u8]) -> Option<u8> {
    (b.len() >= 3 && matches!(b[0] & 0x7f, 0x70 | 0x71)).then_some(b[2] & 0x0f)
}

pub const SENSE_NOT_READY: u8 = 0x02;
pub const SENSE_DATA_PROTECT: u8 = 0x07;
pub const SENSE_UNIT_ATTENTION: u8 = 0x06;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cbw_layout() {
        let b = cbw(7, 0, &rw(false, 0x1234, 8, 512));
        assert_eq!(&b[0..4], b"USBC");
        assert_eq!(u32::from_le_bytes([b[8], b[9], b[10], b[11]]), 4096);
        assert_eq!(b[12], 0x80);
        assert_eq!(b[14], 10);
        assert_eq!(&b[15..25], &[0x28, 0, 0, 0, 0x12, 0x34, 0, 0, 8, 0]);
    }

    #[test]
    fn rw16_for_large_lba() {
        let c = rw(true, 0x1_0000_0000, 1, 512);
        assert_eq!((c.cdb[0], c.cdb_len, c.dir), (0x8a, 16, Dir::Out));
        assert_eq!(&c.cdb[2..10], &[0, 0, 0, 1, 0, 0, 0, 0]);
    }

    #[test]
    fn csw_checks_signature_and_tag() {
        let mut b = [0u8; 13];
        b[0..4].copy_from_slice(b"USBS");
        b[4..8].copy_from_slice(&9u32.to_le_bytes());
        assert_eq!(parse_csw(&b, 9).unwrap().status, CswStatus::Passed);
        assert!(parse_csw(&b, 8).is_none());
        b[12] = 5;
        assert!(parse_csw(&b, 9).is_none());
    }

    #[test]
    fn capacity() {
        assert_eq!(
            parse_capacity10(&[0, 0, 0x0f, 0xff, 0, 0, 2, 0]),
            Some((4096, 512))
        );
        assert_eq!(parse_capacity10(&[0xff; 8]), None);
    }
}
