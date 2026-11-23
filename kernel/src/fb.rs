//! `pos_hal::Display` over the linear framebuffer Limine sets up (from GOP).

use crate::limine::Framebuffer;
use pos_hal::{HalError, HalResult, PixelFormat, Rgb888};

#[derive(Debug)]
pub struct FbDisplay {
    base: *mut u32,
    width: u32,
    height: u32,
    stride: usize, // in pixels
    format: PixelFormat,
}

impl FbDisplay {
    /// Accept only 32-bpp RGB framebuffers with 8-bit channels at the two
    /// byte orders PC firmware uses.
    pub fn new(fb: &'static Framebuffer) -> HalResult<FbDisplay> {
        if fb.bpp != 32
            || fb.memory_model != 1
            || !fb.pitch.is_multiple_of(4)
            || fb.address.is_null()
        {
            return Err(HalError::NotPresent);
        }
        let masks = (fb.red_mask_shift, fb.green_mask_shift, fb.blue_mask_shift);
        let sizes = (fb.red_mask_size, fb.green_mask_size, fb.blue_mask_size);
        if sizes != (8, 8, 8) {
            return Err(HalError::NotPresent);
        }
        let format = match masks {
            (16, 8, 0) => PixelFormat::Bgrx,
            (0, 8, 16) => PixelFormat::Rgbx,
            _ => return Err(HalError::NotPresent),
        };
        Ok(FbDisplay {
            base: fb.address.cast(),
            width: u32::try_from(fb.width).map_err(|_| HalError::NotPresent)?,
            height: u32::try_from(fb.height).map_err(|_| HalError::NotPresent)?,
            stride: (fb.pitch / 4) as usize,
            format,
        })
    }
}

impl pos_hal::Display for FbDisplay {
    fn width(&self) -> u32 {
        self.width
    }
    fn height(&self) -> u32 {
        self.height
    }
    fn pixel_format(&self) -> PixelFormat {
        self.format
    }
    fn blit(
        &mut self,
        src: &[Rgb888],
        src_stride: usize,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> HalResult<()> {
        let (xe, ye) = (x.checked_add(w), y.checked_add(h));
        if xe.is_none_or(|v| v > self.width) || ye.is_none_or(|v| v > self.height) {
            return Err(HalError::InvalidArgument);
        }
        let (w, h) = (w as usize, h as usize);
        if h > 0 && (src_stride < w || src.len() < (h - 1) * src_stride + w) {
            return Err(HalError::InvalidArgument);
        }
        for row in 0..h {
            let s = &src[row * src_stride..row * src_stride + w];
            let off = (y as usize + row) * self.stride + x as usize;
            // SAFETY: bounds checked above against the mode's width/height;
            // `stride >= width`, so the row lies inside the framebuffer.
            let dst = unsafe { self.base.add(off) };
            match self.format {
                // 0x00RRGGBB little-endian is bytes B,G,R,X: a straight copy.
                PixelFormat::Bgrx => {
                    // SAFETY: as above; source and framebuffer never overlap.
                    unsafe { core::ptr::copy_nonoverlapping(s.as_ptr(), dst, w) }
                }
                PixelFormat::Rgbx => {
                    for (i, &p) in s.iter().enumerate() {
                        let q = ((p & 0xff) << 16) | (p & 0xff00) | ((p >> 16) & 0xff);
                        // SAFETY: i < w, inside the row checked above.
                        unsafe { dst.add(i).write(q) }
                    }
                }
            }
        }
        Ok(())
    }
}
