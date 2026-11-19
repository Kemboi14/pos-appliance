//! CRC-32C (Castagnoli), reflected polynomial 0x82F63B78, table-driven.

const fn table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                (c >> 1) ^ 0x82F6_3B78
            } else {
                c >> 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static TABLE: [u32; 256] = table();

#[derive(Debug, Clone, Copy)]
pub struct Crc32c(u32);

impl Default for Crc32c {
    fn default() -> Self {
        Crc32c(!0)
    }
}

impl Crc32c {
    pub fn update(&mut self, data: &[u8]) {
        let mut c = self.0;
        for &b in data {
            c = TABLE[((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8);
        }
        self.0 = c;
    }
    pub fn finish(self) -> u32 {
        !self.0
    }
}

pub fn crc32c(data: &[u8]) -> u32 {
    let mut c = Crc32c::default();
    c.update(data);
    c.finish()
}

#[cfg(test)]
mod tests {
    #[test]
    fn check_value() {
        // RFC 3720 / standard check value.
        assert_eq!(super::crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(super::crc32c(&[0u8; 32]), 0x8A91_36AA);
    }
}
