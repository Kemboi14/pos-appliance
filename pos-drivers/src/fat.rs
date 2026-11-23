//! Minimal FAT16/FAT32 filesystem: files in the root directory only, which
//! is all `pos_hal::ExternalMedia` needs (CSV export, catalog/settings
//! import). Long file names are read and written (`J00000001.CSV` is not
//! 8.3). FAT12 and exFAT are rejected: the operations guide says to format
//! the export stick FAT32.
//!
//! Writes are not crash-safe beyond what FAT allows; the export stick is a
//! transfer medium, the journal of record is the POS store.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use pos_hal::{BlockDevice, DateTime, HalError, HalResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Fat16,
    Fat32,
}

const ATTR_READ_ONLY: u8 = 0x01;
const ATTR_VOLUME: u8 = 0x08;
const ATTR_DIR: u8 = 0x10;
const ATTR_ARCHIVE: u8 = 0x20;
const ATTR_LFN: u8 = 0x0f;

pub struct Fat<'a> {
    dev: &'a mut dyn BlockDevice,
    pub kind: Kind,
    bps: usize,
    spc: u64,
    fat_start: u64,
    fat_sectors: u64,
    nfats: u64,
    root_start: u64,
    root_sectors: u64,
    data_start: u64,
    clusters: u32,
    root_cluster: u32,
    fsinfo: u64,
    fsinfo_invalidated: bool,
    cache: Option<(u64, Vec<u8>)>,
}

impl core::fmt::Debug for Fat<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Fat")
            .field("kind", &self.kind)
            .field("clusters", &self.clusters)
            .finish_non_exhaustive()
    }
}

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

#[derive(Debug, Clone)]
struct Found {
    /// Index of the first entry (LFN or SFN) and of the SFN entry.
    first: usize,
    sfn: usize,
    cluster: u32,
    size: u32,
    read_only: bool,
}

