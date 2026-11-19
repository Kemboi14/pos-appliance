//! ESC/POS command encoder (Epson TM-series command set; also accepted by the
//! common 58/80 mm clones). Pure byte generation, no I/O.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod codepage;

pub use codepage::CodePage;

use alloc::vec::Vec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left = 0,
    Center = 1,
    Right = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cut {
    /// Feed to the cutter and cut fully (GS V 65 n).
    Full,
    /// Feed to the cutter and leave a tab (GS V 66 n).
    Partial,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QrEcc {
    L = 48,
    M = 49,
    Q = 50,
    H = 51,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeError {
    /// EAN-13 data must be 12 or 13 digits.
    BadEan,
    /// Code 128 data must be printable ASCII, ≤ 253 bytes.
    BadCode128,
    /// QR payload too long for a single GS ( k store command.
    QrTooLong,
}

/// Builder for an ESC/POS byte stream.
#[derive(Debug, Clone)]
pub struct Encoder {
    buf: Vec<u8>,
    cp: CodePage,
}

impl Encoder {
    /// Start a job: `ESC @` (initialise) then select the code page.
    pub fn new(cp: CodePage) -> Encoder {
        let mut e = Encoder {
            buf: Vec::with_capacity(1024),
            cp,
        };
        e.raw(&[0x1B, 0x40]);
        e.raw(&[0x1B, 0x74, cp.escpos_table()]);
        e
    }

    pub fn raw(&mut self, b: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(b);
        self
    }

    /// Text in the selected code page. Characters it cannot represent become
    /// `?`. Control characters other than `\n` are dropped so text can never
    /// inject commands.
    pub fn text(&mut self, s: &str) -> &mut Self {
        for c in s.chars() {
            match c {
                '\n' => self.buf.push(b'\n'),
                c if (c as u32) < 0x20 || c == '\u{7f}' => {}
                c => self.buf.push(self.cp.encode(c).unwrap_or(b'?')),
            }
        }
        self
    }

    pub fn line(&mut self, s: &str) -> &mut Self {
        self.text(s).raw(b"\n")
    }

    pub fn align(&mut self, a: Align) -> &mut Self {
        self.raw(&[0x1B, 0x61, a as u8])
    }

    pub fn bold(&mut self, on: bool) -> &mut Self {
        self.raw(&[0x1B, 0x45, u8::from(on)])
    }

    pub fn underline(&mut self, on: bool) -> &mut Self {
        self.raw(&[0x1B, 0x2D, u8::from(on)])
    }

    /// Character size multiplier 1..=8 in each direction (GS ! n).
    pub fn size(&mut self, w: u8, h: u8) -> &mut Self {
        let w = w.clamp(1, 8) - 1;
        let h = h.clamp(1, 8) - 1;
        self.raw(&[0x1D, 0x21, (w << 4) | h])
    }

    /// White-on-black (GS B n).
    pub fn invert(&mut self, on: bool) -> &mut Self {
        self.raw(&[0x1D, 0x42, u8::from(on)])
    }

    /// Print and feed `n` lines (ESC d n).
    pub fn feed(&mut self, n: u8) -> &mut Self {
        self.raw(&[0x1B, 0x64, n])
    }

    /// Feed `lines` and cut (GS V m n).
    pub fn cut(&mut self, kind: Cut, lines: u8) -> &mut Self {
        let m = match kind {
            Cut::Full => 65,
            Cut::Partial => 66,
        };
        self.raw(&[0x1D, 0x56, m, lines])
    }

    /// Cash-drawer kick on connector pin 2 (ESC p 0 t1 t2): pulse on for
    /// 50 ms (t1 = 25 × 2 ms), off for 500 ms (t2 = 250 × 2 ms).
    pub fn drawer_kick(&mut self) -> &mut Self {
        self.raw(&[0x1B, 0x70, 0x00, 25, 250])
    }

    fn barcode_setup(&mut self, height: u8) {
        // Height in dots, module width 2, HRI text below.
        self.raw(&[0x1D, 0x68, height.max(1)]);
        self.raw(&[0x1D, 0x77, 2]);
        self.raw(&[0x1D, 0x48, 2]);
    }

    /// EAN-13 barcode (GS k 67 n d…). Accepts 12 digits (printer adds the
    /// check digit) or 13 (check digit verified by the printer).
    pub fn ean13(&mut self, digits: &str, height: u8) -> Result<&mut Self, EncodeError> {
        let d = digits.as_bytes();
        if !(d.len() == 12 || d.len() == 13) || !d.iter().all(u8::is_ascii_digit) {
            return Err(EncodeError::BadEan);
        }
        self.barcode_setup(height);
        self.raw(&[0x1D, 0x6B, 67, d.len() as u8]);
        self.raw(d);
        Ok(self)
    }

    /// Code 128, code set B (GS k 73 n {B …). `{` in data is escaped as `{{`.
    pub fn code128(&mut self, data: &str, height: u8) -> Result<&mut Self, EncodeError> {
        let d = data.as_bytes();
        if d.is_empty() || !d.iter().all(|c| (0x20..0x7F).contains(c)) {
            return Err(EncodeError::BadCode128);
        }
        let mut body = Vec::with_capacity(d.len() + 2);
        body.extend_from_slice(b"{B");
        for &c in d {
            body.push(c);
            if c == b'{' {
                body.push(b'{');
            }
        }
        if body.len() > 255 {
            return Err(EncodeError::BadCode128);
        }
        self.barcode_setup(height);
        self.raw(&[0x1D, 0x6B, 73, body.len() as u8]);
        self.raw(&body);
        Ok(self)
    }

    /// QR code via GS ( k: model 2, module size `module` (1..=16 dots), ECC.
    pub fn qr(&mut self, data: &[u8], module: u8, ecc: QrEcc) -> Result<&mut Self, EncodeError> {
        if data.len() > 7089 {
            return Err(EncodeError::QrTooLong);
        }
        // fn 165: model 2
        self.raw(&[0x1D, 0x28, 0x6B, 4, 0, 49, 65, 50, 0]);
        // fn 167: module size
        self.raw(&[0x1D, 0x28, 0x6B, 3, 0, 49, 67, module.clamp(1, 16)]);
        // fn 169: error correction
        self.raw(&[0x1D, 0x28, 0x6B, 3, 0, 49, 69, ecc as u8]);
        // fn 180: store data (pL pH = len + 3)
        let n = data.len() + 3;
        self.raw(&[
            0x1D,
            0x28,
            0x6B,
            (n & 0xFF) as u8,
            (n >> 8) as u8,
            49,
            80,
            48,
        ]);
        self.raw(data);
        // fn 181: print
        self.raw(&[0x1D, 0x28, 0x6B, 3, 0, 49, 81, 48]);
        Ok(self)
    }

    pub fn finish(self) -> Vec<u8> {
        self.buf
    }

    pub fn bytes(&self) -> &[u8] {
        &self.buf
    }
}

/// Real-time status request (DLE EOT n). n = 1 printer, 2 offline, 3 error,
/// 4 paper sensor. The response is one byte on the printer's IN pipe.
pub fn status_request(n: u8) -> [u8; 3] {
    [0x10, 0x04, n]
}

/// Interpret a DLE EOT 4 (paper sensor) response.
pub fn paper_out(resp: u8) -> bool {
    resp & 0x60 == 0x60
}

/// Lay out `left` and `right` on one line of `width` columns; `left` is
/// truncated if both do not fit.
pub fn two_col(left: &str, right: &str, width: usize) -> alloc::string::String {
    let rl = right.chars().count();
    let room = width.saturating_sub(rl + 1);
    let l: alloc::string::String = left.chars().take(room).collect();
    let pad = width.saturating_sub(l.chars().count() + rl);
    let mut s = l;
    for _ in 0..pad {
        s.push(' ');
    }
    s.push_str(right);
    s
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Compare against `tests/golden/<name>.bin`; set UPDATE_GOLDEN=1 to
    /// (re)write the golden file.
    pub(crate) fn golden(name: &str, bytes: &[u8]) {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/golden")
            .join(name);
        if std::env::var_os("UPDATE_GOLDEN").is_some() {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, bytes).unwrap();
        }
        let want = std::fs::read(&p)
            .unwrap_or_else(|_| panic!("missing golden {p:?}; run with UPDATE_GOLDEN=1"));
        assert!(want == bytes, "golden mismatch for {name}");
    }

    #[test]
    fn commands() {
        let mut e = Encoder::new(CodePage::Cp858);
        e.align(Align::Center).bold(true).line("SHOP").bold(false);
        e.drawer_kick().cut(Cut::Partial, 3);
        let b = e.finish();
        assert_eq!(&b[..5], &[0x1B, 0x40, 0x1B, 0x74, 19]);
        assert!(b.windows(5).any(|w| w == [0x1B, 0x70, 0, 25, 250]));
        assert!(b.ends_with(&[0x1D, 0x56, 66, 3]));
    }

    #[test]
    fn text_is_sanitised_and_encoded() {
        let mut e = Encoder::new(CodePage::Cp858);
        e.text("€5\x1b@é✓");
        let b = e.finish();
        // € = 0xD5 in CP858, ESC dropped, é = 0x82, ✓ unknown → '?'.
        assert_eq!(&b[5..], &[0xD5, b'5', b'@', 0x82, b'?']);
    }

    #[test]
    fn barcodes() {
        let mut e = Encoder::new(CodePage::Cp437);
        e.ean13("400638133393", 80).unwrap();
        assert!(e.ean13("12", 80).is_err());
        e.code128("AB{1", 60).unwrap();
        e.qr(b"hello", 6, QrEcc::M).unwrap();
        let b = e.finish();
        assert!(b.windows(4).any(|w| w == [0x1D, 0x6B, 67, 12]));
        assert!(
            b.windows(9)
                .any(|w| w == [0x1D, 0x6B, 73, 7, b'{', b'B', b'A', b'B', b'{'])
        );
        assert!(
            b.windows(8)
                .any(|w| w == [0x1D, 0x28, 0x6B, 8, 0, 49, 80, 48])
        );
    }

    #[test]
    fn two_columns() {
        assert_eq!(two_col("Milk", "1.20", 12), "Milk    1.20");
        assert_eq!(two_col("A very long name", "12.00", 12), "A very 12.00");
    }

    #[test]
    fn golden_sample_receipt() {
        let mut e = Encoder::new(CodePage::Cp858);
        e.align(Align::Center)
            .size(2, 2)
            .line("DEMO STORE")
            .size(1, 1);
        e.line("1 Example Street");
        e.align(Align::Left);
        e.line(&two_col("Milk 1L", "1.20 A", 42));
        e.line(&two_col("Bread", "2.49 A", 42));
        e.bold(true)
            .line(&two_col("TOTAL", "€3.69", 42))
            .bold(false);
        e.align(Align::Center).ean13("400638133393", 60).unwrap();
        e.qr(b"POS:R1:abcd", 5, QrEcc::M).unwrap();
        e.feed(2).drawer_kick().cut(Cut::Partial, 3);
        golden("sample_receipt.bin", &e.finish());
    }
}
