//! Borrowed view over an RGBA8 pixel buffer.

use crate::color::Rgba;

/// Read-only view over a 2D RGBA8 image.
///
/// `stride` is the byte distance between successive rows. For a tightly
/// packed buffer it equals `width * 4`.
#[derive(Copy, Clone, Debug)]
pub struct FrameView<'a> {
    pub pixels: &'a [u8],
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

impl<'a> FrameView<'a> {
    /// Construct a view over a tightly-packed RGBA8 buffer of the given
    /// dimensions. Returns `None` if `pixels.len()` is shorter than
    /// `width * height * 4`.
    pub fn packed(pixels: &'a [u8], width: u32, height: u32) -> Option<Self> {
        let needed = (width as usize).checked_mul(height as usize)?.checked_mul(4)?;
        if pixels.len() < needed {
            return None;
        }
        Some(Self {
            pixels,
            width,
            height,
            stride: width * 4,
        })
    }

    /// Read a single pixel. Returns `None` if `(x, y)` is outside the frame
    /// or the slice is too short for the row.
    pub fn pixel(&self, x: u32, y: u32) -> Option<Rgba> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let i = (y as usize) * (self.stride as usize) + (x as usize) * 4;
        let p = self.pixels.get(i..i + 4)?;
        Some(Rgba::new(p[0], p[1], p[2], p[3]))
    }
}
