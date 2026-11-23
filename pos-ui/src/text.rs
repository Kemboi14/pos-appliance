//! Anti-aliased text from embedded DejaVu Sans (regular and bold).
//!
//! The fonts are subset to Latin-1, the Latin-9 extras (`€ Š š Ž ž Œ œ Ÿ`),
//! the currency signs the app can be configured with (`₹ ₦ ₱ ₽ ₺ ₩`), and a
//! few UI symbols (`… ← → ⇧ ⌫ ◀ ▶ ✓ ✕ •`). See `fonts/README.md`.
//!
//! Glyphs are rasterised once per (face, pixel size, char) into 8-bit
//! coverage maps and cached; drawing blends the coverage over the canvas.
//! Pen positions are whole pixels, so a cached glyph looks the same wherever
//! it is drawn and widths can be measured without rasterising.
//!
//! Floating point is used here only for outline rasterisation (ab_glyph), never
//! for money: hence the module-level allow of the workspace's
//! `float_arithmetic` lint.
#![allow(clippy::float_arithmetic)]

use ab_glyph::{Font as _, FontRef, PxScale, ScaleFont as _};
use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

static REGULAR: &[u8] = include_bytes!("../fonts/DejaVuSans.ttf");
static BOLD: &[u8] = include_bytes!("../fonts/DejaVuSans-Bold.ttf");

/// One rasterised glyph.
#[derive(Debug, Clone)]
pub(crate) struct Glyph {
    /// Offset of the bitmap's top-left from the pen (x) and baseline (y).
    pub left: i32,
    pub top: i32,
    pub w: i32,
    pub h: i32,
    /// Whole-pixel advance (same as [`Fonts::advance`]).
    pub adv: i32,
    pub cov: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Face {
    pub bold: bool,
    pub px: u16,
}

#[derive(Clone)]
pub(crate) struct Fonts {
    regular: Option<FontRef<'static>>,
    bold: Option<FontRef<'static>>,
    cache: BTreeMap<(Face, char), Glyph>,
    /// Advances and metrics are asked for many times per frame (layout
    /// measures every label); cache them so the float maths, emulated in
    /// software on the kernel target, runs once per face and char.
    advances: RefCell<BTreeMap<(Face, char), i32>>,
    metrics: RefCell<BTreeMap<Face, Metrics>>,
}

impl core::fmt::Debug for Fonts {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Fonts")
            .field("cached", &self.cache.len())
            .finish()
    }
}

/// Vertical metrics in whole pixels.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Metrics {
    pub ascent: i32,
    pub height: i32,
}

impl Fonts {
    pub fn new() -> Fonts {
        Fonts {
            regular: FontRef::try_from_slice(REGULAR).ok(),
            bold: FontRef::try_from_slice(BOLD).ok(),
            cache: BTreeMap::new(),
            advances: RefCell::new(BTreeMap::new()),
            metrics: RefCell::new(BTreeMap::new()),
        }
    }

    fn font(&self, f: Face) -> Option<&FontRef<'static>> {
        if f.bold {
            self.bold.as_ref().or(self.regular.as_ref())
        } else {
            self.regular.as_ref()
        }
    }

    pub fn metrics(&self, f: Face) -> Metrics {
        if let Some(m) = self.metrics.borrow().get(&f) {
            return *m;
        }
        let m = self.compute_metrics(f);
        self.metrics.borrow_mut().insert(f, m);
        m
    }

    fn compute_metrics(&self, f: Face) -> Metrics {
        match self.font(f) {
            Some(font) => {
                let s = font.as_scaled(PxScale::from(f32::from(f.px)));
                let ascent = libm::ceilf(s.ascent()) as i32;
                let descent = libm::ceilf(-s.descent()) as i32;
                Metrics {
                    ascent,
                    height: ascent + descent,
                }
            }
            None => Metrics {
                ascent: i32::from(f.px),
                height: i32::from(f.px),
            },
        }
    }

    /// Whole-pixel advance of `c`.
    pub fn advance(&self, f: Face, c: char) -> i32 {
        if let Some(a) = self.advances.borrow().get(&(f, c)) {
            return *a;
        }
        let a = self.compute_advance(f, c);
        self.advances.borrow_mut().insert((f, c), a);
        a
    }

    fn compute_advance(&self, f: Face, c: char) -> i32 {
        match self.font(f) {
            Some(font) => {
                let s = font.as_scaled(PxScale::from(f32::from(f.px)));
                libm::roundf(s.h_advance(font.glyph_id(c))) as i32
            }
            None => i32::from(f.px) / 2,
        }
    }

    pub fn width(&self, f: Face, s: &str) -> i32 {
        s.chars().map(|c| self.advance(f, c)).sum()
    }

    pub fn glyph(&mut self, f: Face, c: char) -> Option<&Glyph> {
        if !self.cache.contains_key(&(f, c)) {
            let g = self.raster(f, c)?;
            self.cache.insert((f, c), g);
        }
        self.cache.get(&(f, c))
    }

    fn raster(&self, f: Face, c: char) -> Option<Glyph> {
        let font = self.font(f)?;
        let adv = self.advance(f, c);
        let g = font
            .glyph_id(c)
            .with_scale_and_position(PxScale::from(f32::from(f.px)), ab_glyph::point(0.0, 0.0));
        let Some(og) = font.outline_glyph(g) else {
            // Whitespace: nothing to draw.
            return Some(Glyph {
                left: 0,
                top: 0,
                w: 0,
                h: 0,
                adv,
                cov: Vec::new(),
            });
        };
        let b = og.px_bounds();
        let w = libm::ceilf(b.width()) as i32;
        let h = libm::ceilf(b.height()) as i32;
        let mut cov = vec![0u8; (w.max(0) * h.max(0)) as usize];
        og.draw(|x, y, a| {
            let (x, y) = (x as i32, y as i32);
            if x < w && y < h {
                cov[(y * w + x) as usize] = libm::roundf(a.clamp(0.0, 1.0) * 255.0) as u8;
            }
        });
        Some(Glyph {
            left: b.min.x as i32,
            top: b.min.y as i32,
            w,
            h,
            adv,
            cov,
        })
    }
}
