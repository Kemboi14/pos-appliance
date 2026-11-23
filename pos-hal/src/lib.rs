//! Hardware abstraction layer for the POS appliance.
//!
//! This crate only defines traits and plain data types. Tier A
//! (`platform-uefi`) implements them on UEFI boot services, Tier B (`kernel`)
//! on native drivers, and host tests on in-memory mocks. Domain, storage, UI
//! and ESC/POS crates depend on these traits only.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

/// Errors a HAL implementation can report. Kept deliberately coarse: callers
/// only need to decide between retry, degrade and fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HalError {
    /// The device or protocol does not exist on this machine.
    NotPresent,
    /// The device exists but reported an I/O failure.
    Io,
    /// The operation timed out.
    Timeout,
    /// A parameter was out of range (LBA past end, buffer length wrong, ...).
    InvalidArgument,
    /// The medium is write-protected.
    WriteProtected,
    /// The device has no space left (external media).
    NoSpace,
    /// The device was removed or changed underneath us.
    MediaChanged,
    /// Entropy source failed its health check.
    EntropyFailure,
    /// The file or object does not exist.
    NotFound,
}

impl fmt::Display for HalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            HalError::NotPresent => "device not present",
            HalError::Io => "I/O error",
            HalError::Timeout => "timeout",
            HalError::InvalidArgument => "invalid argument",
            HalError::WriteProtected => "write protected",
            HalError::NoSpace => "no space left",
            HalError::MediaChanged => "media changed",
            HalError::EntropyFailure => "entropy source failure",
            HalError::NotFound => "not found",
        };
        f.write_str(s)
    }
}

pub type HalResult<T> = Result<T, HalError>;

// ---------------------------------------------------------------- display

/// Native pixel layout of the scan-out buffer. The UI always renders in
/// [`Rgb888`] (`0x00RRGGBB` in a `u32`); the platform converts on blit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// Byte order R, G, B, reserved.
    Rgbx,
    /// Byte order B, G, R, reserved (the common case on PC firmware).
    Bgrx,
}

/// A pixel in canonical `0x00RRGGBB` form.
pub type Rgb888 = u32;

/// Framebuffer-style display.
pub trait Display {
    fn width(&self) -> u32;
    fn height(&self) -> u32;
    fn pixel_format(&self) -> PixelFormat;
    /// Copy a `w`×`h` rectangle from `src` (row stride `src_stride` pixels,
    /// canonical RGB888) to the screen at (`x`, `y`). Rectangles that extend
    /// past the screen are an `InvalidArgument` error, not clipped silently.
    fn blit(
        &mut self,
        src: &[Rgb888],
        src_stride: usize,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> HalResult<()>;
}

// ---------------------------------------------------------------- input

/// Logical key. Printable characters arrive as `Char`; keyboard-wedge
/// barcode scanners look exactly like a keyboard at this layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Backspace,
    Tab,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Delete,
    Insert,
    /// Function keys F1..F12.
    F(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEvent {
    /// A key press. `at_us` is the monotonic time the platform observed it,
    /// used by the UI for scanner-vs-human timing.
    Key {
        key: Key,
        mods: Modifiers,
        at_us: u64,
    },
    /// Absolute pointer position in display pixels.
    Pointer {
        x: u32,
        y: u32,
        pressed: bool,
        at_us: u64,
    },
}

pub trait Input {
    /// Return the next pending event without blocking.
    fn poll(&mut self) -> Option<InputEvent>;
    /// Whether a pointing device (touch/mouse) is available.
    fn has_pointer(&self) -> bool;
}

// ---------------------------------------------------------------- clock

/// Calendar time as reported by the RTC (UTC is assumed; the firmware RTC
/// is set to UTC as part of the deployment checklist).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct DateTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

impl DateTime {
    /// Seconds since 1970-01-01T00:00:00Z. Returns `None` for dates before
    /// 1970 or invalid field values.
    pub fn to_unix(&self) -> Option<i64> {
        if !(1..=12).contains(&self.month)
            || self.day == 0
            || self.day > days_in_month(self.year, self.month)
            || self.hour > 23
            || self.minute > 59
            || self.second > 60
            || self.year < 1970
        {
            return None;
        }
        let days = days_from_civil(i64::from(self.year), self.month, self.day);
        Some(
            days * 86_400
                + i64::from(self.hour) * 3600
                + i64::from(self.minute) * 60
                + i64::from(self.second),
        )
    }

    /// Inverse of [`DateTime::to_unix`] for non-negative timestamps.
    pub fn from_unix(t: i64) -> Option<Self> {
        if t < 0 {
            return None;
        }
        let days = t.div_euclid(86_400);
        let rem = t.rem_euclid(86_400);
        let (y, m, d) = civil_from_days(days);
        Some(DateTime {
            year: u16::try_from(y).ok()?,
            month: m,
            day: d,
            hour: (rem / 3600) as u8,
            minute: ((rem % 3600) / 60) as u8,
            second: (rem % 60) as u8,
        })
    }
}

fn is_leap(y: u16) -> bool {
    (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400)
}

fn days_in_month(y: u16, m: u8) -> u8 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(y) => 29,
        2 => 28,
        _ => 0,
    }
}