impl<'a> Fat<'a> {
    /// Mount the volume at LBA 0 of `dev` (wrap a partition in
    /// [`crate::part::Slice`]).
    pub fn mount(dev: &'a mut dyn BlockDevice) -> HalResult<Fat<'a>> {
        let bs = dev.block_size();
        let mut b = vec![0u8; bs];
        dev.read(0, &mut b)?;
        let bps = usize::from(le16(&b, 11));
        let spc = u64::from(b[13]);
        let reserved = u64::from(le16(&b, 14));
        let nfats = u64::from(b[16]);
        let root_entries = u64::from(le16(&b, 17));
        let total16 = u64::from(le16(&b, 19));
        let fat16_size = u64::from(le16(&b, 22));
        let total32 = u64::from(le32(&b, 32));
        let bad = !(b[0] == 0xeb || b[0] == 0xe9)
            || b[510] != 0x55
            || b[511] != 0xaa
            || bps != bs
            || spc == 0
            || !spc.is_power_of_two()
            || reserved == 0
            || nfats == 0;
        if bad {
            return Err(HalError::NotFound);
        }
        let fat_sectors = if fat16_size != 0 {
            fat16_size
        } else {
            u64::from(le32(&b, 36))
        };
        let total = if total16 != 0 { total16 } else { total32 };
        let root_sectors = (root_entries * 32).div_ceil(bps as u64);
        let fat_start = reserved;
        let root_start = fat_start + nfats * fat_sectors;
        let data_start = root_start + root_sectors;
        if total <= data_start || total > dev.block_count() {
            return Err(HalError::NotFound);
        }
        let clusters = (total - data_start) / spc;
        let kind = if clusters < 4085 {
            return Err(HalError::NotFound); // FAT12
        } else if clusters < 65525 {
            Kind::Fat16
        } else {
            Kind::Fat32
        };
        let (root_cluster, fsinfo) = match kind {
            Kind::Fat32 => (le32(&b, 44), u64::from(le16(&b, 48))),
            Kind::Fat16 => (0, 0),
        };
        // The FAT must be able to address every cluster.
        let per_fat = fat_sectors * bps as u64 / if kind == Kind::Fat16 { 2 } else { 4 };
        if per_fat < clusters + 2 {
            return Err(HalError::NotFound);
        }
        Ok(Fat {
            dev,
            kind,
            bps,
            spc,
            fat_start,
            fat_sectors,
            nfats,
            root_start,
            root_sectors,
            data_start,
            clusters: clusters as u32,
            root_cluster,
            fsinfo,
            fsinfo_invalidated: false,
            cache: None,
        })
    }

    fn cluster_bytes(&self) -> usize {
        self.bps * self.spc as usize
    }

    fn cluster_lba(&self, c: u32) -> u64 {
        self.data_start + u64::from(c - 2) * self.spc
    }

    fn valid_cluster(&self, c: u32) -> bool {
        c >= 2 && c < self.clusters + 2
    }

    fn eoc(&self) -> u32 {
        match self.kind {
            Kind::Fat16 => 0xffff,
            Kind::Fat32 => 0x0fff_ffff,
        }
    }

    fn is_eoc(&self, v: u32) -> bool {
        match self.kind {
            Kind::Fat16 => v >= 0xfff8,
            Kind::Fat32 => v >= 0x0fff_fff8,
        }
    }

    fn read_sector(&mut self, lba: u64) -> HalResult<Vec<u8>> {
        if let Some((l, d)) = &self.cache
            && *l == lba
        {
            return Ok(d.clone());
        }
        let mut b = vec![0u8; self.bps];
        self.dev.read(lba, &mut b)?;
        self.cache = Some((lba, b.clone()));
        Ok(b)
    }

    fn write_sector(&mut self, lba: u64, data: &[u8]) -> HalResult<()> {
        self.dev.write(lba, data)?;
        if let Some((l, d)) = &mut self.cache
            && *l == lba
        {
            d.copy_from_slice(data);
        }
        Ok(())
    }

    fn fat_pos(&self, c: u32) -> (u64, usize) {
        let off = match self.kind {
            Kind::Fat16 => c as usize * 2,
            Kind::Fat32 => c as usize * 4,
        };
        (self.fat_start + (off / self.bps) as u64, off % self.bps)
    }

    fn get(&mut self, c: u32) -> HalResult<u32> {
        let (lba, o) = self.fat_pos(c);
        let s = self.read_sector(lba)?;
        Ok(match self.kind {
            Kind::Fat16 => u32::from(le16(&s, o)),
            Kind::Fat32 => le32(&s, o) & 0x0fff_ffff,
        })
    }

    fn set(&mut self, c: u32, v: u32) -> HalResult<()> {
        let (lba, o) = self.fat_pos(c);
        let mut s = self.read_sector(lba)?;
        match self.kind {
            Kind::Fat16 => s[o..o + 2].copy_from_slice(&(v as u16).to_le_bytes()),
            Kind::Fat32 => {
                let keep = le32(&s, o) & 0xf000_0000;
                s[o..o + 4].copy_from_slice(&(keep | (v & 0x0fff_ffff)).to_le_bytes());
            }
        }
        for i in 0..self.nfats {
            self.write_sector(lba + i * self.fat_sectors, &s)?;
        }
        Ok(())
    }

    /// The cluster chain starting at `c` (cycle- and range-checked).
    fn chain(&mut self, mut c: u32) -> HalResult<Vec<u32>> {
        let mut v = Vec::new();
        while self.valid_cluster(c) {
            if v.len() > self.clusters as usize {
                return Err(HalError::Io);
            }
            v.push(c);
            c = self.get(c)?;
        }
        if c != 0 && !self.is_eoc(c) && !v.is_empty() {
            return Err(HalError::Io);
        }
        Ok(v)
    }

    fn free_chain(&mut self, c: u32) -> HalResult<()> {
        for x in self.chain(c)? {
            self.set(x, 0)?;
        }
        Ok(())
    }

    fn invalidate_fsinfo(&mut self) -> HalResult<()> {
        if self.kind != Kind::Fat32 || self.fsinfo_invalidated || self.fsinfo == 0 {
            return Ok(());
        }
        self.fsinfo_invalidated = true;
        let mut s = self.read_sector(self.fsinfo)?;
        if le32(&s, 0) == 0x4161_5252 && le32(&s, 484) == 0x6141_7272 {
            s[488..496].fill(0xff); // free count and next-free hint unknown
            self.write_sector(self.fsinfo, &s)?;
        }
        Ok(())
    }

    /// Allocate `n` clusters as one chain; returns them in order.
    fn alloc(&mut self, n: usize) -> HalResult<Vec<u32>> {
        self.invalidate_fsinfo()?;
        let mut got: Vec<u32> = Vec::with_capacity(n);
        let mut c = 2;
        while got.len() < n {
            if c >= self.clusters + 2 {
                for &x in &got {
                    self.set(x, 0)?;
                }
                return Err(HalError::NoSpace);
            }
            if self.get(c)? == 0 {
                // Mark immediately so the scan never hands it out twice.
                self.set(c, self.eoc())?;
                if let Some(&prev) = got.last() {
                    self.set(prev, c)?;
                }
                got.push(c);
            }
            c += 1;
        }
        Ok(got)
    }

    fn root_sectors_list(&mut self) -> HalResult<Vec<u64>> {
        match self.kind {
            Kind::Fat16 => Ok((0..self.root_sectors)
                .map(|i| self.root_start + i)
                .collect()),
            Kind::Fat32 => {
                let chain = self.chain(self.root_cluster)?;
                Ok(chain
                    .iter()
                    .flat_map(|&c| {
                        let l = self.cluster_lba(c);
                        (0..self.spc).map(move |i| l + i)
                    })
                    .collect())
            }
        }
    }

    fn entry(&mut self, secs: &[u64], i: usize) -> HalResult<[u8; 32]> {
        let per = self.bps / 32;
        let s = self.read_sector(secs[i / per])?;
        let o = (i % per) * 32;
        let mut e = [0u8; 32];
        e.copy_from_slice(&s[o..o + 32]);
        Ok(e)
    }

    fn put_entry(&mut self, secs: &[u64], i: usize, e: &[u8; 32]) -> HalResult<()> {
        let per = self.bps / 32;
        let lba = secs[i / per];
        let mut s = self.read_sector(lba)?;
        let o = (i % per) * 32;
        s[o..o + 32].copy_from_slice(e);
        self.write_sector(lba, &s)
    }

    fn find(&mut self, secs: &[u64], name: &str) -> HalResult<Option<Found>> {
        let total = secs.len() * (self.bps / 32);
        let mut lfn: Vec<u16> = Vec::new();
        let mut lfn_start: Option<usize> = None;
        let mut lfn_sum = 0u8;
        for i in 0..total {
            let e = self.entry(secs, i)?;
            if e[0] == 0 {
                break;
            }
            if e[0] == 0xe5 {
                lfn_start = None;
                continue;
            }
            if e[11] == ATTR_LFN {
                let ord = usize::from(e[0] & 0x3f);
                if e[0] & 0x40 != 0 {
                    lfn = vec![0xffff; ord * 13];
                    lfn_start = Some(i);
                    lfn_sum = e[13];
                }
                if ord == 0 || ord * 13 > lfn.len() || e[13] != lfn_sum {
                    lfn_start = None;
                    continue;
                }
                let base = (ord - 1) * 13;
                for (k, off) in LFN_OFFSETS.iter().enumerate() {
                    lfn[base + k] = le16(&e, *off);
                }
                continue;
            }
            let mut sfn = [0u8; 11];
            sfn.copy_from_slice(&e[0..11]);
            let long = match lfn_start {
                Some(_) if checksum(&sfn) == lfn_sum => {
                    let end = lfn
                        .iter()
                        .position(|&c| c == 0 || c == 0xffff)
                        .unwrap_or(lfn.len());
                    String::from_utf16(&lfn[..end]).ok()
                }
                _ => None,
            };
            let first = lfn_start.filter(|_| long.is_some()).unwrap_or(i);
            lfn_start = None;
            if e[11] & (ATTR_VOLUME | ATTR_DIR) != 0 {
                continue;
            }
            let short = sfn_display(&sfn);
            let hit = long
                .as_deref()
                .is_some_and(|l| l.eq_ignore_ascii_case(name))
                || short.eq_ignore_ascii_case(name);
            if hit {
                let cluster = u32::from(le16(&e, 26))
                    | if self.kind == Kind::Fat32 {
                        u32::from(le16(&e, 20)) << 16
                    } else {
                        0
                    };
                return Ok(Some(Found {
                    first,
                    sfn: i,
                    cluster,
                    size: le32(&e, 28),
                    read_only: e[11] & ATTR_READ_ONLY != 0,
                }));
            }
        }
        Ok(None)
    }

    /// Read `name` from the root directory. `NoSpace` if it exceeds `max_len`.
    pub fn read_file(&mut self, name: &str, max_len: usize) -> HalResult<Vec<u8>> {
        let secs = self.root_sectors_list()?;
        let f = self.find(&secs, name)?.ok_or(HalError::NotFound)?;
        let size = f.size as usize;
        if size > max_len {
            return Err(HalError::NoSpace);
        }
        let mut out = Vec::with_capacity(size);
        if size == 0 {
            return Ok(out);
        }
        let cb = self.cluster_bytes();
        let chain = self.chain(f.cluster)?;
        if chain.len() < size.div_ceil(cb) {
            return Err(HalError::Io);
        }
        let mut buf = vec![0u8; cb];
        for c in chain {
            if out.len() >= size {
                break;
            }
            self.dev.read(self.cluster_lba(c), &mut buf)?;
            let take = (size - out.len()).min(cb);
            out.extend_from_slice(&buf[..take]);
        }
        Ok(out)
    }

    /// Create or replace `name` in the root directory with `data`, then
    /// flush the device.
    pub fn write_file(&mut self, name: &str, data: &[u8], when: Option<DateTime>) -> HalResult<()> {
        let short = to_83(name);
        if name.is_empty() || name.len() > 255 || !name.chars().all(valid_long_char) {
            return Err(HalError::InvalidArgument);
        }
        let mut secs = self.root_sectors_list()?;
        if let Some(f) = self.find(&secs, name)? {
            if f.read_only {
                return Err(HalError::WriteProtected);
            }
            for i in f.first..=f.sfn {
                let mut e = self.entry(&secs, i)?;
                e[0] = 0xe5;
                self.put_entry(&secs, i, &e)?;
            }
            self.free_chain(f.cluster)?;
        }
        let sfn = match short {
            Some(s) => s,
            None => self.alias(&secs, name)?,
        };
        let lfn_entries = if short.is_some() {
            Vec::new()
        } else {
            lfn_entries(name, checksum(&sfn))
        };

        // Data first, so a failure leaves no dangling directory entry.
        let cb = self.cluster_bytes();
        let chain = self.alloc(data.len().div_ceil(cb))?;
        let mut buf = vec![0u8; cb];
        for (k, &c) in chain.iter().enumerate() {
            let part = &data[k * cb..((k + 1) * cb).min(data.len())];
            buf.fill(0);
            buf[..part.len()].copy_from_slice(part);
            self.dev.write(self.cluster_lba(c), &buf)?;
        }
        let first_cluster = chain.first().copied().unwrap_or(0);

        let need = lfn_entries.len() + 1;
        let slot = loop {
            if let Some(s) = self.free_run(&secs, need)? {
                break s;
            }
            if self.kind == Kind::Fat16 || !self.grow_root()? {
                if first_cluster != 0 {
                    self.free_chain(first_cluster)?;
                }
                return Err(HalError::NoSpace);
            }
            secs = self.root_sectors_list()?;
        };
        for (k, e) in lfn_entries.iter().enumerate() {
            self.put_entry(&secs, slot + k, e)?;
        }
        let mut e = [0u8; 32];
        e[0..11].copy_from_slice(&sfn);
        e[11] = ATTR_ARCHIVE;
        let (t, d) = when.map_or((0, 0x21), dos_time);
        e[14..16].copy_from_slice(&t.to_le_bytes());
        e[16..18].copy_from_slice(&d.to_le_bytes());
        e[18..20].copy_from_slice(&d.to_le_bytes());
        e[20..22].copy_from_slice(&((first_cluster >> 16) as u16).to_le_bytes());
        e[22..24].copy_from_slice(&t.to_le_bytes());
        e[24..26].copy_from_slice(&d.to_le_bytes());
        e[26..28].copy_from_slice(&(first_cluster as u16).to_le_bytes());
        e[28..32].copy_from_slice(&(data.len() as u32).to_le_bytes());
        self.put_entry(&secs, slot + lfn_entries.len(), &e)?;
        self.dev.flush()
    }

    /// First run of `n` free directory entries.
    fn free_run(&mut self, secs: &[u64], n: usize) -> HalResult<Option<usize>> {
        let total = secs.len() * (self.bps / 32);
        let mut run = 0;
        for i in 0..total {
            let e = self.entry(secs, i)?;
            if e[0] == 0 {
                // End marker: everything after it is free too.
                return Ok((total - i + run >= n).then(|| i - run));
            }
            if e[0] == 0xe5 {
                run += 1;
                if run == n {
                    return Ok(Some(i + 1 - n));
                }
            } else {
                run = 0;
            }
        }
        Ok(None)
    }

    /// Append a zeroed cluster to the FAT32 root directory.
    fn grow_root(&mut self) -> HalResult<bool> {
        let chain = self.chain(self.root_cluster)?;
        let Some(&last) = chain.last() else {
            return Ok(false);
        };
        let new = match self.alloc(1) {
            Ok(v) => v[0],
            Err(HalError::NoSpace) => return Ok(false),
            Err(e) => return Err(e),
        };
        let zero = vec![0u8; self.cluster_bytes()];
        self.dev.write(self.cluster_lba(new), &zero)?;
        self.cache = None;
        self.set(last, new)?;
        Ok(true)
    }

    /// A unique `BASE~N.EXT` short alias for a long name.
    fn alias(&mut self, secs: &[u64], name: &str) -> HalResult<[u8; 11]> {
        let (base, ext) = match name.rfind('.') {
            Some(p) if p > 0 => (&name[..p], &name[p + 1..]),
            _ => (name, ""),
        };
        let clean = |s: &str, n: usize| -> Vec<u8> {
            s.bytes()
                .filter(|b| b.is_ascii_alphanumeric() || b"_-$~!#%&@^'(){}".contains(b))
                .map(|b| b.to_ascii_uppercase())
                .take(n)
                .collect()
        };
        let b = clean(base, 8);
        let e = clean(ext, 3);
        let total = secs.len() * (self.bps / 32);
        let mut existing: Vec<[u8; 11]> = Vec::new();
        for i in 0..total {
            let en = self.entry(secs, i)?;
            if en[0] == 0 {
                break;
            }
            if en[0] != 0xe5 && en[11] != ATTR_LFN {
                let mut s = [0u8; 11];
                s.copy_from_slice(&en[0..11]);
                existing.push(s);
            }
        }
        for n in 1u32..1_000_000 {
            let tail = alloc::format!("~{n}");
            let keep = 8usize.saturating_sub(tail.len()).min(b.len());
            let mut s = [b' '; 11];
            s[..keep].copy_from_slice(&b[..keep]);
            s[keep..keep + tail.len()].copy_from_slice(tail.as_bytes());
            s[8..8 + e.len()].copy_from_slice(&e);
            if !existing.contains(&s) {
                return Ok(s);
            }
        }
        Err(HalError::NoSpace)
    }
}

const LFN_OFFSETS: [usize; 13] = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];

