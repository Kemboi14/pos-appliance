//! Pictures: decoding uploaded PNG/JPEG files, shrinking them to the sizes
//! the screens use, a compact lossless storage format (QOI), and drawing
//! with scaling, alpha and rounded corners.
//!
//! Integer arithmetic only; scaling is fixed-point (16.16) bilinear.

use alloc::vec;
use alloc::vec::Vec;

use crate::{Canvas, Rect, Rgb888, mix};

/// 0xAARRGGBB pixels, row-major.
#[derive(Clone, PartialEq, Eq)]
pub struct Bitmap {
    pub w: i32,
    pub h: i32,
    pub px: Vec<u32>,
}

impl core::fmt::Debug for Bitmap {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Bitmap({}x{})", self.w, self.h)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageError {
    /// Not a PNG or JPEG file.
    Format,
    /// The decoder rejected the file.
    Corrupt,
    /// Larger than [`MAX_SOURCE_PIXELS`].
    TooLarge,
}

/// Largest picture accepted for import (12 megapixels, ~84 MB while
/// decoding), so it fits the smallest Tier B heap (128 MB machines).
pub const MAX_SOURCE_PIXELS: usize = 12_000_000;

impl Bitmap {
    pub fn new(w: i32, h: i32, fill: u32) -> Bitmap {
        let w = w.max(1);
        let h = h.max(1);
        Bitmap {
            w,
            h,
            px: vec![fill; (w * h) as usize],
        }
    }

    #[inline]
    fn at(&self, x: i32, y: i32) -> u32 {
        self.px[(y.clamp(0, self.h - 1) * self.w + x.clamp(0, self.w - 1)) as usize]
    }

    /// Bilinear sample at fixed-point (16.16) coordinates.
    #[inline]
    fn sample(&self, fx: i64, fy: i64) -> u32 {
        let x0 = (fx >> 16) as i32;
        let y0 = (fy >> 16) as i32;
        let ax = ((fx >> 8) & 0xFF) as u32;
        let ay = ((fy >> 8) & 0xFF) as u32;
        let p00 = self.at(x0, y0);
        let p10 = self.at(x0 + 1, y0);
        let p01 = self.at(x0, y0 + 1);
        let p11 = self.at(x0 + 1, y0 + 1);
        let mut out = 0u32;
        for sh in [0u32, 8, 16, 24] {
            let c = |p: u32| (p >> sh) & 0xFF;
            let top = c(p00) * (256 - ax) + c(p10) * ax;
            let bot = c(p01) * (256 - ax) + c(p11) * ax;
            let v = (top * (256 - ay) + bot * ay + (1 << 15)) >> 16;
            out |= v.min(255) << sh;
        }
        out
    }

    /// Area-averaged downscale to exactly `w`×`h` (upscales bilinearly).
    pub fn resized(&self, w: i32, h: i32) -> Bitmap {
        let w = w.max(1);
        let h = h.max(1);
        if w >= self.w || h >= self.h {
            return self.bilinear(Rect::new(0, 0, self.w, self.h), w, h);
        }
        let mut out = Bitmap::new(w, h, 0);
        for oy in 0..h {
            let y0 = oy * self.h / h;
            let y1 = ((oy + 1) * self.h / h).max(y0 + 1);
            for ox in 0..w {
                let x0 = ox * self.w / w;
                let x1 = ((ox + 1) * self.w / w).max(x0 + 1);
                let mut acc = [0u64; 4];
                for y in y0..y1 {
                    let row = (y * self.w) as usize;
                    for &p in &self.px[row + x0 as usize..row + x1 as usize] {
                        let a = u64::from(p >> 24);
                        // Premultiply so transparent pixels don't bleed colour.
                        acc[0] += u64::from((p >> 16) & 0xFF) * a;
                        acc[1] += u64::from((p >> 8) & 0xFF) * a;
                        acc[2] += u64::from(p & 0xFF) * a;
                        acc[3] += a;
                    }
                }
                let n = ((y1 - y0) * (x1 - x0)) as u64;
                let px = if acc[3] == 0 {
                    0
                } else {
                    let c = |v: u64| ((v + acc[3] / 2) / acc[3]).min(255) as u32;
                    let a = ((acc[3] + n / 2) / n).min(255) as u32;
                    (a << 24) | (c(acc[0]) << 16) | (c(acc[1]) << 8) | c(acc[2])
                };
                out.px[(oy * w + ox) as usize] = px;
            }
        }
        out
    }

