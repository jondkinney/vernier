//! Aspect-ratio detection and snapping for the area tool.
//!
//! When the user drags out a rectangle, we report the aspect ratio in
//! one of four modes:
//!
//! - [`Mode::Automatic`] — detect a "common" ratio if the actual ratio
//!   is within tolerance, otherwise show the reduced fraction.
//! - [`Mode::Standard`] — always show one of the curated common ratios,
//!   choosing the closest by relative error.
//! - [`Mode::Reduced`] — always show the reduced fraction, e.g. 1.77:1.
//! - [`Mode::CommonOnly`] — like `Automatic` but if no match is within
//!   tolerance, return `None` so the UI can hide the readout.

/// Reporting mode for aspect ratios.
#[derive(Copy, Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Mode {
    Automatic,
    Standard,
    Reduced,
    CommonOnly,
}

/// A reported aspect ratio.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Ratio {
    /// One of the curated common ratios, named.
    Common(CommonRatio),
    /// Reduced fraction, e.g. (177, 100) for 1.77:1.
    Reduced { num: u32, den: u32 },
}

/// Curated list of common aspect ratios used by displays, photography,
/// and design. Order is rough display-popularity descending.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CommonRatio {
    /// 16:9 — modern HD displays
    R16x9,
    /// 4:3 — legacy displays
    R4x3,
    /// 1:1 — square
    R1x1,
    /// 21:9 — ultra-wide
    R21x9,
    /// 16:10 — laptop / WUXGA
    R16x10,
    /// 5:4 — SXGA
    R5x4,
    /// 3:2 — DSLR / Microsoft Surface
    R3x2,
    /// 2:1 — mobile-portrait-on-wide-screen
    R2x1,
    /// 9:16 — vertical video
    R9x16,
    /// 3:4 — vertical 4:3
    R3x4,
}

impl CommonRatio {
    pub fn ratio(self) -> (u32, u32) {
        match self {
            CommonRatio::R16x9 => (16, 9),
            CommonRatio::R4x3 => (4, 3),
            CommonRatio::R1x1 => (1, 1),
            CommonRatio::R21x9 => (21, 9),
            CommonRatio::R16x10 => (16, 10),
            CommonRatio::R5x4 => (5, 4),
            CommonRatio::R3x2 => (3, 2),
            CommonRatio::R2x1 => (2, 1),
            CommonRatio::R9x16 => (9, 16),
            CommonRatio::R3x4 => (3, 4),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            CommonRatio::R16x9 => "16:9",
            CommonRatio::R4x3 => "4:3",
            CommonRatio::R1x1 => "1:1",
            CommonRatio::R21x9 => "21:9",
            CommonRatio::R16x10 => "16:10",
            CommonRatio::R5x4 => "5:4",
            CommonRatio::R3x2 => "3:2",
            CommonRatio::R2x1 => "2:1",
            CommonRatio::R9x16 => "9:16",
            CommonRatio::R3x4 => "3:4",
        }
    }

    pub const ALL: &'static [CommonRatio] = &[
        CommonRatio::R16x9,
        CommonRatio::R4x3,
        CommonRatio::R1x1,
        CommonRatio::R21x9,
        CommonRatio::R16x10,
        CommonRatio::R5x4,
        CommonRatio::R3x2,
        CommonRatio::R2x1,
        CommonRatio::R9x16,
        CommonRatio::R3x4,
    ];
}

/// Snap a rectangle's aspect to one of the curated ratios in `mode`.
///
/// `tolerance` is the maximum *relative* error allowed (e.g. 0.02 = 2%)
/// for a match in `Automatic` / `CommonOnly` modes; ignored in
/// `Standard` (always picks the closest) and `Reduced` (no snapping).
pub fn classify(width: u32, height: u32, mode: Mode, tolerance: f32) -> Option<Ratio> {
    if width == 0 || height == 0 {
        return None;
    }
    match mode {
        Mode::Reduced => Some(reduced(width, height)),
        Mode::Standard => closest_common(width, height).map(Ratio::Common),
        Mode::Automatic => match nearest_common(width, height, tolerance) {
            Some(c) => Some(Ratio::Common(c)),
            None => Some(reduced(width, height)),
        },
        Mode::CommonOnly => nearest_common(width, height, tolerance).map(Ratio::Common),
    }
}

