//! Disk image construction: GPT + FAT32 ESP + raw POS data partition.
//! Pure Rust (gpt + fatfs crates); no root, no loop devices.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Cursor, Seek, SeekFrom, Write};
use std::path::Path;

use crate::Res;

pub const SECTOR: u64 = 512;
const MIB: u64 = 1024 * 1024;

/// GPT partition type GUID of the POS data partition. Randomly generated for
/// this project; firmware and OSes ignore unknown types, so the partition is
/// never auto-mounted or "repaired" by a desktop OS.
pub const POS_DATA_TYPE_GUID: &str = "138f131d-afdf-4718-a368-f3126495b979";

/// Fixed identifiers so the image is bit-for-bit reproducible. The *store*
/// identity is not these GUIDs; it is the random store id written into the
/// superblock on first boot (see pos-store).
const DISK_GUID: &str = "5d3e1a7c-0b9f-4c2e-8a61-7f4c2d9e0b13";
const ESP_PART_GUID: &str = "9a1c6e2f-3d4b-4f8a-b7c5-1e2d3f4a5b6c";
const DATA_PART_GUID: &str = "c4e8a2d6-7b1f-4e9c-a3d5-6f8b0c2e4a17";

#[derive(Debug, Clone, Copy)]
pub struct Layout {
    pub total_sectors: u64,
    pub esp_first: u64,
    pub esp_sectors: u64,
    pub data_first: u64,
    pub data_sectors: u64,
}

pub fn layout(total_mib: u64, esp_mib: u64) -> Layout {
    let total_sectors = total_mib * MIB / SECTOR;
    let esp_first = MIB / SECTOR; // 1 MiB alignment
    let esp_sectors = esp_mib * MIB / SECTOR;
    let data_first = esp_first + esp_sectors;
    // Leave room for the backup GPT (33 sectors) and keep 1 MiB alignment.
    let usable_end = total_sectors - 34;
    let data_sectors = (usable_end - data_first) / (MIB / SECTOR) * (MIB / SECTOR);
    Layout {
        total_sectors,
        esp_first,
        esp_sectors,
        data_first,
        data_sectors,
    }
}

#[derive(Debug)]
struct FixedTime(fatfs::DateTime);
impl fatfs::TimeProvider for FixedTime {
    fn get_current_date(&self) -> fatfs::Date {
        self.0.date
    }
    fn get_current_date_time(&self) -> fatfs::DateTime {
        self.0
    }
}

fn fixed_time(epoch: i64) -> &'static FixedTime {
    // FAT can't represent dates before 1980.
    let dt = pos_hal::DateTime::from_unix(epoch.max(315_532_800)).expect("valid epoch");
    Box::leak(Box::new(FixedTime(fatfs::DateTime {
        date: fatfs::Date {
            year: dt.year,
            month: u16::from(dt.month),
            day: u16::from(dt.day),
        },
        time: fatfs::Time {
            hour: u16::from(dt.hour),
            min: u16::from(dt.minute),
            sec: u16::from(dt.second & !1),
            millis: 0,
        },
    })))
}

/// Build a FAT32 volume image containing `files` (path inside volume, bytes).
pub fn build_fat(
    size_bytes: u64,
    label: &[u8; 11],
    volume_id: u32,
    files: &[(&str, &[u8])],
    epoch: i64,
) -> Res<Vec<u8>> {
    let mut buf = Cursor::new(vec![0u8; usize::try_from(size_bytes)?]);
    fatfs::format_volume(
        &mut buf,
        fatfs::FormatVolumeOptions::new()
            .fat_type(fatfs::FatType::Fat32)
            .volume_label(*label)
            .volume_id(volume_id)
            .bytes_per_cluster(512),
    )?;
    buf.seek(SeekFrom::Start(0))?;
    {
        let fs = fatfs::FileSystem::new(
            &mut buf,
            fatfs::FsOptions::new().time_provider(fixed_time(epoch)),
        )?;
        {
            let root = fs.root_dir();
            for (path, data) in files {
                let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
                let (name, dirs) = parts.split_last().ok_or("empty path")?;
                let mut dir = root.clone();
                for d in dirs {
                    dir = match dir.open_dir(d) {
                        Ok(x) => x,
                        Err(_) => dir.create_dir(d)?,
                    };
                }
                let mut f = dir.create_file(name)?;
                f.truncate()?;
                f.write_all(data)?;
                f.flush()?;
            }
        }
        fs.unmount()?;
    }
    Ok(buf.into_inner())
}