    fn bilinear(&self, src: Rect, w: i32, h: i32) -> Bitmap {
        let mut out = Bitmap::new(w, h, 0);
        let sx = (i64::from(src.w) << 16) / i64::from(w);
        let sy = (i64::from(src.h) << 16) / i64::from(h);
        for oy in 0..h {
            let fy = (i64::from(src.y) << 16) + i64::from(oy) * sy + sy / 2 - (1 << 15);
            for ox in 0..w {
                let fx = (i64::from(src.x) << 16) + i64::from(ox) * sx + sx / 2 - (1 << 15);
                out.px[(oy * w + ox) as usize] = self.sample(fx.max(0), fy.max(0));
            }
        }
        out
    }

    /// Largest size with this aspect ratio inside `max_w`×`max_h`, never
    /// larger than the original.
    pub fn fit_size(&self, max_w: i32, max_h: i32) -> (i32, i32) {
        if self.w <= max_w && self.h <= max_h {
            return (self.w, self.h);
        }
        let by_w = (max_w, (self.h * max_w / self.w).max(1));
        if by_w.1 <= max_h {
            by_w
        } else {
            ((self.w * max_h / self.h).max(1), max_h)
        }
    }

    /// Scale and centre-crop to fill exactly `w`×`h`.
    pub fn cover(&self, w: i32, h: i32) -> Bitmap {
        // Crop the source to the target aspect ratio, then scale.
        let (cw, ch) = if i64::from(self.w) * i64::from(h) > i64::from(self.h) * i64::from(w) {
            ((self.h * w / h).max(1), self.h)
        } else {
            (self.w, (self.w * h / w).max(1))
        };
        let crop = self.crop(Rect::new((self.w - cw) / 2, (self.h - ch) / 2, cw, ch));
        crop.resized(w, h)
    }

    pub fn crop(&self, r: Rect) -> Bitmap {
        let mut out = Bitmap::new(r.w, r.h, 0);
        for y in 0..out.h {
            for x in 0..out.w {
                out.px[(y * out.w + x) as usize] = self.at(r.x + x, r.y + y);
            }
        }
        out
    }

    /// Three box-blur passes (≈ Gaussian) of the given radius.
    pub fn blur(&mut self, radius: i32) {
        if radius <= 0 {
            return;
        }
        for _ in 0..3 {
            self.box_pass(radius, true);
            self.box_pass(radius, false);
        }
    }

    fn box_pass(&mut self, r: i32, horizontal: bool) {
        let (len, lines) = if horizontal {
            (self.w, self.h)
        } else {
            (self.h, self.w)
        };
        let idx = |line: i32, i: i32| -> usize {
            let i = i.clamp(0, len - 1);
            if horizontal {
                (line * self.w + i) as usize
            } else {
                (i * self.w + line) as usize
            }
        };
        let n = (2 * r + 1) as u32;
        let mut buf = vec![0u32; len as usize];
        for line in 0..lines {
            let mut acc = [0u32; 4];
            for i in -r..=r {
                let p = self.px[idx(line, i)];
                for (k, a) in acc.iter_mut().enumerate() {
                    *a += (p >> (8 * k)) & 0xFF;
                }
            }
            for i in 0..len {
                let mut v = 0u32;
                for (k, a) in acc.iter().enumerate() {
                    v |= ((a + n / 2) / n).min(255) << (8 * k);
                }
                buf[i as usize] = v;
                let add = self.px[idx(line, i + r + 1)];
                let sub = self.px[idx(line, i - r)];
                for (k, a) in acc.iter_mut().enumerate() {
                    *a = *a + ((add >> (8 * k)) & 0xFF) - ((sub >> (8 * k)) & 0xFF);
                }
            }
            for i in 0..len {
                let j = idx(line, i);
                self.px[j] = buf[i as usize];
            }
        }
    }

