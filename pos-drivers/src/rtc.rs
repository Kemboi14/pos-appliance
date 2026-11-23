//! CMOS RTC register decoding.

use pos_hal::DateTime;

fn bcd(v: u8) -> u8 {
    (v & 0x0f) + (v >> 4) * 10
}

/// Decode `[sec, min, hour, day, month, year]` as read from CMOS registers
/// 0x00/0x02/0x04/0x07/0x08/0x09. `status_b` bit 2 = binary mode, bit 1 =
/// 24-hour mode. Without ACPI's century register the year is taken as 20yy
/// (the appliance never runs before 2000). Returns `None` for impossible
/// values (a dead RTC battery reads as garbage).
pub fn decode(raw: [u8; 6], status_b: u8) -> Option<DateTime> {
    let [s, m, h, d, mo, y] = raw;
    let binary = status_b & 0x04 != 0;
    let conv = |v: u8| if binary { v } else { bcd(v) };
    let pm = h & 0x80 != 0;
    let mut hour = conv(h & 0x7f);
    if status_b & 0x02 == 0 {
        if hour == 0 || hour > 12 {
            return None;
        }
        // 12-hour mode: 12 AM = 0, 12 PM = 12.
        hour %= 12;
        if pm {
            hour += 12;
        }
    }
    let dt = DateTime {
        year: 2000 + u16::from(conv(y)),
        month: conv(mo),
        day: conv(d),
        hour,
        minute: conv(m),
        second: conv(s),
    };
    dt.to_unix().map(|_| dt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bcd_24h() {
        let dt = decode([0x59, 0x30, 0x23, 0x31, 0x12, 0x26], 0x02).unwrap();
        assert_eq!(
            (dt.year, dt.month, dt.day, dt.hour, dt.minute, dt.second),
            (2026, 12, 31, 23, 30, 59)
        );
    }

    #[test]
    fn binary_12h() {
        // 12 AM -> 0, 12 PM -> 12, 1 PM -> 13.
        assert_eq!(decode([0, 0, 12, 1, 1, 26], 0x04).unwrap().hour, 0);
        assert_eq!(decode([0, 0, 0x80 | 12, 1, 1, 26], 0x04).unwrap().hour, 12);
        assert_eq!(decode([0, 0, 0x80 | 1, 1, 1, 26], 0x04).unwrap().hour, 13);
    }

    #[test]
    fn garbage_rejected() {
        assert!(decode([0xff; 6], 0x02).is_none());
        assert!(decode([0, 0, 0, 0x30, 0x02, 0x26], 0x02).is_none()); // 30 Feb
    }
}
