//! `pos_hal::Display` over the UEFI Graphics Output Protocol.
//!
//! We only accept modes with a linear RGBx or BGRx framebuffer and write to it
//! directly (row-wise volatile copies). `Blt`-only and bitmask modes are
//! rejected at mode selection, which every PC GOP driver we target satisfies.

use pos_hal::{HalError, HalResult, PixelFormat, Rgb888};
use uefi::boot::{self, ScopedProtocol};
use uefi::proto::console::gop::{self, GraphicsOutput};

/// Largest mode we will select (spec: highest mode ≤ 1920×1080).
pub const MAX_W: usize = 1920;
pub const MAX_H: usize = 1080;

#[derive(Debug)]
pub struct GopDisplay {
    gop: ScopedProtocol<GraphicsOutput>,
    width: u32,
    height: u32,
    stride: usize,
    format: PixelFormat,
    fb_size: usize,
}

impl GopDisplay {
    /// Locate GOP and switch to the largest supported mode within
    /// `MAX_W`×`MAX_H` (largest by pixel count, ties broken by width).
    pub fn init() -> HalResult<Self> {
        let handle =
            boot::get_handle_for_protocol::<GraphicsOutput>().map_err(|_| HalError::NotPresent)?;
        let mut gop = boot::open_protocol_exclusive::<GraphicsOutput>(handle)
            .map_err(|_| HalError::NotPresent)?;

        let mut best: Option<(gop::Mode, usize)> = None;
        for mode in gop.modes() {
            let info = mode.info();
            let (w, h) = info.resolution();
            let ok_fmt = matches!(
                info.pixel_format(),
                gop::PixelFormat::Rgb | gop::PixelFormat::Bgr
            );
            if !ok_fmt || w > MAX_W || h > MAX_H {
                continue;
            }
            let score = w * h;
            if best.as_ref().is_none_or(|(_, s)| score > *s) {
                best = Some((mode, score));
            }
        }
        let (mode, _) = best.ok_or(HalError::NotPresent)?;
        gop.set_mode(&mode).map_err(|_| HalError::Io)?;

        let info = gop.current_mode_info();
        let (w, h) = info.resolution();
        let format = match info.pixel_format() {
            gop::PixelFormat::Rgb => PixelFormat::Rgbx,
            _ => PixelFormat::Bgrx,
        };
        let stride = info.stride();
        let fb_size = gop.frame_buffer().size();
        Ok(Self {
            gop,
            width: u32::try_from(w).map_err(|_| HalError::InvalidArgument)?,
            height: u32::try_from(h).map_err(|_| HalError::InvalidArgument)?,
            stride,
            format,
            fb_size,
        })
    }
}

impl pos_hal::Display for GopDisplay {
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
        let (x, y, w, h) = (x as usize, y as usize, w as usize, h as usize);
        if x + w > self.width as usize || y + h > self.height as usize {
            return Err(HalError::InvalidArgument);
        }
        if h > 0 && (src_stride < w || src.len() < (h - 1) * src_stride + w) {
            return Err(HalError::InvalidArgument);
        }
        let mut fb = self.gop.frame_buffer();
        let base = fb.as_mut_ptr().cast::<u32>();
        for row in 0..h {
            let dst_off = (y + row) * self.stride + x;
            // Bounds were checked against the mode geometry above; this guards
            // against firmware reporting a framebuffer smaller than stride*h.
            if (dst_off + w) * 4 > self.fb_size {
                return Err(HalError::InvalidArgument);
            }
            let line = &src[row * src_stride..row * src_stride + w];
            for (i, &px) in line.iter().enumerate() {
                let v = match self.format {
                    // 0x00RRGGBB little-endian is B,G,R,X in memory.
                    PixelFormat::Bgrx => px,
                    PixelFormat::Rgbx => ((px & 0xFF) << 16) | (px & 0xFF00) | ((px >> 16) & 0xFF),
                };
                // SAFETY: `dst_off + i < dst_off + w`, and `(dst_off + w) * 4
                // <= fb_size` was checked above, so the write is inside the
                // GOP framebuffer, which is valid, 4-byte aligned MMIO/RAM for
                // as long as `fb` (borrowing `self.gop`) is alive. Volatile is
                // required because it may be device memory.
                unsafe { base.add(dst_off + i).write_volatile(v) };
            }
        }
        Ok(())
    }
}