    /// Mean colour (opaque pixels), for tinting placeholders and panels.
    pub fn average(&self) -> Rgb888 {
        let mut acc = [0u64; 3];
        let mut n = 0u64;
        for &p in &self.px {
            if p >> 24 >= 128 {
                acc[0] += u64::from((p >> 16) & 0xFF);
                acc[1] += u64::from((p >> 8) & 0xFF);
                acc[2] += u64::from(p & 0xFF);
                n += 1;
            }
        }
        if n == 0 {
            return 0x0080_8080;
        }
        ((acc[0] / n) << 16 | (acc[1] / n) << 8 | (acc[2] / n)) as u32
    }
}

// ---------------------------------------------------------------- decoding

/// Decode a PNG or JPEG file.
pub fn decode(bytes: &[u8]) -> Result<Bitmap, ImageError> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        decode_png(bytes)
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        decode_jpeg(bytes)
    } else if bytes.starts_with(b"qoif") {
        unpack(bytes).ok_or(ImageError::Corrupt)
    } else {
        Err(ImageError::Format)
    }
}

fn decoder_options() -> zune_core::options::DecoderOptions {
    zune_core::options::DecoderOptions::default()
        .set_max_width(16384)
        .set_max_height(16384)
}

fn decode_png(bytes: &[u8]) -> Result<Bitmap, ImageError> {
    use zune_core::bytestream::ZCursor;
    use zune_core::colorspace::ColorSpace;
    let mut d = zune_png::PngDecoder::new_with_options(ZCursor::new(bytes), decoder_options());
    d.decode_headers().map_err(|_| ImageError::Corrupt)?;
    let (w, h) = d.dimensions().ok_or(ImageError::Corrupt)?;
    if w.saturating_mul(h) > MAX_SOURCE_PIXELS {
        return Err(ImageError::TooLarge);
    }
    let cs = d.colorspace().ok_or(ImageError::Corrupt)?;
    // 16-bit PNGs come back as u16; take the high byte.
    let raw = d.decode().map_err(|_| ImageError::Corrupt)?;
    let px8: Vec<u8> = match raw {
        zune_core::result::DecodingResult::U8(v) => v,
        zune_core::result::DecodingResult::U16(v) => v.iter().map(|x| (x >> 8) as u8).collect(),
        _ => return Err(ImageError::Corrupt),
    };
    let ch = cs.num_components();
    let mut out = Bitmap::new(w as i32, h as i32, 0);
    if px8.len() < w * h * ch {
        return Err(ImageError::Corrupt);
    }
    for (i, p) in out.px.iter_mut().enumerate() {
        let s = &px8[i * ch..i * ch + ch];
        let (r, g, b, a) = match cs {
            ColorSpace::Luma => (s[0], s[0], s[0], 255),
            ColorSpace::LumaA => (s[0], s[0], s[0], s[1]),
            ColorSpace::RGB => (s[0], s[1], s[2], 255),
            ColorSpace::RGBA => (s[0], s[1], s[2], s[3]),
            _ => return Err(ImageError::Format),
        };
        *p = u32::from(a) << 24 | u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b);
    }
    Ok(out)
}

