//! Immediate-mode software-rendered UI over `pos_hal::Display`.
//!
//! * [`Canvas`]: RGB888 back buffer with dirty-rectangle tracking; only the
//!   changed region is blitted to the display on [`Canvas::present`].
//! * Text: anti-aliased DejaVu Sans (regular and bold), embedded and
//!   subset, see [`text`]. Sizes follow the screen: the baseline design is
//!   1024×768 and every size and spacing scales by the canvas
//!   [`zoom`](Canvas::zoom) (e.g. ×1.41 at 1920×1080).
//! * [`Ui`]: per-frame widget helpers. Buttons light up while pressed and
//!   report a click only when the press and the release both land on them.
//! * [`scan::ScanDetector`]: keyboard-wedge barcode scanner detection.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod image;
pub mod scan;
mod text;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use pos_hal::{Display, HalResult, Rgb888};
use text::{Face, Fonts};

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
    pub fn split_right(&self, w: i32) -> (Rect, Rect) {
        let w = w.clamp(0, self.w);
        (
            Rect::new(self.x, self.y, self.w - w, self.h),
            Rect::new(self.x + self.w - w, self.y, w, self.h),
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
    /// Centred sub-rectangle of at most `w`×`h`.
    pub fn centered(&self, w: i32, h: i32) -> Rect {
        let w = w.min(self.w);
        let h = h.min(self.h);
        Rect::new(self.x + (self.w - w) / 2, self.y + (self.h - h) / 2, w, h)
    }
    /// `cols`×`rows` equal cells, row by row, with `gap` between them.
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
    /// Cell spanning `span` columns starting at `cell` in a row of `cells`.
    pub fn span(cells: &[Rect], first: usize, span: usize) -> Rect {
        let a = cells[first];
        let b = cells[first + span - 1];
        a.union(&b)
    }
    fn union(&self, o: &Rect) -> Rect {
        let x0 = self.x.min(o.x);
        let y0 = self.y.min(o.y);
        let x1 = (self.x + self.w).max(o.x + o.w);
        let y1 = (self.y + self.h).max(o.y + o.h);
        Rect::new(x0, y0, x1 - x0, y1 - y0)
    }
}

/// Text styles. Pixel sizes are for the 1024×768 baseline and scale with
/// the canvas zoom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Font {
    /// 14 px regular (hints, secondary text)
    Small,
    /// 19 px regular (body text, lists)
    Normal,
    /// 19 px bold (buttons, emphasis)
    Bold,
    /// 32 px bold (titles, totals)
    Large,
    /// 60 px bold (change due)
    Huge,
}

impl Font {
    /// Largest to smallest, for fitting labels.
    pub const FIT_ORDER: [Font; 4] = [Font::Large, Font::Bold, Font::Normal, Font::Small];

    /// At most [`Font::Bold`] (Large is for single short labels).
    pub fn min_bold(self) -> Font {
        if self == Font::Large {
            Font::Bold
        } else {
            self
        }
    }

    fn base_px(self) -> i32 {
        match self {
            Font::Small => 14,
            Font::Normal | Font::Bold => 19,
            Font::Large => 32,
            Font::Huge => 60,
        }
    }
    fn bold(self) -> bool {
        matches!(self, Font::Bold | Font::Large | Font::Huge)
    }
}

/// RGB888 back buffer.
#[derive(Debug, Clone)]
pub struct Canvas {
    w: i32,
    h: i32,
    px: Vec<Rgb888>,
    dirty: Option<Rect>,
    zoom: i32,
    fonts: Fonts,
    /// Progress through [`WARM`] (glyphs rasterised ahead of use).
    warm_pos: usize,
}

/// Glyphs to rasterise ahead of first use, per font: printable ASCII and
/// the symbols the screens draw. [`Font::Huge`] only shows amounts.
const WARM_FONTS: [Font; 4] = [Font::Normal, Font::Bold, Font::Large, Font::Small];
const WARM_SYMBOLS: &str = "€£¥₹₦₱₽₺₩…←→⇧⌫◀▶✓✕•×›−";
const WARM_HUGE: &str = "0123456789.,−-";