// Howard Hinnant's days_from_civil / civil_from_days, integer only.
fn days_from_civil(y: i64, m: u8, d: u8) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(m);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, u8, u8) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub trait Clock {
    /// Monotonic microseconds since an arbitrary epoch (boot). Never goes
    /// backwards.
    fn monotonic_us(&self) -> u64;
    /// Wall-clock time from the RTC.
    fn wall(&self) -> HalResult<DateTime>;
}

// ---------------------------------------------------------------- storage

/// A raw block device: the dedicated data partition. LBAs are relative to
/// the start of the partition.
pub trait BlockDevice {
    fn block_size(&self) -> usize;
    fn block_count(&self) -> u64;
    /// Read `buf.len() / block_size` blocks starting at `lba`. `buf.len()`
    /// must be a non-zero multiple of the block size.
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> HalResult<()>;
    /// Write blocks. The data is *not* durable until [`flush`] returns `Ok`.
    ///
    /// [`flush`]: BlockDevice::flush
    fn write(&mut self, lba: u64, buf: &[u8]) -> HalResult<()>;
    /// Make every completed write durable (UEFI `FlushBlocks`, SCSI
    /// SYNCHRONIZE CACHE).
    fn flush(&mut self) -> HalResult<()>;
}

/// Lets a borrowed device be used wherever an owned one is expected
/// (partition windows, filesystems over a device the caller keeps).
impl<D: BlockDevice + ?Sized> BlockDevice for &mut D {
    fn block_size(&self) -> usize {
        (**self).block_size()
    }
    fn block_count(&self) -> u64 {
        (**self).block_count()
    }
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> HalResult<()> {
        (**self).read(lba, buf)
    }
    fn write(&mut self, lba: u64, buf: &[u8]) -> HalResult<()> {
        (**self).write(lba, buf)
    }
    fn flush(&mut self) -> HalResult<()> {
        (**self).flush()
    }
}

/// External removable media with a FAT filesystem, used for CSV export and
/// catalog import. Paths are flat file names in the volume root.
pub trait ExternalMedia {
    /// Whether a writable external volume (other than the boot stick) is
    /// currently attached.
    fn is_present(&mut self) -> bool;
    /// Create or truncate `name` and write `data`, then flush the volume.
    fn write_file(&mut self, name: &str, data: &[u8]) -> HalResult<()>;
    /// Read `name` entirely; fails with `NoSpace` if it exceeds `max_len`.
    fn read_file(&mut self, name: &str, max_len: usize) -> HalResult<Vec<u8>>;
}

// ---------------------------------------------------------------- printer

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrinterStatus {
    Ready,
    NotPresent,
    PaperOut,
    Error,
}

pub trait Printer {
    /// Send raw ESC/POS bytes. Must not return until the bytes are handed to
    /// the device (bulk transfer completed / serial FIFO drained).
    fn write(&mut self, bytes: &[u8]) -> HalResult<()>;
    fn status(&mut self) -> PrinterStatus;
    /// Human-readable description of the transport, for the status bar.
    fn describe(&self) -> String;
}

// ---------------------------------------------------------------- rng / power

pub trait Rng {
    /// Fill `buf` with cryptographically secure random bytes, or fail. Must
    /// never return predictable data on failure.
    fn fill(&mut self, buf: &mut [u8]) -> HalResult<()>;
}

pub trait Power {
    fn reset(&mut self) -> !;
    fn shutdown(&mut self) -> !;
}

// ---------------------------------------------------------------- network

/// An Ethernet interface: whole frames in and out (no FCS), never blocking.
pub trait Nic {
    fn mac(&self) -> [u8; 6];
    fn link_up(&mut self) -> bool;
    /// Queue one frame (14-byte header + payload, ≤ 1514 bytes).
    fn transmit(&mut self, frame: &[u8]) -> HalResult<()>;
    /// Copy the next received frame into `buf` and return its length.
    fn receive(&mut self, buf: &mut [u8]) -> Option<usize>;
}

/// No network hardware (or networking off).
#[derive(Debug, Default)]
pub struct NoNic;

impl Nic for NoNic {
    fn mac(&self) -> [u8; 6] {
        [0x02, 0, 0, 0, 0, 0]
    }
    fn link_up(&mut self) -> bool {
        false
    }
    fn transmit(&mut self, _: &[u8]) -> HalResult<()> {
        Err(HalError::NotFound)
    }
    fn receive(&mut self, _: &mut [u8]) -> Option<usize> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_roundtrip_known_values() {
        let dt = DateTime {
            year: 2000,
            month: 3,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
        };
        assert_eq!(dt.to_unix(), Some(951_868_800));
        assert_eq!(DateTime::from_unix(951_868_800), Some(dt));
        let epoch = DateTime::from_unix(0).unwrap();
        assert_eq!((epoch.year, epoch.month, epoch.day), (1970, 1, 1));
    }

    #[test]
    fn unix_roundtrip_every_day_for_200_years() {
        let mut t = 0i64;
        while t < 200 * 366 * 86_400 {
            let dt = DateTime::from_unix(t).unwrap();
            assert_eq!(dt.to_unix(), Some(t));
            t += 86_400 + 3_661;
        }
    }

    #[test]
    fn rejects_invalid_dates() {
        let bad = DateTime {
            year: 2023,
            month: 2,
            day: 29,
            hour: 0,
            minute: 0,
            second: 0,
        };
        assert_eq!(bad.to_unix(), None);
    }
}
