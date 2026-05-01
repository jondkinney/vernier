//! Algorithms, geometry, and color math for vernier.
//!
//! No GUI or platform dependencies live in this crate.

pub mod aspect;
pub mod color;
pub mod edge;
pub mod frame;
pub mod geometry;
pub mod measurement;

pub use aspect::{classify as classify_aspect, CommonRatio, Mode as AspectMode, Ratio};
pub use color::Rgba;
pub use edge::{detect_edges, Direction, EdgeCandidate, EdgeQuad, Tolerance};
pub use frame::FrameView;
pub use geometry::{Px, PxRect};
pub use measurement::{
    axis_biased_snap, best_snap, Axis, Measurement, Mode as InteractionMode, SnapPoint,
};
