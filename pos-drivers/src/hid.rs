//! HID: boot-protocol keyboard decoding (US layout) and a report-descriptor
//! parser that finds an absolute pointer (tablet, touch screen) layout.

use alloc::vec::Vec;
use pos_hal::{Key, Modifiers};

// ---------------------------------------------------------------- keyboard

/// Map a keyboard-page usage to a key, US layout. Keypad digits assume
/// NumLock on (keyboard-wedge scanners use the main row anyway).
pub fn usage_to_key(usage: u8, shift: bool, caps: bool) -> Option<Key> {
    const ROW1: &[u8; 10] = b"1234567890";
    const ROW1_SHIFT: &[u8; 10] = b"!@#$%^&*()";
    let c = |ch: u8| Some(Key::Char(char::from(ch)));
    match usage {
        0x04..=0x1d => {
            let base = b'a' + (usage - 0x04);
            c(if shift != caps {
                base.to_ascii_uppercase()
            } else {
                base
            })
        }
        0x1e..=0x27 => {
            let i = usize::from(usage - 0x1e);
            c(if shift { ROW1_SHIFT[i] } else { ROW1[i] })
        }
        0x28 | 0x58 => Some(Key::Enter),
        0x29 => Some(Key::Escape),
        0x2a => Some(Key::Backspace),
        0x2b => Some(Key::Tab),
        0x2c => c(b' '),
        0x2d..=0x38 => {
            // - = [ ] \ (0x32 non-US #) ; ' ` , . /
            const N: &[u8; 12] = b"-=[]\\#;'`,./";
            const S: &[u8; 12] = b"_+{}|~:\"~<>?";
            let i = usize::from(usage - 0x2d);
            c(if shift { S[i] } else { N[i] })
        }
        0x3a..=0x45 => Some(Key::F(usage - 0x3a + 1)),
        0x49 => Some(Key::Insert),
        0x4a => Some(Key::Home),
        0x4b => Some(Key::PageUp),
        0x4c => Some(Key::Delete),
        0x4d => Some(Key::End),
        0x4e => Some(Key::PageDown),
        0x4f => Some(Key::Right),
        0x50 => Some(Key::Left),
        0x51 => Some(Key::Down),
        0x52 => Some(Key::Up),
        0x54 => c(b'/'),
        0x55 => c(b'*'),
        0x56 => c(b'-'),
        0x57 => c(b'+'),
        0x59..=0x61 => c(b'1' + (usage - 0x59)),
        0x62 => c(b'0'),
        0x63 => c(b'.'),
        _ => None,
    }
}

const CAPS_LOCK: u8 = 0x39;
/// Typematic delay and rate, as on a PC keyboard.
pub const REPEAT_DELAY_US: u64 = 500_000;
pub const REPEAT_PERIOD_US: u64 = 33_000;

#[derive(Debug, Default)]
pub struct BootKeyboard {
    prev: [u8; 6],
    caps: bool,
    /// Most recently pressed, still-held usage and its next repeat time.
    held: Option<(u8, u64)>,
    mods: u8,
}

fn mods_of(m: u8) -> Modifiers {
    Modifiers {
        ctrl: m & 0x11 != 0,
        shift: m & 0x22 != 0,
        alt: m & 0x44 != 0,
    }
}

impl BootKeyboard {
    /// Process one 8-byte boot report observed at `now_us`; returns the
    /// newly pressed keys in report order.
    pub fn report(&mut self, r: &[u8], now_us: u64) -> Vec<(Key, Modifiers)> {
        let mut out = Vec::new();
        if r.len() < 8 {
            return out;
        }
        let keys = &r[2..8];
        // Phantom state (too many keys): ignore the report entirely.
        if keys.iter().all(|&k| k == 0x01) {
            return out;
        }
        self.mods = r[0];
        let m = mods_of(r[0]);
        for &u in keys {
            if u < 0x04 || self.prev.contains(&u) {
                continue;
            }
            if u == CAPS_LOCK {
                self.caps = !self.caps;
                continue;
            }
            if let Some(k) = usage_to_key(u, m.shift, self.caps) {
                out.push((k, m));
                self.held = Some((u, now_us + REPEAT_DELAY_US));
            }
        }
        if let Some((u, _)) = self.held
            && !keys.contains(&u)
        {
            self.held = None;
        }
        self.prev.copy_from_slice(keys);
        out
    }