impl Canvas {
    pub fn new(w: u32, h: u32) -> Canvas {
        // Per-mille scale against the 1024×768 design, kept within reason
        // for small panels and 4K.
        let zoom = ((w as i32 * 1000 / 1024).min(h as i32 * 1000 / 768)).clamp(700, 4000);
        Canvas {
            w: w as i32,
            h: h as i32,
            px: vec![0; (w * h) as usize],
            dirty: Some(Rect::new(0, 0, w as i32, h as i32)),
            zoom,
            fonts: Fonts::new(),
            warm_pos: 0,
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
    /// Scale against the 1024×768 design, in thousandths.
    pub fn zoom(&self) -> i32 {
        self.zoom
    }
    /// A baseline length (pixels at 1024×768) at this canvas's zoom.
    pub fn px(&self, base: i32) -> i32 {
        (base * self.zoom + 500) / 1000
    }
    pub fn pixel(&self, x: i32, y: i32) -> Option<Rgb888> {
        (x >= 0 && y >= 0 && x < self.w && y < self.h).then(|| self.px[(y * self.w + x) as usize])
    }

    #[inline]
    pub(crate) fn blend(&mut self, x: i32, y: i32, c: Rgb888, a: u8) {
        if a == 0 || x < 0 || y < 0 || x >= self.w || y >= self.h {
            return;
        }
        let i = (y * self.w + x) as usize;
        self.px[i] = mix(self.px[i], c, a);
    }

    pub(crate) fn mark(&mut self, r: Rect) {
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

    /// Filled rectangle with corners of radius `rad` (edge pixels blended).
    pub fn fill_round_rect(&mut self, r: Rect, rad: i32, c: Rgb888) {
        let rad = rad.min(r.w / 2).min(r.h / 2).max(0);
        if rad == 0 {
            self.fill_rect(r, c);
            return;
        }
        self.fill_rect(Rect::new(r.x, r.y + rad, r.w, r.h - 2 * rad), c);
        // Corner rows: integer circle with a one-pixel blended edge.
        let r2 = rad * rad;
        for dy in 0..rad {
            let yy = rad - dy; // distance from the arc centre, ≥ 1
            let mut inset = rad;
            while inset > 0 && (rad - inset + 1) * (rad - inset + 1) + yy * yy <= r2 + rad {
                inset -= 1;
            }
            // inset = first column (from the edge) fully inside.
            let top = r.y + dy;
            let bot = r.y + r.h - 1 - dy;
            self.fill_rect(Rect::new(r.x + inset, top, r.w - 2 * inset, 1), c);
            self.fill_rect(Rect::new(r.x + inset, bot, r.w - 2 * inset, 1), c);
            if inset > 0 {
                let xa = r.x + inset - 1;
                let xb = r.x + r.w - inset;
                for (x, y) in [(xa, top), (xb, top), (xa, bot), (xb, bot)] {
                    self.blend(x, y, c, 128);
                }
            }
        }
        self.mark(r);
    }

    pub fn frame_rect(&mut self, r: Rect, t: i32, c: Rgb888) {
        self.fill_rect(Rect::new(r.x, r.y, r.w, t), c);
        self.fill_rect(Rect::new(r.x, r.y + r.h - t, r.w, t), c);
        self.fill_rect(Rect::new(r.x, r.y, t, r.h), c);
        self.fill_rect(Rect::new(r.x + r.w - t, r.y, t, r.h), c);
    }

    fn face(&self, font: Font) -> Face {
        Face {
            bold: font.bold(),
            px: self.px(font.base_px()).clamp(6, 400) as u16,
        }
    }

    /// Width in pixels of `s` in `font`.
    pub fn text_width(&self, s: &str, font: Font) -> i32 {
        self.fonts.width(self.face(font), s)
    }
    /// Height of one line of `font` (ascent + descent).
    pub fn line_height(&self, font: Font) -> i32 {
        self.fonts.metrics(self.face(font)).height
    }

    /// Draw `s` with its top-left at (x, y). Returns the advance width.
    pub fn text(&mut self, x: i32, y: i32, s: &str, font: Font, c: Rgb888) -> i32 {
        let face = self.face(font);
        let m = self.fonts.metrics(face);
        let base = y + m.ascent;
        let mut pen = x;
        // Disjoint borrows: read glyphs from the cache, write pixels.
        let (cw, chh) = (self.w, self.h);
        let Canvas { fonts, px, .. } = self;
        for ch in s.chars() {
            let Some(g) = fonts.glyph(face, ch) else {
                continue;
            };
            let (gx0, gy0) = (pen + g.left, base + g.top);
            // Clip the glyph box to the canvas once, then blend rows.
            let xa = (-gx0).max(0);
            let xb = g.w.min(cw - gx0);
            let ya = (-gy0).max(0);
            let yb = g.h.min(chh - gy0);
            for gy in ya..yb {
                let row = ((gy0 + gy) * cw) as usize;
                let src = (gy * g.w) as usize;
                for gx in xa..xb {
                    let a = g.cov[src + gx as usize];
                    if a != 0 {
                        let i = row + (gx0 + gx) as usize;
                        px[i] = mix(px[i], c, a);
                    }
                }
            }
            pen += g.adv;
        }
        self.mark(Rect::new(x, y, pen - x + 2, m.height));
        pen - x
    }

    /// `s` cut to fit in `w` pixels, ending in `…` if it had to be cut.
    pub fn fit(&self, s: &str, font: Font, w: i32) -> String {
        if self.text_width(s, font) <= w {
            return s.into();
        }
        let ell = self.text_width("…", font);
        let mut out = String::new();
        let mut used = 0;
        for ch in s.chars() {
            let a = self.text_width(ch.encode_utf8(&mut [0; 4]), font);
            if used + a + ell > w {
                break;
            }
            used += a;
            out.push(ch);
        }
        out.push('…');
        out
    }

    /// Text aligned within `r`: `align` 0 = left, 1 = centre, 2 = right;
    /// vertically centred. Cut with `…` if it does not fit.
    pub fn text_in(&mut self, r: Rect, s: &str, font: Font, c: Rgb888, align: u8) {
        let shown = self.fit(s, font, r.w);
        let w = self.text_width(&shown, font);
        let h = self.line_height(font);
        let x = match align {
            0 => r.x,
            1 => r.x + (r.w - w) / 2,
            _ => r.x + r.w - w,
        };
        let y = r.y + (r.h - h) / 2;
        self.text(x, y, &shown, font, c);
    }

    /// Word-wrap `s` to lines no wider than `w`. A word longer than a line
    /// is broken between characters.
    pub fn wrap(&self, s: &str, font: Font, w: i32) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        let mut cur = String::new();
        for word in s.split_whitespace() {
            let cand = if cur.is_empty() {
                String::from(word)
            } else {
                alloc::format!("{cur} {word}")
            };
            if self.text_width(&cand, font) <= w {
                cur = cand;
                continue;
            }
            if !cur.is_empty() {
                lines.push(core::mem::take(&mut cur));
            }
            // Break over-long words.
            for ch in word.chars() {
                let mut next = cur.clone();
                next.push(ch);
                if !cur.is_empty() && self.text_width(&next, font) > w {
                    lines.push(core::mem::take(&mut cur));
                    cur.push(ch);
                } else {
                    cur = next;
                }
            }
        }
        if !cur.is_empty() {
            lines.push(cur);
        }
        lines
    }

    /// Does `s` fit in `r` within `max_lines` wrapped lines?
    pub fn fits(&self, s: &str, font: Font, r: Rect, max_lines: usize) -> bool {
        let lines = self.wrap(s, font, r.w);
        lines.len() <= max_lines
            && lines.len() as i32 * self.line_height(font) <= r.h
            && lines.iter().all(|l| self.text_width(l, font) <= r.w)
    }

    /// Wrapped, centred text in `r`; lines that do not fit are dropped and
    /// the last shown line ends in `…`.
    pub fn text_wrapped(&mut self, r: Rect, s: &str, font: Font, c: Rgb888) {
        let lh = self.line_height(font);
        let max = (r.h / lh.max(1)).max(1) as usize;
        let mut lines = self.wrap(s, font, r.w);
        if lines.len() > max {
            lines.truncate(max);
            if let Some(last) = lines.last_mut() {
                let cut = alloc::format!("{last}…");
                *last = self.fit(&cut, font, r.w);
                if !last.ends_with('…') {
                    last.push('…');
                }
            }
        }
        let total = lines.len() as i32 * lh;
        let mut y = r.y + (r.h - total) / 2;
        for l in &lines {
            self.text_in(Rect::new(r.x, y, r.w, lh), l, font, c, 1);
            y += lh;
        }
    }

    /// Rasterise up to `budget` glyphs that are likely to be drawn soon, so
    /// a screen's first appearance does not stall on outline rendering
    /// (slow where floating point is emulated in software, as on both
    /// tiers' targets). Returns false once everything is cached.
    pub fn warm(&mut self, budget: usize) -> bool {
        let per_font = ('\u{20}'..='\u{7e}').count() + WARM_SYMBOLS.chars().count();
        let total = per_font * WARM_FONTS.len() + WARM_HUGE.chars().count();
        let mut done = 0;
        while done < budget && self.warm_pos < total {
            let i = self.warm_pos;
            self.warm_pos += 1;
            let (font, ch) = if i < per_font * WARM_FONTS.len() {
                let f = WARM_FONTS[i / per_font];
                let k = i % per_font;
                let ch = ('\u{20}'..='\u{7e}')
                    .chain(WARM_SYMBOLS.chars())
                    .nth(k)
                    .unwrap_or(' ');
                (f, ch)
            } else {
                let k = i - per_font * WARM_FONTS.len();
                (Font::Huge, WARM_HUGE.chars().nth(k).unwrap_or('0'))
            };
            let face = self.face(font);
            let _ = self.fonts.glyph(face, ch);
            done += 1;
        }
        self.warm_pos < total
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

/// `a/255` of the way from `d` to `c`, per channel.
pub fn mix(d: Rgb888, c: Rgb888, a: u8) -> Rgb888 {
    let a = u32::from(a);
    let ch = |shift: u32| {
        let dv = (d >> shift) & 0xFF;
        let cv = (c >> shift) & 0xFF;
        ((dv * (255 - a) + cv * a + 127) / 255) << shift
    };
    ch(16) | ch(8) | ch(0)
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
    /// Drawn but inert (e.g. a key that does nothing on this screen).
    Disabled,
}

/// Pointer state for one frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PointerFrame {
    /// Where the pointer is held down right now (for pressed highlights).
    pub held: Option<(i32, i32)>,
    /// A completed tap: where it went down and where it was released.
    pub click: Option<((i32, i32), (i32, i32))>,
}

/// Per-frame UI context: canvas plus this frame's pointer state.
#[derive(Debug)]
pub struct Ui<'a> {
    pub c: &'a mut Canvas,
    pub theme: Theme,
    ptr: PointerFrame,
    /// The press began here (for highlight: only the widget it began on).
    held_from: Option<(i32, i32)>,
    /// Every tappable area drawn this frame, with its label (tests, and
    /// a record of what was on screen).
    pub targets: Vec<(Rect, String)>,
}

impl<'a> Ui<'a> {
    /// `held_from` is where the current press began, if one is in progress.
    pub fn new(c: &'a mut Canvas, ptr: PointerFrame, held_from: Option<(i32, i32)>) -> Ui<'a> {
        Ui {
            c,
            theme: THEME,
            ptr,
            held_from,
            targets: Vec::new(),
        }
    }

    /// Register a tappable area; true if it was tapped this frame.
    pub fn target(&mut self, r: Rect, label: &str) -> bool {
        self.targets.push((r, label.into()));
        self.clicked_in(r)
    }

    /// Unit of spacing (8 px at 1024×768).
    pub fn u(&self) -> i32 {
        self.c.px(8).max(4)
    }

    pub fn clear(&mut self) {
        let b = self.c.bounds();
        self.c.fill_rect(b, self.theme.bg);
    }

    pub fn panel(&mut self, r: Rect) {
        let rad = self.c.px(6);
        self.c.fill_round_rect(r, rad, self.theme.panel);
    }

    pub fn label(&mut self, r: Rect, s: &str, font: Font, align: u8) {
        self.c.text_in(r, s, font, self.theme.text, align);
    }
    pub fn label_color(&mut self, r: Rect, s: &str, font: Font, align: u8, color: Rgb888) {
        self.c.text_in(r, s, font, color, align);
    }

    /// Is a press that began inside `r` still held inside it?
    pub fn pressed_in(&self, r: Rect) -> bool {
        matches!((self.held_from, self.ptr.held),
            (Some((ax, ay)), Some((bx, by))) if r.contains(ax, ay) && r.contains(bx, by))
    }

    /// Was `r` tapped this frame (pressed and released inside it)?
    pub fn clicked_in(&self, r: Rect) -> bool {
        matches!(self.ptr.click,
            Some(((ax, ay), (bx, by))) if r.contains(ax, ay) && r.contains(bx, by))
    }

    /// The largest font in which every label fits a `cell` (one line in
    /// [`Font::Large`], up to three otherwise), so a grid of buttons reads
    /// as one set.
    pub fn grid_font<'s>(&self, cell: Rect, labels: impl IntoIterator<Item = &'s str>) -> Font {
        let labels: Vec<&str> = labels.into_iter().collect();
        let inner = self.label_area(cell);
        for f in Font::FIT_ORDER {
            let lines = if f == Font::Large { 1 } else { 3 };
            if labels.iter().all(|l| self.c.fits(l, f, inner, lines)) {
                return f;
            }
        }
        Font::Small
    }

    fn label_area(&self, r: Rect) -> Rect {
        let pad = self.u() / 2 + 1;
        r.inset(pad)
    }

    /// Draw a button with the best-fitting font; true if tapped this frame.
    pub fn button(&mut self, r: Rect, label: &str, style: ButtonStyle) -> bool {
        let f = self.grid_font(r, [label]);
        self.button_font(r, label, style, f)
    }

    /// Draw a button in `font` (wrapped to fit); true if tapped this frame.
    pub fn button_font(&mut self, r: Rect, label: &str, style: ButtonStyle, font: Font) -> bool {
        let (bg, fg) = match style {
            ButtonStyle::Normal => (self.theme.button, self.theme.button_text),
            ButtonStyle::Primary => (self.theme.accent, self.theme.button_text),
            ButtonStyle::Danger => (self.theme.danger, self.theme.button_text),
            ButtonStyle::Ok => (self.theme.ok, self.theme.button_text),
            ButtonStyle::Disabled => (self.theme.panel, self.theme.muted),
        };
        let live = style != ButtonStyle::Disabled;
        let pressed = live && self.pressed_in(r);
        let bg = if pressed {
            mix(bg, 0x00FF_FFFF, 90)
        } else {
            bg
        };
        let rad = self.c.px(6);
        self.c.fill_round_rect(r, rad, bg);
        if pressed {
            let t = self.c.px(2).max(1);
            self.c.frame_rect(r.inset(t), t, mix(bg, 0x00FF_FFFF, 160));
        }
        let inner = self.label_area(r);
        self.c.text_wrapped(inner, label, font, fg);
        live && self.target(r, label)
    }

    /// A row in a list; `selected` rows are highlighted. Returns true if tapped.
    pub fn row(&mut self, r: Rect, cols: &[(&str, i32, u8)], selected: bool) -> bool {
        if selected {
            self.c.fill_rect(r, self.theme.select);
        } else if self.pressed_in(r) {
            self.c.fill_rect(r, self.theme.button);
        }
        let pad = self.u() / 2;
        let mut x = r.x + pad;
        for &(s, w, align) in cols {
            self.c.text_in(
                Rect::new(x, r.y, w - 2 * pad, r.h),
                s,
                Font::Normal,
                self.theme.text,
                align,
            );
            x += w;
        }
        let label = cols.iter().map(|c| c.0).collect::<Vec<_>>().join(" ");
        self.target(r, label.trim())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::borrow::ToOwned;
    use alloc::string::ToString;
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

    /// Every currency and UI symbol the app uses has a real glyph (not the
    /// empty "missing glyph" box), with anti-aliased edges.
    #[test]
    fn text_draws_every_needed_glyph() {
        let mut c = Canvas::new(1024, 768);
        let set = "Aé€£¥₹₦₱₽₺₩ŠŒ…←→⇧⌫◀▶✓✕•×›−·";
        let mut x = 0;
        for ch in set.chars() {
            let s = ch.encode_utf8(&mut [0; 4]).to_owned();
            c.fill_rect(c.bounds(), 0);
            let w = c.text(0, 0, &s, Font::Large, 0xFFFFFF);
            assert!(w > 0, "{ch} has no advance");
            let h = c.line_height(Font::Large);
            let lit: Vec<u32> = (0..h)
                .flat_map(|y| (0..w).map(move |x| (x, y)))
                .filter_map(|(x, y)| c.pixel(x, y))
                .filter(|p| *p != 0)
                .collect();
            assert!(lit.len() > 10, "{ch} drew nothing");
            assert!(
                lit.iter().any(|p| *p != 0xFFFFFF),
                "{ch} has no anti-aliasing"
            );
            x += w;
        }
        assert!(x > 0);
    }

    #[test]
    fn digits_are_tabular() {
        let c = Canvas::new(1024, 768);
        let w0 = c.text_width("0", Font::Normal);
        for d in "123456789".chars() {
            assert_eq!(c.text_width(&d.to_string(), Font::Normal), w0);
        }
    }

    #[test]
    fn fit_and_wrap() {
        let c = Canvas::new(1024, 768);
        let s = "Long product name that does not fit";
        let cut = c.fit(s, Font::Normal, 100);
        assert!(cut.ends_with('…'));
        assert!(c.text_width(&cut, Font::Normal) <= 100);
        let lines = c.wrap(s, Font::Normal, 120);
        assert!(lines.len() > 1);
        assert!(lines.iter().all(|l| c.text_width(l, Font::Normal) <= 120));
        assert_eq!(c.fit("Milk", Font::Normal, 100), "Milk");
    }

    #[test]
    fn button_tap_needs_press_and_release_inside() {
        let mut c = Canvas::new(1024, 768);
        let r = Rect::new(100, 100, 80, 40);
        let tap = |down, up| PointerFrame {
            held: None,
            click: Some((down, up)),
        };
        let mut ui = Ui::new(&mut c, tap((120, 110), (125, 115)), None);
        assert!(ui.button(r, "OK", ButtonStyle::Primary));
        assert!(!ui.button(Rect::new(0, 0, 10, 10), "X", ButtonStyle::Normal));
        assert!(!ui.button(r, "OK", ButtonStyle::Disabled));
        // Slid off the button before release: no tap.
        let mut ui = Ui::new(&mut c, tap((120, 110), (300, 300)), None);
        assert!(!ui.button(r, "OK", ButtonStyle::Primary));
    }

    #[test]
    fn pressed_button_is_highlighted() {
        let mut c = Canvas::new(1024, 768);
        let r = Rect::new(100, 100, 80, 40);
        let held = PointerFrame {
            held: Some((105, 135)),
            click: None,
        };
        let mut ui = Ui::new(&mut c, held, Some((105, 135)));
        ui.button(r, "", ButtonStyle::Normal);
        let lit = c.pixel(140, 120).unwrap();
        let mut c2 = Canvas::new(1024, 768);
        let mut ui = Ui::new(&mut c2, PointerFrame::default(), None);
        ui.button(r, "", ButtonStyle::Normal);
        assert_eq!(c2.pixel(140, 120), Some(THEME.button));
        assert_ne!(lit, THEME.button);
    }

    #[test]
    fn grid_font_is_shared() {
        let c = Canvas::new(1024, 768);
        let mut c = c;
        let ui = Ui::new(&mut c, PointerFrame::default(), None);
        let cell = Rect::new(0, 0, 120, 90);
        assert_eq!(ui.grid_font(cell, ["Bread"]), Font::Large);
        // One long label pulls the whole grid down to the same size.
        assert_ne!(
            ui.grid_font(cell, ["Bread", "Milk 1L long-life"]),
            Font::Large
        );
    }

    #[test]
    fn warm_finishes_and_matches_drawing() {
        let mut a = Canvas::new(1024, 768);
        while a.warm(50) {}
        let mut b = Canvas::new(1024, 768);
        a.fill_rect(a.bounds(), 0);
        b.fill_rect(b.bounds(), 0);
        a.text(5, 5, "Total 18.99 €", Font::Large, 0xFFFFFF);
        b.text(5, 5, "Total 18.99 €", Font::Large, 0xFFFFFF);
        assert!(a.px == b.px);
    }

    #[test]
    fn zoom_from_resolution() {
        assert_eq!(Canvas::new(1024, 768).zoom(), 1000);
        assert_eq!(Canvas::new(1280, 800).zoom(), 1041);
        assert_eq!(Canvas::new(1920, 1080).zoom(), 1406);
        assert_eq!(Canvas::new(2048, 1536).zoom(), 2000);
        let c = Canvas::new(1920, 1080);
        assert!(c.line_height(Font::Normal) > Canvas::new(1024, 768).line_height(Font::Normal));
    }
}