fn decode_jpeg(bytes: &[u8]) -> Result<Bitmap, ImageError> {
    use zune_core::bytestream::ZCursor;
    use zune_core::colorspace::ColorSpace;
    let opts = decoder_options().jpeg_set_out_colorspace(ColorSpace::RGB);
    let mut d = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(bytes), opts);
    d.decode_headers().map_err(|_| ImageError::Corrupt)?;
    let (w, h) = d.dimensions().ok_or(ImageError::Corrupt)?;
    if w.saturating_mul(h) > MAX_SOURCE_PIXELS {
        return Err(ImageError::TooLarge);
    }
    let px8 = d.decode().map_err(|_| ImageError::Corrupt)?;
    if px8.len() < w * h * 3 {
        return Err(ImageError::Corrupt);
    }
    let mut out = Bitmap::new(w as i32, h as i32, 0);
    for (p, s) in out.px.iter_mut().zip(px8.as_chunks::<3>().0) {
        *p = 0xFF00_0000 | u32::from(s[0]) << 16 | u32::from(s[1]) << 8 | u32::from(s[2]);
    }
    Ok(out)
}

// ---------------------------------------------------------------- QOI

/// Encode as QOI (lossless, ~2-4× smaller than raw for photos and logos).
pub fn pack(b: &Bitmap) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.px.len() * 2 + 22);
    out.extend_from_slice(b"qoif");
    out.extend_from_slice(&(b.w as u32).to_be_bytes());
    out.extend_from_slice(&(b.h as u32).to_be_bytes());
    out.extend_from_slice(&[4, 0]);
    let mut index = [0u32; 64];
    let mut prev = 0xFF00_0000u32;
    let mut run = 0u8;
    let n = b.px.len();
    for (i, &p) in b.px.iter().enumerate() {
        if p == prev {
            run += 1;
            if run == 62 || i + 1 == n {
                out.push(0xC0 | (run - 1));
                run = 0;
            }
            continue;
        }
        if run > 0 {
            out.push(0xC0 | (run - 1));
            run = 0;
        }
        let h = qoi_hash(p);
        if index[h] == p {
            out.push(h as u8);
        } else {
            index[h] = p;
            let [a, r, g, bl] = p.to_be_bytes();
            let [pa, pr, pg, pb] = prev.to_be_bytes();
            if a == pa {
                let dr = r.wrapping_sub(pr) as i8;
                let dg = g.wrapping_sub(pg) as i8;
                let db = bl.wrapping_sub(pb) as i8;
                let dr_dg = dr.wrapping_sub(dg);
                let db_dg = db.wrapping_sub(dg);
                if (-2..=1).contains(&dr) && (-2..=1).contains(&dg) && (-2..=1).contains(&db) {
                    out.push(0x40 | ((dr + 2) as u8) << 4 | ((dg + 2) as u8) << 2 | (db + 2) as u8);
                } else if (-32..=31).contains(&dg)
                    && (-8..=7).contains(&dr_dg)
                    && (-8..=7).contains(&db_dg)
                {
                    out.push(0x80 | (dg + 32) as u8);
                    out.push(((dr_dg + 8) as u8) << 4 | (db_dg + 8) as u8);
                } else {
                    out.extend_from_slice(&[0xFE, r, g, bl]);
                }
            } else {
                out.extend_from_slice(&[0xFF, r, g, bl, a]);
            }
        }
        prev = p;
    }
    out.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 1]);
    out
}

fn qoi_hash(p: u32) -> usize {
    let [a, r, g, b] = p.to_be_bytes();
    (usize::from(r) * 3 + usize::from(g) * 5 + usize::from(b) * 7 + usize::from(a) * 11) % 64
}