fn reduced(width: u32, height: u32) -> Ratio {
    let g = gcd(width, height);
    Ratio::Reduced {
        num: width / g,
        den: height / g,
    }
}

fn closest_common(width: u32, height: u32) -> Option<CommonRatio> {
    let target = width as f32 / height as f32;
    CommonRatio::ALL.iter().copied().min_by(|a, b| {
        let (an, ad) = a.ratio();
        let (bn, bd) = b.ratio();
        let aerr = ((an as f32 / ad as f32) - target).abs();
        let berr = ((bn as f32 / bd as f32) - target).abs();
        aerr.partial_cmp(&berr).unwrap_or(std::cmp::Ordering::Equal)
    })
}

fn nearest_common(width: u32, height: u32, tolerance: f32) -> Option<CommonRatio> {
    let target = width as f32 / height as f32;
    CommonRatio::ALL
        .iter()
        .copied()
        .filter(|c| {
            let (n, d) = c.ratio();
            let r = n as f32 / d as f32;
            (r - target).abs() / target <= tolerance
        })
        .min_by(|a, b| {
            let (an, ad) = a.ratio();
            let (bn, bd) = b.ratio();
            let aerr = ((an as f32 / ad as f32) - target).abs();
            let berr = ((bn as f32 / bd as f32) - target).abs();
            aerr.partial_cmp(&berr).unwrap_or(std::cmp::Ordering::Equal)
        })
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 { a } else { gcd(b, a % b) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reduced_returns_lowest_terms() {
        let r = classify(1920, 1080, Mode::Reduced, 0.0).unwrap();
        assert_eq!(r, Ratio::Reduced { num: 16, den: 9 });
    }

    #[test]
    fn automatic_snaps_to_common_within_tolerance() {
        // 1918x1080 is just barely off 16:9 — should snap.
        let r = classify(1918, 1080, Mode::Automatic, 0.02).unwrap();
        assert_eq!(r, Ratio::Common(CommonRatio::R16x9));
    }

    #[test]
    fn automatic_falls_back_to_reduced_when_far_from_common() {
        // 7:5 is not in the common list and not within 2% of any common ratio.
        let r = classify(7, 5, Mode::Automatic, 0.02).unwrap();
        assert_eq!(r, Ratio::Reduced { num: 7, den: 5 });
    }

    #[test]
    fn common_only_returns_none_when_no_match() {
        let r = classify(7, 5, Mode::CommonOnly, 0.02);
        assert_eq!(r, None);
    }

    #[test]
    fn standard_always_picks_closest_common() {
        // 7:5 = 1.4 — closest common is 4:3 (1.333, diff 0.067) over
        // 3:2 (1.5, diff 0.1) and 16:10 (1.6, diff 0.2).
        let r = classify(7, 5, Mode::Standard, 0.0).unwrap();
        assert_eq!(r, Ratio::Common(CommonRatio::R4x3));
    }

    #[test]
    fn handles_portrait_orientations() {
        let r = classify(1080, 1920, Mode::Automatic, 0.02).unwrap();
        assert_eq!(r, Ratio::Common(CommonRatio::R9x16));
    }

    #[test]
    fn square_is_one_to_one() {
        let r = classify(500, 500, Mode::Automatic, 0.02).unwrap();
        assert_eq!(r, Ratio::Common(CommonRatio::R1x1));
    }

    #[test]
    fn zero_dim_returns_none() {
        assert_eq!(classify(0, 100, Mode::Automatic, 0.02), None);
        assert_eq!(classify(100, 0, Mode::Automatic, 0.02), None);
    }
}