/// Create `out` with a protective MBR, GPT, ESP (with `efi` as
/// `\EFI\BOOT\BOOTX64.EFI`) and the data partition, whose first sectors are
/// initialised with `data_init` (pre-formatted store).
pub fn build_image(
    out: &Path,
    total_mib: u64,
    esp_mib: u64,
    efi: &[u8],
    data_init: &[u8],
    epoch: i64,
) -> Res<Layout> {
    let l = layout(total_mib, esp_mib);
    if out.exists() {
        fs::remove_file(out)?;
    }
    let file = File::options()
        .create_new(true)
        .read(true)
        .write(true)
        .open(out)?;
    file.set_len(l.total_sectors * SECTOR)?;

    // Protective MBR.
    let mbr = gpt::mbr::ProtectiveMBR::with_lb_size(
        u32::try_from(l.total_sectors - 1).unwrap_or(u32::MAX),
    );
    let mut file = file;
    mbr.overwrite_lba0(&mut file)?;

    let mut disk = gpt::GptConfig::new()
        .writable(true)
        .logical_block_size(gpt::disk::LogicalBlockSize::Lb512)
        .create_from_device(file, Some(DISK_GUID.parse()?))?;

    let mut parts = BTreeMap::new();
    parts.insert(
        1,
        gpt::partition::Partition {
            part_type_guid: gpt::partition_types::EFI,
            part_guid: ESP_PART_GUID.parse()?,
            first_lba: l.esp_first,
            last_lba: l.esp_first + l.esp_sectors - 1,
            flags: 0,
            name: "EFI System".into(),
        },
    );
    parts.insert(
        2,
        gpt::partition::Partition {
            part_type_guid: gpt::partition_types::Type {
                guid: POS_DATA_TYPE_GUID.parse()?,
                os: gpt::partition_types::OperatingSystem::Custom("POS".into()),
            },
            part_guid: DATA_PART_GUID.parse()?,
            first_lba: l.data_first,
            last_lba: l.data_first + l.data_sectors - 1,
            flags: 0,
            name: "POS Data".into(),
        },
    );
    disk.update_partitions(parts)?;
    let mut file = disk.write()?;

    let esp = build_fat(
        l.esp_sectors * SECTOR,
        b"POS-ESP    ",
        0x504F_5345,
        &[("EFI/BOOT/BOOTX64.EFI", efi)],
        epoch,
    )?;
    file.seek(SeekFrom::Start(l.esp_first * SECTOR))?;
    file.write_all(&esp)?;

    if data_init.len() as u64 > l.data_sectors * SECTOR {
        return Err("data init larger than partition".into());
    }
    file.seek(SeekFrom::Start(l.data_first * SECTOR))?;
    file.write_all(data_init)?;
    file.sync_all()?;
    Ok(l)
}

/// A whole-disk GPT image with a single FAT32 "basic data" partition, used as
/// the second USB stick (export/import media) in QEMU.
pub fn build_media_image(
    out: &Path,
    total_mib: u64,
    files: &[(&str, &[u8])],
    epoch: i64,
) -> Res<()> {
    let total_sectors = total_mib * MIB / SECTOR;
    let first = MIB / SECTOR;
    let sectors = (total_sectors - 34 - first) / (MIB / SECTOR) * (MIB / SECTOR);
    if out.exists() {
        fs::remove_file(out)?;
    }
    let mut file = File::options()
        .create_new(true)
        .read(true)
        .write(true)
        .open(out)?;
    file.set_len(total_sectors * SECTOR)?;
    gpt::mbr::ProtectiveMBR::with_lb_size(u32::try_from(total_sectors - 1).unwrap_or(u32::MAX))
        .overwrite_lba0(&mut file)?;
    let mut disk = gpt::GptConfig::new()
        .writable(true)
        .logical_block_size(gpt::disk::LogicalBlockSize::Lb512)
        .create_from_device(file, Some("0f1e2d3c-4b5a-4968-8776-a5b4c3d2e1f0".parse()?))?;
    let mut parts = BTreeMap::new();
    parts.insert(
        1,
        gpt::partition::Partition {
            part_type_guid: gpt::partition_types::BASIC,
            part_guid: "1a2b3c4d-5e6f-4a1b-9c2d-3e4f5a6b7c8d".parse()?,
            first_lba: first,
            last_lba: first + sectors - 1,
            flags: 0,
            name: "EXPORT".into(),
        },
    );
    disk.update_partitions(parts)?;
    let mut file = disk.write()?;
    let fat = build_fat(sectors * SECTOR, b"POS-EXPORT ", 0x4558_5054, files, epoch)?;
    file.seek(SeekFrom::Start(first * SECTOR))?;
    file.write_all(&fat)?;
    file.sync_all()?;
    Ok(())
}

/// Read a file back out of the FAT partition of a media image (for tests).
#[allow(dead_code)] // used by the M5 end-to-end test
pub fn read_media_file(img: &Path, name: &str) -> Res<Option<Vec<u8>>> {
    let data = fs::read(img)?;
    let start = usize::try_from(MIB)?;
    let total_sectors = data.len() as u64 / SECTOR;
    let sectors = (total_sectors - 34 - MIB / SECTOR) / (MIB / SECTOR) * (MIB / SECTOR);
    let end = start + usize::try_from(sectors * SECTOR)?;
    let mut cur = Cursor::new(data[start..end].to_vec());
    let fs = fatfs::FileSystem::new(&mut cur, fatfs::FsOptions::new())?;
    let root = fs.root_dir();
    let res = match root.open_file(name) {
        Ok(mut f) => {
            let mut v = Vec::new();
            std::io::Read::read_to_end(&mut f, &mut v)?;
            Some(v)
        }
        Err(_) => None,
    };
    Ok(res)
}