fn checksum(sfn: &[u8; 11]) -> u8 {
    sfn.iter()
        .fold(0u8, |s, &b| (s >> 1 | (s & 1) << 7).wrapping_add(b))
}

fn sfn_display(sfn: &[u8; 11]) -> String {
    let mut b: Vec<u8> = sfn[..8].to_vec();
    if b[0] == 0x05 {
        b[0] = 0xe5;
    }
    while b.last() == Some(&b' ') {
        b.pop();
    }
    let ext: Vec<u8> = sfn[8..].iter().copied().filter(|&c| c != b' ').collect();
    if !ext.is_empty() {
        b.push(b'.');
        b.extend_from_slice(&ext);
    }
    b.iter().map(|&c| char::from(c)).collect()
}

fn valid_long_char(c: char) -> bool {
    !(c.is_control() || "\\/:*?\"<>|".contains(c))
}

/// The 8.3 form of `name` if it is a plain uppercase-able 8.3 name.
fn to_83(name: &str) -> Option<[u8; 11]> {
    let (base, ext) = match name.split_once('.') {
        Some((b, e)) => (b, e),
        None => (name, ""),
    };
    let ok = |s: &str, max: usize| {
        s.len() <= max
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-$~!#%&@^'(){}".contains(&b))
    };
    if base.is_empty() || ext.contains('.') || !ok(base, 8) || !ok(ext, 3) {
        return None;
    }
    let mut s = [b' '; 11];
    for (i, b) in base.bytes().enumerate() {
        s[i] = b.to_ascii_uppercase();
    }
    for (i, b) in ext.bytes().enumerate() {
        s[8 + i] = b.to_ascii_uppercase();
    }
    Some(s)
}

