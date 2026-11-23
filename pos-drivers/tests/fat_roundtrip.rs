//! FAT driver against volumes formatted and read back by the `fatfs` crate
//! (the same library xtask uses to build images), plus `fsck.fat` when it is
//! on PATH (it is in the Nix dev shell).

use pos_drivers::fat::{Fat, Kind};
use pos_hal::{BlockDevice, DateTime, HalError, HalResult};
use std::io::{Cursor, Read, Write};

struct Mem(Vec<u8>);

impl BlockDevice for Mem {
    fn block_size(&self) -> usize {
        512
    }
    fn block_count(&self) -> u64 {
        (self.0.len() / 512) as u64
    }
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> HalResult<()> {
        let s = lba as usize * 512;
        buf.copy_from_slice(
            self.0
                .get(s..s + buf.len())
                .ok_or(HalError::InvalidArgument)?,
        );
        Ok(())
    }
    fn write(&mut self, lba: u64, buf: &[u8]) -> HalResult<()> {
        let s = lba as usize * 512;
        self.0
            .get_mut(s..s + buf.len())
            .ok_or(HalError::InvalidArgument)?
            .copy_from_slice(buf);
        Ok(())
    }
    fn flush(&mut self) -> HalResult<()> {
        Ok(())
    }
}

fn volume(kind: fatfs::FatType, mib: usize, files: &[(&str, &[u8])]) -> Mem {
    let mut c = Cursor::new(vec![0u8; mib << 20]);
    fatfs::format_volume(
        &mut c,
        fatfs::FormatVolumeOptions::new()
            .fat_type(kind)
            .bytes_per_cluster(if kind == fatfs::FatType::Fat32 {
                512
            } else {
                2048
            }),
    )
    .unwrap();
    {
        let fs = fatfs::FileSystem::new(&mut c, fatfs::FsOptions::new()).unwrap();
        for (n, d) in files {
            let mut f = fs.root_dir().create_file(n).unwrap();
            f.write_all(d).unwrap();
        }
    }
    Mem(c.into_inner())
}

fn host_read(m: &Mem, name: &str) -> Option<Vec<u8>> {
    let mut c = Cursor::new(m.0.clone());
    let fs = fatfs::FileSystem::new(&mut c, fatfs::FsOptions::new()).unwrap();
    let mut f = fs.root_dir().open_file(name).ok()?;
    let mut v = Vec::new();
    f.read_to_end(&mut v).unwrap();
    Some(v)
}

fn fsck(m: &Mem) {
    let Ok(dir) = std::env::var("CARGO_TARGET_TMPDIR") else {
        return;
    };
    let p = std::path::Path::new(&dir).join(format!("fsck-{}.img", std::process::id()));
    std::fs::write(&p, &m.0).unwrap();
    match std::process::Command::new("fsck.fat")
        .arg("-n")
        .arg(&p)
        .output()
    {
        Ok(o) => assert!(
            o.status.success(),
            "fsck.fat: {}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        ),
        Err(_) => eprintln!("fsck.fat not on PATH; skipped"),
    }
    let _ = std::fs::remove_file(p);
}

fn pattern(n: usize, seed: u8) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

const WHEN: DateTime = DateTime {
    year: 2026,
    month: 11,
    day: 23,
    hour: 14,
    minute: 5,
    second: 30,
};

#[test]
fn reads_files_written_by_fatfs() {
    let big = pattern(100_000, 3);
    for kind in [fatfs::FatType::Fat16, fatfs::FatType::Fat32] {
        let mut m = volume(
            kind,
            if kind == fatfs::FatType::Fat32 {
                64
            } else {
                16
            },
            &[
                ("CATALOG.CSV", b"sku,name\n1,x\n"),
                ("Long Name File.txt", &big),
            ],
        );
        let mut fs = Fat::mount(&mut m).unwrap();
        assert_eq!(
            fs.kind,
            if kind == fatfs::FatType::Fat32 {
                Kind::Fat32
            } else {
                Kind::Fat16
            }
        );
        assert_eq!(
            fs.read_file("catalog.csv", 1 << 20).unwrap(),
            b"sku,name\n1,x\n"
        );
        assert_eq!(fs.read_file("LONG NAME FILE.TXT", 1 << 20).unwrap(), big);
        assert_eq!(fs.read_file("CATALOG.CSV", 4), Err(HalError::NoSpace));
        assert_eq!(fs.read_file("MISSING.CSV", 10), Err(HalError::NotFound));
    }
}

#[test]
fn writes_are_readable_by_fatfs_and_fsck_clean() {
    for kind in [fatfs::FatType::Fat16, fatfs::FatType::Fat32] {
        let mut m = volume(
            kind,
            if kind == fatfs::FatType::Fat32 {
                64
            } else {
                16
            },
            &[("CATALOG.CSV", b"old")],
        );
        let a = pattern(5000, 1);
        let b = pattern(70_000, 2);
        {
            let mut fs = Fat::mount(&mut m).unwrap();
            fs.write_file("Z000001.CSV", &a, Some(WHEN)).unwrap();
            fs.write_file("J00000001.CSV", &b, Some(WHEN)).unwrap();
            fs.write_file("EMPTY.TXT", &[], None).unwrap();
            // Replace an existing file with a longer one, then a shorter one.
            fs.write_file("CATALOG.CSV", &b, Some(WHEN)).unwrap();
            fs.write_file("CATALOG.CSV", b"new", Some(WHEN)).unwrap();
            // Same long name twice: replaced, not duplicated.
            fs.write_file("J00000001.CSV", &a, Some(WHEN)).unwrap();
            assert_eq!(fs.read_file("J00000001.CSV", 1 << 20).unwrap(), a);
        }
        assert_eq!(host_read(&m, "Z000001.CSV").unwrap(), a);
        assert_eq!(host_read(&m, "J00000001.CSV").unwrap(), a);
        assert_eq!(host_read(&m, "CATALOG.CSV").unwrap(), b"new");
        assert_eq!(host_read(&m, "EMPTY.TXT").unwrap(), b"");
        fsck(&m);
    }
}

#[test]
fn fat32_root_grows_and_aliases_stay_unique() {
    let mut m = volume(fatfs::FatType::Fat32, 64, &[]);
    {
        let mut fs = Fat::mount(&mut m).unwrap();
        // 512-byte clusters hold 16 entries; 60 long names need many.
        for i in 0..60u32 {
            let name = format!("J{:08}.CSV", i + 1);
            fs.write_file(&name, name.as_bytes(), Some(WHEN)).unwrap();
        }
    }
    for i in 0..60u32 {
        let name = format!("J{:08}.CSV", i + 1);
        assert_eq!(host_read(&m, &name).unwrap(), name.as_bytes());
    }
    fsck(&m);
}

#[test]
fn full_volume_reports_no_space_and_stays_consistent() {
    let mut m = volume(fatfs::FatType::Fat16, 16, &[]);
    {
        let mut fs = Fat::mount(&mut m).unwrap();
        let huge = vec![7u8; 20 << 20];
        assert_eq!(
            fs.write_file("HUGE.BIN", &huge, None),
            Err(HalError::NoSpace)
        );
        fs.write_file("SMALL.TXT", b"ok", None).unwrap();
    }
    assert_eq!(host_read(&m, "SMALL.TXT").unwrap(), b"ok");
    assert!(host_read(&m, "HUGE.BIN").is_none());
    fsck(&m);
}

#[test]
fn rejects_non_fat() {
    let mut m = Mem(vec![0u8; 1 << 20]);
    assert!(Fat::mount(&mut m).is_err());
}
