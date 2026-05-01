//! Pure-Rust color types used by edge detection and the geometry layer.

/// 8-bit-per-channel RGBA. Stored as four bytes in source order.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    pub const TRANSPARENT: Self = Self::new(0, 0, 0, 0);
    pub const BLACK: Self = Self::new(0, 0, 0, 255);
    pub const WHITE: Self = Self::new(255, 255, 255, 255);

    /// Sum of absolute per-channel differences in R/G/B. Range 0..=765.
    /// Alpha is ignored — edges between transparent and opaque pixels still
    /// register based on their RGB.
    pub fn rgb_delta(self, other: Self) -> u32 {
        let dr = (self.r as i32 - other.r as i32).unsigned_abs();
        let dg = (self.g as i32 - other.g as i32).unsigned_abs();
        let db = (self.b as i32 - other.b as i32).unsigned_abs();
        dr + dg + db
    }

    /// Largest single per-channel difference in R/G/B. Range 0..=255.
    pub fn rgb_delta_max(self, other: Self) -> u32 {
        let dr = (self.r as i32 - other.r as i32).unsigned_abs();
        let dg = (self.g as i32 - other.g as i32).unsigned_abs();
        let db = (self.b as i32 - other.b as i32).unsigned_abs();
        dr.max(dg).max(db)
    }
}