fn lfn_entries(name: &str, sum: u8) -> Vec<[u8; 32]> {
    let mut units: Vec<u16> = name.encode_utf16().collect();
    if !units.len().is_multiple_of(13) {
        units.push(0);
        while !units.len().is_multiple_of(13) {
            units.push(0xffff);
        }
    }
    let n = units.len() / 13;
    // Entries are stored last-part first.
    (0..n)
        .rev()
        .map(|k| {
            let mut e = [0u8; 32];
            e[0] = (k as u8 + 1) | if k == n - 1 { 0x40 } else { 0 };
            e[11] = ATTR_LFN;
            e[13] = sum;
            for (j, off) in LFN_OFFSETS.iter().enumerate() {
                e[*off..*off + 2].copy_from_slice(&units[k * 13 + j].to_le_bytes());
            }
            e
        })
        .collect()
}

fn dos_time(t: DateTime) -> (u16, u16) {
    let year = t.year.clamp(1980, 2107) - 1980;
    (
        u16::from(t.hour) << 11 | u16::from(t.minute) << 5 | u16::from(t.second / 2),
        year << 9 | u16::from(t.month) << 5 | u16::from(t.day),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_names() {
        assert_eq!(&to_83("CATALOG.CSV").unwrap(), b"CATALOG CSV");
        assert_eq!(&to_83("z1.csv").unwrap(), b"Z1      CSV");
        assert_eq!(&to_83("README").unwrap(), b"README     ");
        assert!(to_83("J00000001.CSV").is_none());
        assert!(to_83("A.B.C").is_none());
        assert!(to_83("my file.txt").is_none());
    }

    #[test]
    fn lfn_layout() {
        let e = lfn_entries("J00000001.CSV", 0x42);
        assert_eq!(e.len(), 1); // exactly 13 units, no terminator needed
        assert_eq!(e[0][0], 0x41);
        let e = lfn_entries("J00000001.CSVX", 0x42);
        assert_eq!(e.len(), 2);
        assert_eq!((e[0][0], e[1][0]), (0x42, 0x01));
    }
}
