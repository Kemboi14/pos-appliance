//! Immediate-mode software-rendered UI over `pos_hal::Display`.
//!
//! * [`Canvas`]: RGB888 back buffer with dirty-rectangle tracking; only the
//!   changed region is blitted to the display on [`Canvas::present`].
//! * Text: X11 "misc-fixed" bitmap fonts (public domain) from
//!   embedded-graphics, ISO 8859-1 for Latin-1, ISO 8859-15 for `€`, plus a
//!   few hand-drawn currency glyphs (₹ ₦ ₱ ₽ ₺ ₩) in [`glyphs`].
//! * [`Ui`]: per-frame widget helpers (buttons that report pointer clicks).
//! * [`scan::ScanDetector`]: keyboard-wedge barcode scanner detection.
//!
//! Layout is computed from the actual screen size; the baseline design is
//! 1024×768 and everything scales by integer factors above that.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod glyphs;
pub mod scan;

use alloc::vec;
use alloc::vec::Vec;
use embedded_graphics::mono_font::{MonoFont, MonoTextStyle, iso_8859_1, iso_8859_15};
use embedded_graphics::pixelcolor::Rgb888 as EgRgb;
use embedded_graphics::prelude::*;
use embedded_graphics::text::{Baseline, Text};
use pos_hal::{Display, HalResult, Rgb888};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { x, y, w, h }
    }
    pub fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && py >= self.y && px < self.x + self.w && py < self.y + self.h
    }
    pub fn inset(&self, d: i32) -> Rect {
        Rect::new(
            self.x + d,
            self.y + d,
            (self.w - 2 * d).max(0),
            (self.h - 2 * d).max(0),
        )
    }
    /// Split horizontally: left part of width `w`, remainder.
    pub fn split_left(&self, w: i32) -> (Rect, Rect) {
        let w = w.clamp(0, self.w);
        (
            Rect::new(self.x, self.y, w, self.h),
            Rect::new(self.x + w, self.y, self.w - w, self.h),
        )
    }
    pub fn split_top(&self, h: i32) -> (Rect, Rect) {
        let h = h.clamp(0, self.h);
        (
            Rect::new(self.x, self.y, self.w, h),
            Rect::new(self.x, self.y + h, self.w, self.h - h),
        )
    }
    pub fn split_bottom(&self, h: i32) -> (Rect, Rect) {
        let h = h.clamp(0, self.h);
        (
            Rect::new(self.x, self.y, self.w, self.h - h),
            Rect::new(self.x, self.y + self.h - h, self.w, h),
        )
    }
    /// `n` equal cells in a `cols`-column grid, with `gap` between them.
    pub fn grid(&self, cols: i32, rows: i32, gap: i32) -> Vec<Rect> {
        let cw = (self.w - gap * (cols - 1)) / cols.max(1);
        let ch = (self.h - gap * (rows - 1)) / rows.max(1);
        let mut v = Vec::new();
        for r in 0..rows {
            for c in 0..cols {
                v.push(Rect::new(
                    self.x + c * (cw + gap),
                    self.y + r * (ch + gap),
                    cw,
                    ch,
                ));
            }
        }
        v
    }
    fn union(&self, o: &Rect) -> Rect {
        let x0 = self.x.min(o.x);
        let y0 = self.y.min(o.y);
        let x1 = (self.x + self.w).max(o.x + o.w);
        let y1 = (self.y + self.h).max(o.y + o.h);
        Rect::new(x0, y0, x1 - x0, y1 - y0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Font {
    /// 6×13
    Small,
    /// 10×20
    Normal,
    /// 10×20 at 2× (totals, big numbers)
    Large,
}

impl Font {
    fn mono(self) -> (&'static MonoFont<'static>, &'static MonoFont<'static>, i32) {
        match self {
            Font::Small => (&iso_8859_1::FONT_6X13, &iso_8859_15::FONT_6X13, 1),
            Font::Normal => (&iso_8859_1::FONT_10X20, &iso_8859_15::FONT_10X20, 1),
            Font::Large => (&iso_8859_1::FONT_10X20, &iso_8859_15::FONT_10X20, 2),
        }
    }
    /// Cell size in pixels at UI scale 1.
    pub fn cell(self) -> (i32, i32) {
        let (f, _, s) = self.mono();
        (
            f.character_size.width as i32 * s,
            f.character_size.height as i32 * s,
        )
    }
}

/// RGB888 back buffer.
#[derive(Debug, Clone)]
pub struct Canvas {
    w: i32,
    h: i32,
    px: Vec<Rgb888>,
    dirty: Option<Rect>,
    /// Integer UI scale (1 at ≤ 1920×1080).
    pub scale: i32,
}

/// Drawing target that scales each pixel to an s×s block, used for text.
struct Scaled<'a> {
    c: &'a mut Canvas,
    ox: i32,
    oy: i32,
    s: i32,
}

impl OriginDimensions for Scaled<'_> {
    fn size(&self) -> Size {
        Size::new(self.c.w as u32, self.c.h as u32)
    }
}

