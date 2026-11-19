//! Single-byte code pages for printer text.

/// Upper halves (0x80..=0xFF) of the supported code pages.
#[rustfmt::skip]
const CP437_HI: [char; 128] = [
    'Ç','ü','é','â','ä','à','å','ç','ê','ë','è','ï','î','ì','Ä','Å',
    'É','æ','Æ','ô','ö','ò','û','ù','ÿ','Ö','Ü','¢','£','¥','₧','ƒ',
    'á','í','ó','ú','ñ','Ñ','ª','º','¿','⌐','¬','½','¼','¡','«','»',
    '░','▒','▓','│','┤','╡','╢','╖','╕','╣','║','╗','╝','╜','╛','┐',
    '└','┴','┬','├','─','┼','╞','╟','╚','╔','╩','╦','╠','═','╬','╧',
    '╨','╤','╥','╙','╘','╒','╓','╫','╪','┘','┌','█','▄','▌','▐','▀',
    'α','ß','Γ','π','Σ','σ','µ','τ','Φ','Θ','Ω','δ','∞','φ','ε','∩',
    '≡','±','≥','≤','⌠','⌡','÷','≈','°','∙','·','√','ⁿ','²','■','\u{a0}',
];

/// CP858 = CP850 with € at 0xD5.
#[rustfmt::skip]
const CP858_HI: [char; 128] = [
    'Ç','ü','é','â','ä','à','å','ç','ê','ë','è','ï','î','ì','Ä','Å',
    'É','æ','Æ','ô','ö','ò','û','ù','ÿ','Ö','Ü','ø','£','Ø','×','ƒ',
    'á','í','ó','ú','ñ','Ñ','ª','º','¿','®','¬','½','¼','¡','«','»',
    '░','▒','▓','│','┤','Á','Â','À','©','╣','║','╗','╝','¢','¥','┐',
    '└','┴','┬','├','─','┼','ã','Ã','╚','╔','╩','╦','╠','═','╬','¤',
    'ð','Ð','Ê','Ë','È','€','Í','Î','Ï','┘','┌','█','▄','¦','Ì','▀',
    'Ó','ß','Ô','Ò','õ','Õ','µ','þ','Þ','Ú','Û','Ù','ý','Ý','¯','´',
    '\u{ad}','±','‗','¾','¶','§','÷','¸','°','¨','·','¹','³','²','■','\u{a0}',
];

/// Windows-1252 0x80..=0x9F (0xA0..=0xFF equal Latin-1).
#[rustfmt::skip]
const WPC1252_80: [Option<char>; 32] = [
    Some('€'),None,Some('‚'),Some('ƒ'),Some('„'),Some('…'),Some('†'),Some('‡'),
    Some('ˆ'),Some('‰'),Some('Š'),Some('‹'),Some('Œ'),None,Some('Ž'),None,
    None,Some('‘'),Some('’'),Some('“'),Some('”'),Some('•'),Some('–'),Some('—'),
    Some('˜'),Some('™'),Some('š'),Some('›'),Some('œ'),None,Some('ž'),Some('Ÿ'),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodePage {
    /// PC437 (USA, standard Europe) — ESC t 0.
    Cp437,
    /// PC858 (Multilingual + €) — ESC t 19.
    Cp858,
    /// WPC1252 — ESC t 16.
    Wpc1252,
}

impl CodePage {
    pub fn escpos_table(self) -> u8 {
        match self {
            CodePage::Cp437 => 0,
            CodePage::Cp858 => 19,
            CodePage::Wpc1252 => 16,
        }
    }

    pub fn encode(self, c: char) -> Option<u8> {
        let u = c as u32;
        if u < 0x80 {
            return Some(u as u8);
        }
        match self {
            CodePage::Cp437 => CP437_HI
                .iter()
                .position(|&x| x == c)
                .map(|i| 0x80 + i as u8),
            CodePage::Cp858 => CP858_HI
                .iter()
                .position(|&x| x == c)
                .map(|i| 0x80 + i as u8),
            CodePage::Wpc1252 => {
                if (0xA0..=0xFF).contains(&u) {
                    Some(u as u8)
                } else {
                    WPC1252_80
                        .iter()
                        .position(|&x| x == Some(c))
                        .map(|i| 0x80 + i as u8)
                }
            }
        }
    }
}
