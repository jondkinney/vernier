//! Geometry primitives shared by the core algorithms.

/// Integer pixel position in screen space.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct Px {
    pub x: i32,
    pub y: i32,
}

impl Px {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

/// Integer pixel rectangle. `width` / `height` are unsigned by convention.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct PxRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl PxRect {
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}