impl DrawTarget for Scaled<'_> {
    type Color = EgRgb;
    type Error = core::convert::Infallible;
    fn draw_iter<I: IntoIterator<Item = Pixel<EgRgb>>>(
        &mut self,
        pixels: I,
    ) -> Result<(), Self::Error> {
        for Pixel(p, c) in pixels {
            let v = (u32::from(c.r()) << 16) | (u32::from(c.g()) << 8) | u32::from(c.b());
            let x = self.ox + p.x * self.s;
            let y = self.oy + p.y * self.s;
            for dy in 0..self.s {
                for dx in 0..self.s {
                    self.c.put(x + dx, y + dy, v);
                }
            }
        }
        Ok(())
    }
}

fn eg(c: Rgb888) -> EgRgb {
    EgRgb::new((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

impl Canvas {
    pub fn new(w: u32, h: u32) -> Canvas {
        let scale = ((w / 1024).min(h / 768)).max(1) as i32;
        Canvas {
            w: w as i32,
            h: h as i32,
            px: vec![0; (w * h) as usize],
            dirty: Some(Rect::new(0, 0, w as i32, h as i32)),
            scale,
        }
    }
    pub fn width(&self) -> i32 {
        self.w
    }
    pub fn height(&self) -> i32 {
        self.h
    }
    pub fn bounds(&self) -> Rect {
        Rect::new(0, 0, self.w, self.h)
    }
    pub fn pixel(&self, x: i32, y: i32) -> Option<Rgb888> {
        (x >= 0 && y >= 0 && x < self.w && y < self.h).then(|| self.px[(y * self.w + x) as usize])
    }

    #[inline]
    fn put(&mut self, x: i32, y: i32, c: Rgb888) {
        if x >= 0 && y >= 0 && x < self.w && y < self.h {
            self.px[(y * self.w + x) as usize] = c;
        }
    }

    fn mark(&mut self, r: Rect) {
        let r = clip(r, self.bounds());
        if r.w <= 0 || r.h <= 0 {
            return;
        }
        self.dirty = Some(match self.dirty {
            Some(d) => d.union(&r),
            None => r,
        });
    }

    pub fn fill_rect(&mut self, r: Rect, c: Rgb888) {
        let r = clip(r, self.bounds());
        if r.w <= 0 || r.h <= 0 {
            return;
        }
        for y in r.y..r.y + r.h {
            let row = (y * self.w) as usize;
            self.px[row + r.x as usize..row + (r.x + r.w) as usize].fill(c);
        }
        self.mark(r);
    }

    pub fn frame_rect(&mut self, r: Rect, t: i32, c: Rgb888) {
        self.fill_rect(Rect::new(r.x, r.y, r.w, t), c);
        self.fill_rect(Rect::new(r.x, r.y + r.h - t, r.w, t), c);
        self.fill_rect(Rect::new(r.x, r.y, t, r.h), c);
        self.fill_rect(Rect::new(r.x + r.w - t, r.y, t, r.h), c);
    }

    /// Width in pixels of `s` in `font`.
    pub fn text_width(&self, s: &str, font: Font) -> i32 {
        font.cell().0 * self.scale * s.chars().count() as i32
    }
    pub fn line_height(&self, font: Font) -> i32 {
        font.cell().1 * self.scale
    }

    /// Draw `s` with its top-left at (x, y). Returns the advance width.
    pub fn text(&mut self, x: i32, y: i32, s: &str, font: Font, c: Rgb888) -> i32 {
        let (latin1, latin9, fs) = font.mono();
        let s_total = fs * self.scale;
        let (cw, ch) = font.cell();
        let (cw, ch) = (cw * self.scale, ch * self.scale);
        let mut cx = x;
        let mut buf = [0u8; 4];
        for chr in s.chars() {
            let mono = if u32::from(chr) < 0x100 {
                Some(latin1)
            } else if matches!(chr, '€' | 'Š' | 'š' | 'Ž' | 'ž' | 'Œ' | 'œ' | 'Ÿ') {
                Some(latin9)
            } else {
                None
            };
            match mono {
                Some(m) => {
                    let style = MonoTextStyle::new(m, eg(c));
                    let mut t = Scaled {
                        c: self,
                        ox: cx,
                        oy: y,
                        s: s_total,
                    };
                    let _ = Text::with_baseline(
                        chr.encode_utf8(&mut buf),
                        Point::zero(),
                        style,
                        Baseline::Top,
                    )
                    .draw(&mut t);
                }
                None => {
                    let g = glyphs::glyph(chr);
                    // Hand-drawn glyphs are 10×20; scale to the font cell.
                    for (row, bits) in g.iter().enumerate() {
                        for col in 0..10 {
                            if bits & (1 << (9 - col)) != 0 {
                                let x0 = cx + col * cw / 10;
                                let x1 = cx + (col + 1) * cw / 10;
                                let y0 = y + row as i32 * ch / 20;
                                let y1 = y + (row as i32 + 1) * ch / 20;
                                for yy in y0..y1.max(y0 + 1) {
                                    for xx in x0..x1.max(x0 + 1) {
                                        self.put(xx, yy, c);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            cx += cw;
        }
        self.mark(Rect::new(x, y, cx - x, ch));
        cx - x
    }

    /// Text aligned within `r`: `align` 0 = left, 1 = centre, 2 = right;
    /// vertically centred. Truncated with `~` if it does not fit (the fonts have no `…`).
    pub fn text_in(&mut self, r: Rect, s: &str, font: Font, c: Rgb888, align: u8) {
        let (cw, ch) = font.cell();
        let cw = cw * self.scale;
        let ch = ch * self.scale;
        let max_chars = (r.w / cw.max(1)).max(0) as usize;
        let n = s.chars().count();
        let shown: alloc::string::String = if n > max_chars && max_chars > 0 {
            s.chars()
                .take(max_chars - 1)
                // '…' is not in the Latin-1/-15 fonts; '~' is.
                .chain(core::iter::once('~'))
                .collect()
        } else {
            s.into()
        };
        let w = cw * shown.chars().count() as i32;
        let x = match align {
            0 => r.x,
            1 => r.x + (r.w - w) / 2,
            _ => r.x + r.w - w,
        };
        let y = r.y + (r.h - ch) / 2;
        self.text(x, y, &shown, font, c);
    }

    /// Blit the dirty region (if any) to the display.
    pub fn present(&mut self, d: &mut dyn Display) -> HalResult<()> {
        if let Some(r) = self.dirty.take() {
            let off = (r.y * self.w + r.x) as usize;
            d.blit(
                &self.px[off..],
                self.w as usize,
                r.x as u32,
                r.y as u32,
                r.w as u32,
                r.h as u32,
            )?;
        }
        Ok(())
    }

    /// Force the next `present` to redraw everything.
    pub fn invalidate(&mut self) {
        self.dirty = Some(self.bounds());
    }
}

fn clip(r: Rect, b: Rect) -> Rect {
    let x0 = r.x.max(b.x);
    let y0 = r.y.max(b.y);
    let x1 = (r.x + r.w).min(b.x + b.w);
    let y1 = (r.y + r.h).min(b.y + b.h);
    Rect::new(x0, y0, (x1 - x0).max(0), (y1 - y0).max(0))
}

/// Colour theme (high contrast, readable under shop lighting).
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub bg: Rgb888,
    pub panel: Rgb888,
    pub text: Rgb888,
    pub muted: Rgb888,
    pub accent: Rgb888,
    pub button: Rgb888,
    pub button_text: Rgb888,
    pub danger: Rgb888,
    pub ok: Rgb888,
    pub warn: Rgb888,
    pub select: Rgb888,
}

pub const THEME: Theme = Theme {
    bg: 0x0016_1B22,
    panel: 0x0022_2A35,
    text: 0x00EE_F2F6,
    muted: 0x0098_A4B3,
    accent: 0x002F_81F7,
    button: 0x0033_3D4B,
    button_text: 0x00FF_FFFF,
    danger: 0x00D7_3A49,
    ok: 0x0028_A745,
    warn: 0x00E3_B341,
    select: 0x0035_5C9A,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonStyle {
    Normal,
    Primary,
    Danger,
    Ok,
}

/// Per-frame UI context: canvas + the pointer click (if any) delivered for
/// this frame.
#[derive(Debug)]
pub struct Ui<'a> {
    pub c: &'a mut Canvas,
    pub theme: Theme,
    click: Option<(i32, i32)>,
}

impl<'a> Ui<'a> {
    pub fn new(c: &'a mut Canvas, click: Option<(i32, i32)>) -> Ui<'a> {
        Ui {
            c,
            theme: THEME,
            click,
        }
    }

    /// Unit of spacing (8 px at scale 1).
    pub fn u(&self) -> i32 {
        8 * self.c.scale
    }

    pub fn clear(&mut self) {
        let b = self.c.bounds();
        self.c.fill_rect(b, self.theme.bg);
    }

    pub fn panel(&mut self, r: Rect) {
        self.c.fill_rect(r, self.theme.panel);
    }

    pub fn label(&mut self, r: Rect, s: &str, font: Font, align: u8) {
        self.c.text_in(r, s, font, self.theme.text, align);
    }
    pub fn label_color(&mut self, r: Rect, s: &str, font: Font, align: u8, color: Rgb888) {
        self.c.text_in(r, s, font, color, align);
    }

    /// Draw a button; returns true if it was clicked this frame.
    pub fn button(&mut self, r: Rect, label: &str, style: ButtonStyle) -> bool {
        let bg = match style {
            ButtonStyle::Normal => self.theme.button,
            ButtonStyle::Primary => self.theme.accent,
            ButtonStyle::Danger => self.theme.danger,
            ButtonStyle::Ok => self.theme.ok,
        };
        self.c.fill_rect(r, bg);
        self.c.frame_rect(r, 1, self.theme.bg);
        // Large only when the label fits on one line at that size.
        let large_w = label.chars().count() as i32 * Font::Large.cell().0 * self.c.scale;
        let font = if r.h >= 3 * Font::Normal.cell().1 * self.c.scale
            && label.chars().count() <= 6
            && large_w <= r.inset(4).w
        {
            Font::Large
        } else {
            Font::Normal
        };
        self.c
            .text_in(r.inset(4), label, font, self.theme.button_text, 1);
        matches!(self.click, Some((x, y)) if r.contains(x, y))
    }

    /// A row in a list; `selected` rows are highlighted. Returns true if clicked.
    pub fn row(&mut self, r: Rect, cols: &[(&str, i32, u8)], selected: bool) -> bool {
        if selected {
            self.c.fill_rect(r, self.theme.select);
        }
        let mut x = r.x + 4;
        for &(s, w, align) in cols {
            self.c.text_in(
                Rect::new(x, r.y, w - 8, r.h),
                s,
                Font::Normal,
                self.theme.text,
                align,
            );
            x += w;
        }
        matches!(self.click, Some((cx, cy)) if r.contains(cx, cy))
    }

    pub fn clicked_in(&self, r: Rect) -> bool {
        matches!(self.click, Some((x, y)) if r.contains(x, y))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pos_hal::{HalError, PixelFormat};

    struct MockDisplay {
        w: u32,
        h: u32,
        fb: Vec<u32>,
        blits: u32,
    }
    impl Display for MockDisplay {
        fn width(&self) -> u32 {
            self.w
        }
        fn height(&self) -> u32 {
            self.h
        }
        fn pixel_format(&self) -> PixelFormat {
            PixelFormat::Bgrx
        }
        fn blit(
            &mut self,
            src: &[u32],
            stride: usize,
            x: u32,
            y: u32,
            w: u32,
            h: u32,
        ) -> HalResult<()> {
            if x + w > self.w || y + h > self.h {
                return Err(HalError::InvalidArgument);
            }
            for r in 0..h as usize {
                for c in 0..w as usize {
                    self.fb[(y as usize + r) * self.w as usize + x as usize + c] =
                        src[r * stride + c];
                }
            }
            self.blits += 1;
            Ok(())
        }
    }

    #[test]
    fn dirty_rect_present() {
        let mut c = Canvas::new(1024, 768);
        let mut d = MockDisplay {
            w: 1024,
            h: 768,
            fb: vec![0; 1024 * 768],
            blits: 0,
        };
        c.fill_rect(c.bounds(), 0x112233);
        c.present(&mut d).unwrap();
        assert_eq!(d.fb[0], 0x112233);
        // Nothing dirty: no blit.
        c.present(&mut d).unwrap();
        assert_eq!(d.blits, 1);
        c.fill_rect(Rect::new(10, 10, 5, 5), 0xFF0000);
        c.present(&mut d).unwrap();
        assert_eq!(d.fb[12 * 1024 + 12], 0xFF0000);
        assert_eq!(d.blits, 2);
    }

    #[test]
    fn text_draws_pixels_and_measures() {
        let mut c = Canvas::new(1024, 768);
        let w = c.text(0, 0, "€£¥₹A", Font::Normal, 0xFFFFFF);
        assert_eq!(w, 50);
        // Each glyph cell has at least one lit pixel.
        for g in 0..5 {
            let lit = (0..20).any(|y| (0..10).any(|x| c.pixel(g * 10 + x, y) == Some(0xFFFFFF)));
            assert!(lit, "glyph {g} empty");
        }
    }

    #[test]
    fn button_click_hit_test() {
        let mut c = Canvas::new(1024, 768);
        let r = Rect::new(100, 100, 80, 40);
        let mut ui = Ui::new(&mut c, Some((120, 110)));
        assert!(ui.button(r, "OK", ButtonStyle::Primary));
        assert!(!ui.button(Rect::new(0, 0, 10, 10), "X", ButtonStyle::Normal));
    }

    #[test]
    fn scale_from_resolution() {
        assert_eq!(Canvas::new(1024, 768).scale, 1);
        assert_eq!(Canvas::new(1920, 1080).scale, 1);
        assert_eq!(Canvas::new(2048, 1536).scale, 2);
    }
}