/// Decode [`pack`] output. `None` if malformed or over 4096×4096.
pub fn unpack(d: &[u8]) -> Option<Bitmap> {
    if d.len() < 22 || &d[..4] != b"qoif" {
        return None;
    }
    let w = u32::from_be_bytes(d[4..8].try_into().ok()?);
    let h = u32::from_be_bytes(d[8..12].try_into().ok()?);
    if w == 0 || h == 0 || w > 4096 || h > 4096 {
        return None;
    }
    let n = (w * h) as usize;
    let mut px = Vec::with_capacity(n);
    let mut index = [0u32; 64];
    let mut prev = 0xFF00_0000u32;
    let mut i = 14;
    let end = d.len() - 8;
    while px.len() < n {
        let b0 = *d.get(i).filter(|_| i < end)?;
        i += 1;
        let p = match b0 {
            0xFE => {
                let s = d.get(i..i + 3)?;
                i += 3;
                prev & 0xFF00_0000 | u32::from(s[0]) << 16 | u32::from(s[1]) << 8 | u32::from(s[2])
            }
            0xFF => {
                let s = d.get(i..i + 4)?;
                i += 4;
                u32::from_be_bytes([s[3], s[0], s[1], s[2]])
            }
            _ => match b0 >> 6 {
                0 => index[usize::from(b0 & 0x3F)],
                1 => {
                    let [a, r, g, b] = prev.to_be_bytes();
                    let r = r.wrapping_add((b0 >> 4) & 3).wrapping_sub(2);
                    let g = g.wrapping_add((b0 >> 2) & 3).wrapping_sub(2);
                    let b = b.wrapping_add(b0 & 3).wrapping_sub(2);
                    u32::from_be_bytes([a, r, g, b])
                }
                2 => {
                    let b1 = *d.get(i)?;
                    i += 1;
                    let dg = (b0 & 0x3F).wrapping_sub(32);
                    let [a, r, g, b] = prev.to_be_bytes();
                    let r = r.wrapping_add(dg).wrapping_add(b1 >> 4).wrapping_sub(8);
                    let g = g.wrapping_add(dg);
                    let b = b.wrapping_add(dg).wrapping_add(b1 & 0xF).wrapping_sub(8);
                    u32::from_be_bytes([a, r, g, b])
                }
                _ => {
                    let run = usize::from(b0 & 0x3F) + 1;
                    for _ in 0..run.min(n - px.len()) {
                        px.push(prev);
                    }
                    continue;
                }
            },
        };
        index[qoi_hash(p)] = p;
        prev = p;
        px.push(p);
    }
    Some(Bitmap {
        w: w as i32,
        h: h as i32,
        px,
    })
}

// ---------------------------------------------------------------- drawing

/// Coverage (0-255) of pixel (x, y) inside a rounded rectangle of size
/// `w`×`h` with corner radius `rad`, using 4×4 supersampling at corners.
fn corner_cov(x: i32, y: i32, w: i32, h: i32, rad: i32) -> u32 {
    if rad <= 0 {
        return 255;
    }
    let cx = if x < rad {
        rad
    } else if x >= w - rad {
        w - rad
    } else {
        return 255;
    };
    let cy = if y < rad {
        rad
    } else if y >= h - rad {
        h - rad
    } else {
        return 255;
    };
    let r2 = i64::from(rad) * i64::from(rad) * 64;
    let mut hits = 0u32;
    for sy in 0..4 {
        for sx in 0..4 {
            // Sub-sample centres in eighths of a pixel.
            let dx = i64::from(x * 8 + 1 + 2 * sx - cx * 8);
            let dy = i64::from(y * 8 + 1 + 2 * sy - cy * 8);
            if dx * dx + dy * dy <= r2 {
                hits += 1;
            }
        }
    }
    hits * 255 / 16
}

