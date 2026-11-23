//! Hardware-independent driver logic for Tier B, kept out of the kernel so
//! it can be unit-tested on the host: USB descriptor parsing, HID (boot
//! keyboard, report-descriptor pointer layout), SCSI/BOT framing, GPT/MBR
//! partition discovery, a FAT16/32 root-directory filesystem, and CMOS RTC
//! decoding. No `unsafe`, no allocation beyond `alloc`.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod fat;
pub mod hid;
pub mod part;
pub mod rtc;
pub mod scsi;
pub mod usb;

/// Parse a textual GUID into its on-disk (mixed-endian) byte order, as used
/// by GPT and the Limine protocol. Panics at compile time on bad input when
/// used in a `const`.
pub const fn guid(s: &str) -> [u8; 16] {
    const fn hex(c: u8) -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => panic!("bad hex digit in GUID"),
        }
    }
    let b = s.as_bytes();
    assert!(b.len() == 36, "GUID must be 36 characters");
    let mut raw = [0u8; 16];
    let mut i = 0;
    let mut n = 0;
    while i < 36 {
        if b[i] == b'-' {
            i += 1;
            continue;
        }
        raw[n] = (hex(b[i]) << 4) | hex(b[i + 1]);
        n += 1;
        i += 2;
    }
    assert!(n == 16, "GUID must have 32 hex digits");
    [
        raw[3], raw[2], raw[1], raw[0], raw[5], raw[4], raw[7], raw[6], raw[8], raw[9], raw[10],
        raw[11], raw[12], raw[13], raw[14], raw[15],
    ]
}

#[cfg(test)]
mod tests {
    #[test]
    fn guid_mixed_endian() {
        assert_eq!(
            super::guid("C12A7328-F81F-11D2-BA4B-00A0C93EC93B"),
            [
                0x28, 0x73, 0x2a, 0xc1, 0x1f, 0xf8, 0xd2, 0x11, 0xba, 0x4b, 0x00, 0xa0, 0xc9, 0x3e,
                0xc9, 0x3b
            ]
        );
    }
}