    /// Typematic repeat: call periodically; yields a key when one is due.
    pub fn repeat(&mut self, now_us: u64) -> Option<(Key, Modifiers)> {
        let (u, due) = self.held?;
        if now_us < due {
            return None;
        }
        self.held = Some((u, now_us + REPEAT_PERIOD_US));
        let m = mods_of(self.mods);
        usage_to_key(u, m.shift, self.caps).map(|k| (k, m))
    }

    /// Release everything (device unplugged or transfer error).
    pub fn reset(&mut self) {
        self.prev = [0; 6];
        self.held = None;
    }
}

// ---------------------------------------------------------------- pointer

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field {
    pub bit_offset: u32,
    pub bit_size: u32,
    pub logical_min: i32,
    pub logical_max: i32,
}

impl Field {
    /// Extract the field from a report (after the report-ID byte, if any).
    pub fn read(&self, data: &[u8]) -> Option<i32> {
        if self.bit_size == 0 || self.bit_size > 32 {
            return None;
        }
        let mut v: u64 = 0;
        for i in 0..self.bit_size {
            let bit = self.bit_offset + i;
            let byte = *data.get((bit / 8) as usize)?;
            if byte >> (bit % 8) & 1 != 0 {
                v |= 1 << i;
            }
        }
        // Sign-extend when the logical range is signed.
        if self.logical_min < 0 && self.bit_size < 32 && v >> (self.bit_size - 1) & 1 != 0 {
            v |= !0u64 << self.bit_size;
        }
        Some(v as i64 as i32)
    }
}

/// Where an absolute pointer's X, Y and primary button/tip are in its
/// input report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PointerLayout {
    pub report_id: Option<u8>,
    pub x: Field,
    pub y: Field,
    pub button: Option<Field>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PointerSample {
    pub x: u32,
    pub y: u32,
    pub pressed: bool,
}