impl Canvas {
    /// Draw `b` scaled to fill `r` (centre-cropped), clipped to a rounded
    /// rectangle of radius `rad` (`rad = r.w / 2` on a square gives a
    /// circle), blended by the bitmap's alpha times `alpha`.
    pub fn draw_bitmap(&mut self, r: Rect, b: &Bitmap, rad: i32, alpha: u8) {
        if r.w <= 0 || r.h <= 0 {
            return;
        }
        // Source window with the target's aspect ratio.
        let (cw, ch) = if i64::from(b.w) * i64::from(r.h) > i64::from(b.h) * i64::from(r.w) {
            ((b.h * r.w / r.h).max(1), b.h)
        } else {
            (b.w, (b.w * r.h / r.w).max(1))
        };
        let (cx, cy) = ((b.w - cw) / 2, (b.h - ch) / 2);
        let downscale = cw > r.w * 2 || ch > r.h * 2;
        let scaled;
        let (src, sx0, sy0, sw, sh) = if downscale {
            // Large reduction: area-average first so it doesn't alias.
            scaled = b.crop(Rect::new(cx, cy, cw, ch)).resized(r.w, r.h);
            (&scaled, 0, 0, r.w, r.h)
        } else {
            (b, cx, cy, cw, ch)
        };
        let stx = (i64::from(sw) << 16) / i64::from(r.w);
        let sty = (i64::from(sh) << 16) / i64::from(r.h);
        let rad = rad.min(r.w / 2).min(r.h / 2);
        let bounds = self.bounds();
        for y in 0..r.h {
            let py = r.y + y;
            if py < 0 || py >= bounds.h {
                continue;
            }
            let fy = (i64::from(sy0) << 16) + i64::from(y) * sty + sty / 2 - (1 << 15);
            for x in 0..r.w {
                let px = r.x + x;
                if px < 0 || px >= bounds.w {
                    continue;
                }
                let cov = corner_cov(x, y, r.w, r.h, rad);
                if cov == 0 {
                    continue;
                }
                let fx = (i64::from(sx0) << 16) + i64::from(x) * stx + stx / 2 - (1 << 15);
                let s = src.sample(fx.max(0), fy.max(0));
                let a = (s >> 24) * cov / 255 * u32::from(alpha) / 255;
                self.blend(px, py, s & 0x00FF_FFFF, a as u8);
            }
        }
        self.mark(r);
    }

    /// Copy a bitmap of exactly the canvas size (pre-scaled wallpaper).
    pub fn blit_full(&mut self, b: &Bitmap) {
        if b.w != self.w || b.h != self.h {
            let r = self.bounds();
            self.draw_bitmap(r, b, 0, 255);
            return;
        }
        for (d, s) in self.px.iter_mut().zip(&b.px) {
            *d = s & 0x00FF_FFFF;
        }
        let r = self.bounds();
        self.mark(r);
    }

    /// Rounded rectangle blended over what is there ("frosted" panels).
    pub fn fill_round_rect_alpha(&mut self, r: Rect, rad: i32, c: Rgb888, alpha: u8) {
        let rad = rad.min(r.w / 2).min(r.h / 2).max(0);
        let b = self.bounds();
        for y in r.y.max(0)..(r.y + r.h).min(b.h) {
            for x in r.x.max(0)..(r.x + r.w).min(b.w) {
                let cov = corner_cov(x - r.x, y - r.y, r.w, r.h, rad);
                let a = cov * u32::from(alpha) / 255;
                self.blend(x, y, c, a as u8);
            }
        }
        self.mark(r);
    }

    /// Anti-aliased filled circle.
    pub fn fill_circle(&mut self, cx: i32, cy: i32, radius: i32, c: Rgb888) {
        let r = Rect::new(cx - radius, cy - radius, 2 * radius, 2 * radius);
        self.fill_round_rect_alpha(r, radius, c, 255);
    }

    /// Vertical gradient from `top` to `bottom`, clipped to rounded corners.
    pub fn fill_gradient(&mut self, r: Rect, rad: i32, top: Rgb888, bottom: Rgb888) {
        let rad = rad.min(r.w / 2).min(r.h / 2).max(0);
        let b = self.bounds();
        for y in r.y.max(0)..(r.y + r.h).min(b.h) {
            let t = ((y - r.y) * 255 / r.h.max(1)) as u8;
            let c = mix(top, bottom, t);
            for x in r.x.max(0)..(r.x + r.w).min(b.w) {
                let cov = corner_cov(x - r.x, y - r.y, r.w, r.h, rad);
                self.blend(x, y, c, cov as u8);
            }
        }
        self.mark(r);
    }
}

