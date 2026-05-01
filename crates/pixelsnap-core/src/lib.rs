//! Algorithms, geometry, and color math for vernier.
//!
//! No GUI or platform dependencies live in this crate.

pub mod color;
pub mod edge;
pub mod frame;
pub mod geometry;

pub use color::Rgba;
pub use edge::{detect_edges, Direction, EdgeCandidate, EdgeQuad, Tolerance};
pub use frame::FrameView;
pub use geometry::{Px, PxRect};
