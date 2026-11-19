//! GTIN (EAN-8, UPC-A, EAN-13) validation and price/weight-embedded codes.

use alloc::string::String;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarcodeError {
    Empty,
    BadCharacter,
    BadLength,
    BadCheckDigit,
}

/// GS1 mod-10 check digit for `body` (all digits except the check digit).
pub fn gs1_check_digit(body: &[u8]) -> Option<u8> {
    let mut sum: u32 = 0;
    for (i, &c) in body.iter().rev().enumerate() {
        if !c.is_ascii_digit() {
            return None;
        }
        let d = u32::from(c - b'0');
        sum += if i % 2 == 0 { d * 3 } else { d };
    }
    Some(((10 - (sum % 10)) % 10) as u8)
}

fn is_gtin_len(n: usize) -> bool {
    matches!(n, 8 | 12 | 13 | 14)
}

/// Validate a GTIN-8/12/13/14 and return its canonical form: UPC-A (12
/// digits) is normalised to EAN-13 by prefixing `0`, so a UPC-A scan matches
/// an EAN-13 catalog entry and vice versa.
pub fn validate_gtin(code: &str) -> Result<String, BarcodeError> {
    let b = code.as_bytes();
    if b.is_empty() {
        return Err(BarcodeError::Empty);
    }
    if !b.iter().all(u8::is_ascii_digit) {
        return Err(BarcodeError::BadCharacter);
    }
    if !is_gtin_len(b.len()) {
        return Err(BarcodeError::BadLength);
    }
    let (body, check) = b.split_at(b.len() - 1);
    if gs1_check_digit(body) != Some(check[0] - b'0') {
        return Err(BarcodeError::BadCheckDigit);
    }
    let mut s = String::with_capacity(13);
    if b.len() == 12 {
        s.push('0');
    }
    s.push_str(code);
    Ok(s)
}

/// Normalise any catalog/scan code: digit strings of GTIN length must carry
/// a valid check digit; anything else is an internal code (1–20 ASCII
/// alphanumerics, `-` or `_`), upper-cased.
pub fn normalize_code(code: &str) -> Result<String, BarcodeError> {
    let code = code.trim();
    if code.is_empty() {
        return Err(BarcodeError::Empty);
    }
    if code.bytes().all(|c| c.is_ascii_digit()) && is_gtin_len(code.len()) {
        return validate_gtin(code);
    }
    if code.len() > 20
        || !code
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err(BarcodeError::BadCharacter);
    }
    Ok(code.to_ascii_uppercase())
}

/// What the embedded 5-digit value of a variable-measure code means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EmbedKind {
    /// Price in minor units (e.g. `01234` = 12.34).
    Price,
    /// Weight in grams (e.g. `01234` = 1.234 kg).
    Weight,
}

/// Variable-measure (in-store) EAN-13 layout: `PP IIIII VVVVV C`, where `PP`
/// is one of `prefixes` (GS1 reserves 20–29 for in-store use), `IIIII` the
/// item's PLU and `VVVVV` the price or weight. No separate price check digit
/// is used (some regions add one; that would be a new `EmbedKind`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriceEmbed {
    pub prefixes: alloc::vec::Vec<u8>,
    pub kind: EmbedKind,
}

impl Default for PriceEmbed {
    fn default() -> Self {
        PriceEmbed {
            prefixes: (20..=29).collect(),
            kind: EmbedKind::Price,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scan {
    /// A regular code: look it up as a barcode.
    Code(String),
    /// A variable-measure code: look up the PLU, use the embedded value.
    Embedded {
        plu: String,
        kind: EmbedKind,
        value: u32,
    },
}

/// Classify a scanned code (already terminated by Enter, without it).
pub fn classify(code: &str, embed: Option<&PriceEmbed>) -> Result<Scan, BarcodeError> {
    let norm = normalize_code(code)?;
    if let Some(cfg) = embed {
        let b = norm.as_bytes();
        if b.len() == 13 && b[0] != b'0' {
            let prefix = (b[0] - b'0') * 10 + (b[1] - b'0');
            if cfg.prefixes.contains(&prefix) {
                let plu = String::from(&norm[2..7]);
                let value = norm[7..12]
                    .parse::<u32>()
                    .map_err(|_| BarcodeError::BadCharacter)?;
                return Ok(Scan::Embedded {
                    plu,
                    kind: cfg.kind,
                    value,
                });
            }
        }
    }
    Ok(Scan::Code(norm))
}

/// Build a valid variable-measure EAN-13 (used by tests and label printing).
pub fn make_embedded(prefix: u8, plu: u32, value: u32) -> String {
    let body = alloc::format!("{prefix:02}{plu:05}{value:05}");
    let c = gs1_check_digit(body.as_bytes()).unwrap_or(0);
    alloc::format!("{body}{c}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_gtins() {
        // Published examples.
        assert_eq!(
            validate_gtin("4006381333931").as_deref(),
            Ok("4006381333931")
        );
        assert_eq!(
            validate_gtin("036000291452").as_deref(),
            Ok("0036000291452")
        );
        assert_eq!(validate_gtin("96385074").as_deref(), Ok("96385074"));
        assert_eq!(
            validate_gtin("4006381333932"),
            Err(BarcodeError::BadCheckDigit)
        );
        assert_eq!(validate_gtin("40063813339"), Err(BarcodeError::BadLength));
        assert_eq!(
            validate_gtin("40063813339a1"),
            Err(BarcodeError::BadCharacter)
        );
    }

    #[test]
    fn internal_codes() {
        assert_eq!(normalize_code(" abc-1 ").as_deref(), Ok("ABC-1"));
        assert_eq!(normalize_code("123").as_deref(), Ok("123"));
        assert!(normalize_code("a b").is_err());
    }

    #[test]
    fn embedded() {
        let code = make_embedded(21, 4711, 1234);
        assert_eq!(code.len(), 13);
        let s = classify(&code, Some(&PriceEmbed::default())).unwrap();
        assert_eq!(
            s,
            Scan::Embedded {
                plu: "04711".into(),
                kind: EmbedKind::Price,
                value: 1234
            }
        );
        // Without embed config it's a plain code.
        assert!(matches!(classify(&code, None).unwrap(), Scan::Code(_)));
    }
}