/// The built-in wallpaper: a dark blue-violet field with soft light blobs,
/// rendered at `w`×`h` (deterministic, no assets).
pub fn default_wallpaper(w: i32, h: i32) -> Bitmap {
    // Paint small, blur, then scale up: smooth and cheap.
    let (sw, sh) = (160, 90);
    let mut b = Bitmap::new(sw, sh, 0xFF0B_1020);
    let blobs: [(i32, i32, i32, u32); 5] = [
        (30, 20, 40, 0x001F_4ED8),
        (120, 15, 34, 0x007C_3AED),
        (95, 75, 46, 0x000E_9FA6),
        (10, 85, 30, 0x0025_63EB),
        (150, 60, 28, 0x00C0_26D3),
    ];
    for y in 0..sh {
        // Base: vertical gradient.
        let base = mix(0x000B_1530, 0x0016_0B2E, (y * 255 / sh) as u8);
        for x in 0..sw {
            let mut c = base;
            for &(bx, by, br, col) in &blobs {
                let d2 = (x - bx) * (x - bx) + (y - by) * (y - by);
                let r2 = br * br;
                if d2 < r2 {
                    let a = (r2 - d2) * 150 / r2;
                    c = mix(c, col, a as u8);
                }
            }
            b.px[(y * sw + x) as usize] = 0xFF00_0000 | c;
        }
    }
    b.blur(4);
    b.resized(w, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Bitmap {
        let mut b = Bitmap::new(37, 23, 0);
        for y in 0..23 {
            for x in 0..37 {
                let a = if x < 3 { (x * 80) as u32 } else { 255 };
                b.px[(y * 37 + x) as usize] =
                    a << 24 | ((x * 7) as u32) << 16 | ((y * 11) as u32) << 8 | 0x40;
            }
        }
        b.px[100] = 0xFF12_3456;
        b
    }

    #[test]
    fn qoi_round_trip() {
        let b = sample();
        let p = pack(&b);
        assert_eq!(unpack(&p).as_ref(), Some(&b));
        let flat = Bitmap::new(300, 200, 0xFFFF_FFFF);
        let p = pack(&flat);
        assert!(p.len() < 2000);
        assert_eq!(unpack(&p), Some(flat));
        // Truncated data never panics.
        for cut in 0..p.len() {
            let _ = unpack(&p[..cut]);
        }
    }

    #[test]
    fn resize_and_cover() {
        let b = sample();
        let r = b.resized(10, 6);
        assert_eq!((r.w, r.h), (10, 6));
        let c = b.cover(16, 16);
        assert_eq!((c.w, c.h), (16, 16));
        assert_eq!(b.fit_size(100, 100), (37, 23));
        assert_eq!(b.fit_size(18, 18), (18, 11));
        let flat = Bitmap::new(64, 64, 0xFF20_4060);
        assert!(flat.resized(7, 5).px.iter().all(|&p| p == 0xFF20_4060));
    }

    #[test]
    fn blur_keeps_flat_colour() {
        let mut b = Bitmap::new(20, 20, 0xFF80_4020);
        b.blur(3);
        assert!(b.px.iter().all(|&p| p == 0xFF80_4020));
    }

    #[test]
    fn decode_rejects_junk() {
        assert_eq!(decode(b"hello").err(), Some(ImageError::Format));
        assert!(decode(b"\x89PNG\r\n\x1a\nxx").is_err());
        assert!(decode(&[0xFF, 0xD8, 0xFF, 0, 1]).is_err());
    }

    #[test]
    fn draw_into_canvas() {
        let mut c = Canvas::new(64, 48);
        let b = sample();
        c.draw_bitmap(Rect::new(4, 4, 30, 30), &b, 15, 255);
        // Corner of the circle stays background, centre is painted.
        assert_eq!(c.pixel(4, 4), Some(0));
        assert_ne!(c.pixel(19, 19), Some(0));
        let wall = default_wallpaper(64, 48);
        c.blit_full(&wall);
        assert_eq!(c.pixel(0, 0), Some(wall.px[0] & 0xFFFFFF));
    }
}