impl PointerLayout {
    /// Decode a report and scale X/Y to `w`×`h` pixels. Returns `None` for
    /// reports with another report ID or out-of-range data.
    pub fn decode(&self, report: &[u8], w: u32, h: u32) -> Option<PointerSample> {
        let data = match self.report_id {
            Some(id) => {
                if report.first() != Some(&id) {
                    return None;
                }
                &report[1..]
            }
            None => report,
        };
        let scale = |f: &Field, v: i32, size: u32| -> Option<u32> {
            let span = i64::from(f.logical_max) - i64::from(f.logical_min);
            if span <= 0 || size == 0 {
                return None;
            }
            let rel = (i64::from(v) - i64::from(f.logical_min)).clamp(0, span);
            Some((rel * i64::from(size - 1) / span) as u32)
        };
        let x = scale(&self.x, self.x.read(data)?, w)?;
        let y = scale(&self.y, self.y.read(data)?, h)?;
        let pressed = match &self.button {
            Some(b) => b.read(data)? != 0,
            None => false,
        };
        Some(PointerSample { x, y, pressed })
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Globals {
    usage_page: u32,
    logical_min: i32,
    logical_max: i32,
    report_size: u32,
    report_count: u32,
    report_id: Option<u8>,
}

/// Find the first absolute X/Y pair (generic desktop 0x30/0x31 with the
/// "relative" flag clear) and the first button 1 (0x09:0x01) or digitizer
/// tip switch (0x0d:0x42) in the same report.
pub fn parse_pointer(desc: &[u8]) -> Option<PointerLayout> {
    let mut g = Globals::default();
    let mut stack: Vec<Globals> = Vec::new();
    let mut usages: Vec<u32> = Vec::new();
    let mut usage_min: Option<u32> = None;
    let mut usage_max: Option<u32> = None;
    // Bit offsets per report ID.
    let mut offsets: Vec<(Option<u8>, u32)> = Vec::new();
    let (mut x, mut y, mut button) = (None, None, None);
    let mut layout_id: Option<Option<u8>> = None;

    let mut i = 0;
    while i < desc.len() {
        let prefix = desc[i];
        if prefix == 0xfe {
            // Long item: skip.
            let size = usize::from(*desc.get(i + 1)?);
            i += 3 + size;
            continue;
        }
        let size = match prefix & 3 {
            3 => 4,
            n => usize::from(n),
        };
        let data = desc.get(i + 1..i + 1 + size)?;
        i += 1 + size;
        let mut u: u32 = 0;
        for (k, b) in data.iter().enumerate() {
            u |= u32::from(*b) << (8 * k);
        }
        let s: i32 = match size {
            1 => i32::from(u as u8 as i8),
            2 => i32::from(u as u16 as i16),
            _ => u as i32,
        };
        let full_usage = |u: u32, page: u32| if size == 4 { u } else { page << 16 | u };
        match (prefix >> 2) & 3 {
            // Main items.
            0 => {
                let tag = prefix >> 4;
                if tag == 0x8 {
                    // Input.
                    let constant = u & 1 != 0;
                    let relative = u & 4 != 0;
                    let off = match offsets.iter_mut().find(|(id, _)| *id == g.report_id) {
                        Some((_, o)) => o,
                        None => {
                            offsets.push((g.report_id, 0));
                            &mut offsets.last_mut()?.1
                        }
                    };
                    for n in 0..g.report_count {
                        let usage = if let Some(&uu) = usages.get(n as usize) {
                            Some(uu)
                        } else if let (Some(lo), Some(hi)) = (usage_min, usage_max) {
                            (lo + n <= hi).then_some(lo + n)
                        } else {
                            usages.last().copied()
                        };
                        let field = Field {
                            bit_offset: *off + n * g.report_size,
                            bit_size: g.report_size,
                            logical_min: g.logical_min,
                            logical_max: g.logical_max,
                        };
                        let same_report = layout_id.is_none_or(|id| id == g.report_id);
                        if !constant
                            && same_report
                            && let Some(us) = usage
                        {
                            match us {
                                0x0001_0030 if !relative && x.is_none() => {
                                    x = Some(field);
                                    layout_id = Some(g.report_id);
                                }
                                0x0001_0031 if !relative && y.is_none() => {
                                    y = Some(field);
                                    layout_id = Some(g.report_id);
                                }
                                0x0009_0001 | 0x000d_0042 if button.is_none() => {
                                    button = Some(field)
                                }
                                _ => {}
                            }
                        }
                    }
                    *off += g.report_count * g.report_size;
                } else if tag == 0x9 || tag == 0xb {
                    // Output / feature reports don't move input offsets.
                }
                usages.clear();
                usage_min = None;
                usage_max = None;
            }
            // Global items.
            1 => match prefix >> 4 {
                0x0 => g.usage_page = u,
                0x1 => g.logical_min = s,
                0x2 => {
                    // Logical max is unsigned when min is non-negative.
                    g.logical_max = if g.logical_min >= 0 { u as i32 } else { s }
                }
                0x7 => g.report_size = u,
                0x8 => g.report_id = Some(u as u8),
                0x9 => g.report_count = u,
                0xa => stack.push(g),
                0xb => g = stack.pop()?,
                _ => {}
            },
            // Local items.
            2 => match prefix >> 4 {
                0x0 => usages.push(full_usage(u, g.usage_page)),
                0x1 => usage_min = Some(full_usage(u, g.usage_page)),
                0x2 => usage_max = Some(full_usage(u, g.usage_page)),
                _ => {}
            },
            _ => {}
        }
    }
    Some(PointerLayout {
        report_id: layout_id?,
        x: x?,
        y: y?,
        button,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // QEMU usb-tablet report descriptor.
    const TABLET: &[u8] = &[
        0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29,
        0x03, 0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x05,
        0x81, 0x01, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x00, 0x26, 0xff, 0x7f, 0x35, 0x00,
        0x46, 0xff, 0x7f, 0x75, 0x10, 0x95, 0x02, 0x81, 0x02, 0x05, 0x01, 0x09, 0x38, 0x15, 0x81,
        0x25, 0x7f, 0x35, 0x00, 0x45, 0x00, 0x75, 0x08, 0x95, 0x01, 0x81, 0x06, 0xc0, 0xc0,
    ];

    #[test]
    fn qemu_tablet_layout() {
        let l = parse_pointer(TABLET).unwrap();
        assert_eq!(l.report_id, None);
        assert_eq!(
            (l.x.bit_offset, l.x.bit_size, l.x.logical_max),
            (8, 16, 0x7fff)
        );
        assert_eq!(l.y.bit_offset, 24);
        assert_eq!(l.button.unwrap().bit_offset, 0);
        // Button down at the centre.
        let s = l
            .decode(&[1, 0xff, 0x3f, 0xff, 0x3f, 0], 1024, 768)
            .unwrap();
        assert_eq!(
            s,
            PointerSample {
                x: 511,
                y: 383,
                pressed: true
            }
        );
        let s = l.decode(&[0, 0xff, 0x7f, 0, 0, 0], 1024, 768).unwrap();
        assert_eq!((s.x, s.y, s.pressed), (1023, 0, false));
    }

    #[test]
    fn relative_mouse_rejected() {
        // Boot mouse: X/Y relative.
        let mouse = [
            0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x15, 0x00,
            0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x05, 0x81, 0x01,
            0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x81, 0x25, 0x7f, 0x75, 0x08, 0x95, 0x02,
            0x81, 0x06, 0xc0,
        ];
        assert!(parse_pointer(&mouse).is_none());
    }

    #[test]
    fn digitizer_with_report_id() {
        // Report 2: tip switch bit, 7 pad bits, X 0..4095, Y 0..4095.
        let d = [
            0x05, 0x0d, 0x09, 0x04, 0xa1, 0x01, 0x85, 0x02, 0x09, 0x22, 0xa1, 0x02, 0x09, 0x42,
            0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x01, 0x81, 0x02, 0x95, 0x07, 0x81, 0x03,
            0x05, 0x01, 0x26, 0xff, 0x0f, 0x75, 0x10, 0x95, 0x01, 0x09, 0x30, 0x81, 0x02, 0x09,
            0x31, 0x81, 0x02, 0xc0, 0xc0,
        ];
        let l = parse_pointer(&d).unwrap();
        assert_eq!(l.report_id, Some(2));
        let s = l.decode(&[2, 1, 0xff, 0x0f, 0x00, 0x00], 800, 600).unwrap();
        assert_eq!(
            s,
            PointerSample {
                x: 799,
                y: 0,
                pressed: true
            }
        );
        assert!(l.decode(&[3, 1, 0, 0, 0, 0], 800, 600).is_none());
    }

    #[test]
    fn parser_never_panics_on_truncation() {
        for n in 0..TABLET.len() {
            let _ = parse_pointer(&TABLET[..n]);
        }
    }

    #[test]
    fn keyboard_press_release_shift_caps() {
        let mut k = BootKeyboard::default();
        let ev = k.report(&[0x02, 0, 0x04, 0, 0, 0, 0, 0], 0); // Shift + a
        assert_eq!(ev[0].0, Key::Char('A'));
        assert!(ev[0].1.shift);
        assert!(k.report(&[0x02, 0, 0x04, 0, 0, 0, 0, 0], 10).is_empty()); // still held
        k.report(&[0; 8], 20);
        k.report(&[0, 0, CAPS_LOCK, 0, 0, 0, 0, 0], 30);
        k.report(&[0; 8], 40);
        assert_eq!(
            k.report(&[0, 0, 0x05, 0, 0, 0, 0, 0], 50)[0].0,
            Key::Char('B')
        );
        k.report(&[0; 8], 60);
        assert_eq!(k.report(&[0, 0, 0x1e, 0x28, 0, 0, 0, 0], 70).len(), 2); // '1', Enter
        assert!(k.report(&[0, 0, 1, 1, 1, 1, 1, 1], 80).is_empty()); // phantom
    }

    #[test]
    fn typematic_repeat() {
        let mut k = BootKeyboard::default();
        k.report(&[0, 0, 0x2a, 0, 0, 0, 0, 0], 0); // Backspace held
        assert!(k.repeat(REPEAT_DELAY_US - 1).is_none());
        assert_eq!(k.repeat(REPEAT_DELAY_US).unwrap().0, Key::Backspace);
        assert!(k.repeat(REPEAT_DELAY_US + 1).is_none());
        assert!(k.repeat(REPEAT_DELAY_US + REPEAT_PERIOD_US).is_some());
        k.report(&[0; 8], REPEAT_DELAY_US * 2);
        assert!(k.repeat(REPEAT_DELAY_US * 10).is_none());
    }

    #[test]
    fn us_symbols() {
        assert_eq!(usage_to_key(0x37, false, false), Some(Key::Char('.')));
        assert_eq!(usage_to_key(0x34, true, false), Some(Key::Char('"')));
        assert_eq!(usage_to_key(0x45, false, false), Some(Key::F(12)));
        assert_eq!(usage_to_key(0x62, false, false), Some(Key::Char('0')));
    }
}
