//! Deterministic math for tick code: vectors, boxes and float helpers that give the same bits on
//! every platform and thread count.
//!
//! Tick state holds `f64`; packets carry `f32`, converted at the protocol boundary. Only the four
//! arithmetic operations, `sqrt`, `floor`, `ceil`, `round` and `abs` are used: IEEE 754 fixes
//! their results, while std's `min`, `max`, `clamp` (signed zeros) and transcendental functions
//! (platform libraries) may differ, so they are disallowed in `.clippy.toml`.

use std::f64::consts::{FRAC_2_PI, FRAC_PI_2, PI};
use std::ops::{Add, AddAssign, Mul, Neg, Sub, SubAssign};

/// The high parts of the arc tangents of 0.5, 1, 1.5 and ∞ (fdlibm `atanhi`).
const ATAN_HI: [f64; 4] = [
    f64::from_bits(0x3FDD_AC67_0561_BB4F),
    f64::from_bits(0x3FE9_21FB_5444_2D18),
    f64::from_bits(0x3FEF_730B_D281_F69B),
    f64::from_bits(0x3FF9_21FB_5444_2D18),
];
/// The low parts of the arc tangents of 0.5, 1, 1.5 and ∞ (fdlibm `atanlo`).
const ATAN_LO: [f64; 4] = [
    f64::from_bits(0x3C7A_2B7F_222F_65E2),
    f64::from_bits(0x3C81_A626_3314_5C07),
    f64::from_bits(0x3C70_0788_7AF0_CBBD),
    f64::from_bits(0x3C91_A626_3314_5C07),
];
/// The coefficients of the arc tangent polynomial (fdlibm `aT`).
const ATAN_T: [f64; 11] = [
    f64::from_bits(0x3FD5_5555_5555_550D),
    f64::from_bits(0xBFC9_9999_9998_EBC4),
    f64::from_bits(0x3FC2_4924_9200_83FF),
    f64::from_bits(0xBFBC_71C6_FE23_1671),
    f64::from_bits(0x3FB7_45CD_C54C_206E),
    f64::from_bits(0xBFB3_B0F2_AF74_9A6D),
    f64::from_bits(0x3FB1_0D66_A0D0_3D51),
    f64::from_bits(0xBFAD_DE2D_52DE_FD9A),
    f64::from_bits(0x3FA9_7B4B_2476_0DEB),
    f64::from_bits(0xBFA2_B444_2C6A_6C2F),
    f64::from_bits(0x3F90_AD3A_E322_DA11),
];
/// The first coefficient of the cosine polynomial (fdlibm `C1`).
const COS_1: f64 = f64::from_bits(0x3FA5_5555_5555_554C);
/// The second coefficient of the cosine polynomial (fdlibm `C2`).
const COS_2: f64 = f64::from_bits(0xBF56_C16C_16C1_5177);
/// The third coefficient of the cosine polynomial (fdlibm `C3`).
const COS_3: f64 = f64::from_bits(0x3EFA_01A0_19CB_1590);
/// The fourth coefficient of the cosine polynomial (fdlibm `C4`).
const COS_4: f64 = f64::from_bits(0xBE92_7E4F_809C_52AD);
/// The fifth coefficient of the cosine polynomial (fdlibm `C5`).
const COS_5: f64 = f64::from_bits(0x3E21_EE9E_BDB4_B1C4);
/// The sixth coefficient of the cosine polynomial (fdlibm `C6`).
const COS_6: f64 = f64::from_bits(0xBDA8_FAE9_BE88_38D4);
/// The low part of π (fdlibm `pi_lo`).
const PI_LO: f64 = f64::from_bits(0x3CA1_A626_3314_5C07);
/// The leading part of π/2 (fdlibm `pio2_1`).
const PIO2_1: f64 = f64::from_bits(0x3FF9_21FB_5440_0000);
/// The trailing part of π/2 after the leading part (fdlibm `pio2_1t`).
const PIO2_1T: f64 = f64::from_bits(0x3DD0_B461_1A62_6331);
/// The first coefficient of the sine polynomial (fdlibm `S1`).
const SIN_1: f64 = f64::from_bits(0xBFC5_5555_5555_5549);
/// The second coefficient of the sine polynomial (fdlibm `S2`).
const SIN_2: f64 = f64::from_bits(0x3F81_1111_1110_F8A6);
/// The third coefficient of the sine polynomial (fdlibm `S3`).
const SIN_3: f64 = f64::from_bits(0xBF2A_01A0_19C1_61D5);
/// The fourth coefficient of the sine polynomial (fdlibm `S4`).
const SIN_4: f64 = f64::from_bits(0x3EC7_1DE3_57B1_FE7D);
/// The fifth coefficient of the sine polynomial (fdlibm `S5`).
const SIN_5: f64 = f64::from_bits(0xBE5A_E5E6_8A2B_9CEB);
/// The sixth coefficient of the sine polynomial (fdlibm `S6`).
const SIN_6: f64 = f64::from_bits(0x3DE5_D93A_5ACF_D57C);

/// A position, offset or direction in blocks.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Vec3 {
    /// East is positive.
    pub x: f64,
    /// Up is positive.
    pub y: f64,
    /// South is positive.
    pub z: f64,
}

impl Vec3 {
    /// (0, 0, 0).
    pub const ZERO: Self = Self::new(0.0, 0.0, 0.0);

    /// The vector of a packet's `f32` components.
    pub fn from_f32(v: [f32; 3]) -> Self {
        Self::new(f64::from(v[0]), f64::from(v[1]), f64::from(v[2]))
    }

    /// The vector (x, y, z).
    pub const fn new(x: f64, y: f64, z: f64) -> Self {
        Self { x, y, z }
    }

    /// The squared distance to `other`.
    pub fn distance_squared(self, other: Self) -> f64 {
        (self - other).length_squared()
    }

    /// Whether every component is finite (tick state never holds NaN or infinity).
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.z.is_finite()
    }

    /// The length.
    pub fn length(self) -> f64 {
        self.length_squared().sqrt()
    }

    /// The squared length.
    pub fn length_squared(self) -> f64 {
        self.dot(self)
    }

    /// The dot product.
    pub fn dot(self, other: Self) -> f64 {
        self.x * other.x + self.y * other.y + self.z * other.z
    }

    /// Each component rounded down.
    #[must_use]
    pub fn floor(self) -> Self {
        Self::new(self.x.floor(), self.y.floor(), self.z.floor())
    }

    /// The vector scaled to length 1; the zero vector stays zero.
    #[must_use]
    pub fn normalize(self) -> Self {
        let length = self.length();
        if length > 0.0 {
            Self::new(self.x / length, self.y / length, self.z / length)
        } else {
            Self::ZERO
        }
    }

    /// The `f32` components a packet carries (rounded to nearest).
    pub fn to_f32(self) -> [f32; 3] {
        [to_f32(self.x), to_f32(self.y), to_f32(self.z)]
    }
}

impl Add for Vec3 {
    type Output = Self;

    fn add(self, o: Self) -> Self {
        Self::new(self.x + o.x, self.y + o.y, self.z + o.z)
    }
}

impl AddAssign for Vec3 {
    fn add_assign(&mut self, o: Self) {
        *self = *self + o;
    }
}

impl Sub for Vec3 {
    type Output = Self;

    fn sub(self, o: Self) -> Self {
        Self::new(self.x - o.x, self.y - o.y, self.z - o.z)
    }
}

impl SubAssign for Vec3 {
    fn sub_assign(&mut self, o: Self) {
        *self = *self - o;
    }
}

impl Mul<f64> for Vec3 {
    type Output = Self;

    fn mul(self, k: f64) -> Self {
        Self::new(self.x * k, self.y * k, self.z * k)
    }
}

impl Neg for Vec3 {
    type Output = Self;

    fn neg(self) -> Self {
        Self::new(-self.x, -self.y, -self.z)
    }
}

/// An axis-aligned box from `min` to `max` (inclusive bounds, `min <= max` on each axis).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Aabb {
    /// The lowest corner.
    pub min: Vec3,
    /// The highest corner.
    pub max: Vec3,
}

impl Aabb {
    /// The box between two corners given in any order: on each axis it runs from the smaller
    /// coordinate to the larger. A NaN stays where it is.
    pub const fn new(corner: Vec3, opposite: Vec3) -> Self {
        let (min_x, max_x) = ordered(corner.x, opposite.x);
        let (min_y, max_y) = ordered(corner.y, opposite.y);
        let (min_z, max_z) = ordered(corner.z, opposite.z);
        Self {
            min: Vec3::new(min_x, min_y, min_z),
            max: Vec3::new(max_x, max_y, max_z),
        }
    }

    /// How far `moving` may go along x, up to `dx`, before it hits this box: `dx` itself when
    /// they do not share the y and z ranges or this box is not ahead.
    pub fn clip_x(self, moving: Self, dx: f64) -> f64 {
        if moving.max.y <= self.min.y
            || moving.min.y >= self.max.y
            || moving.max.z <= self.min.z
            || moving.min.z >= self.max.z
        {
            return dx;
        }
        clip(self.min.x, self.max.x, moving.min.x, moving.max.x, dx)
    }

    /// [`Aabb::clip_x`] along y.
    pub fn clip_y(self, moving: Self, dy: f64) -> f64 {
        if moving.max.x <= self.min.x
            || moving.min.x >= self.max.x
            || moving.max.z <= self.min.z
            || moving.min.z >= self.max.z
        {
            return dy;
        }
        clip(self.min.y, self.max.y, moving.min.y, moving.max.y, dy)
    }

    /// [`Aabb::clip_x`] along z.
    pub fn clip_z(self, moving: Self, dz: f64) -> f64 {
        if moving.max.x <= self.min.x
            || moving.min.x >= self.max.x
            || moving.max.y <= self.min.y
            || moving.min.y >= self.max.y
        {
            return dz;
        }
        clip(self.min.z, self.max.z, moving.min.z, moving.max.z, dz)
    }

    /// The box grown by `by` on each side (shrunk for negative components).
    #[must_use]
    pub fn grow(self, by: Vec3) -> Self {
        Self {
            min: self.min - by,
            max: self.max + by,
        }
    }

    /// Whether the two boxes overlap with a positive volume (touching faces do not count).
    pub fn intersects(self, o: Self) -> bool {
        self.overlaps(o, 0.0)
    }

    /// The box moved by `by`.
    #[must_use]
    pub fn offset(self, by: Vec3) -> Self {
        Self {
            min: self.min + by,
            max: self.max + by,
        }
    }

    /// Whether the boxes overlap by more than `epsilon` on every axis.
    pub fn overlaps(self, o: Self, epsilon: f64) -> bool {
        o.max.x - self.min.x > epsilon
            && self.max.x - o.min.x > epsilon
            && o.max.y - self.min.y > epsilon
            && self.max.y - o.min.y > epsilon
            && o.max.z - self.min.z > epsilon
            && self.max.z - o.min.z > epsilon
    }

    /// The box stretched toward `by`: the side each component points to moves by it, so the
    /// result covers every place the box passes moving by `by`.
    #[must_use]
    pub fn stretch(self, by: Vec3) -> Self {
        let (mut min, mut max) = (self.min, self.max);
        for (lo, hi, d) in [
            (&mut min.x, &mut max.x, by.x),
            (&mut min.y, &mut max.y, by.y),
            (&mut min.z, &mut max.z, by.z),
        ] {
            if d < 0.0 {
                *lo += d;
            } else if d > 0.0 {
                *hi += d;
            }
        }
        Self { min, max }
    }
}

/// The two coordinates of one axis as (smaller, larger); a NaN keeps its place.
const fn ordered(a: f64, b: f64) -> (f64, f64) {
    if a > b { (b, a) } else { (a, b) }
}

/// The clipped move `d` of a range `lo..hi` toward an obstacle range `min..max` on one axis.
fn clip(min: f64, max: f64, lo: f64, hi: f64, d: f64) -> f64 {
    if d > 0.0 && hi <= min {
        let room = min - hi;
        if room < d {
            return room;
        }
    } else if d < 0.0 && lo >= max {
        let room = max - lo;
        if room > d {
            return room;
        }
    }
    d
}

/// `v` as the `f32` a packet or a save carries (rounded to nearest).
#[expect(
    clippy::cast_possible_truncation,
    reason = "consistency: packets and saves carry f32, and rounding f64 to f32 is exact IEEE behaviour"
)]
pub(crate) const fn to_f32(v: f64) -> f32 {
    v as f32
}

/// The smaller of `a` and `b`; `b` on a tie (including `-0.0` and `0.0`).
pub fn min(a: f64, b: f64) -> f64 {
    if a < b { a } else { b }
}

/// The larger of `a` and `b`; `b` on a tie (including `-0.0` and `0.0`).
pub fn max(a: f64, b: f64) -> f64 {
    if a > b { a } else { b }
}

/// `v` limited to `lo..=hi`; `v` itself when inside, and a NaN `v` stays NaN. Callers keep
/// `lo <= hi`: with `lo > hi` the result is `lo` for a `v` below `lo` and `hi` for any other
/// (std's `clamp` panics there).
pub fn clamp(v: f64, lo: f64, hi: f64) -> f64 {
    if v < lo {
        lo
    } else if v > hi {
        hi
    } else {
        v
    }
}

/// The sine of `x` radians, within 4 × `f64::EPSILON` of the true value for |x| < 2^20. For a
/// larger |x| or a non-finite `x` no accuracy is promised, and the result may leave [-1, 1].
pub fn sin(x: f64) -> f64 {
    let (quadrant, r) = reduce(x);
    match quadrant & 3 {
        0 => sin_kernel(r),
        1 => cos_kernel(r),
        2 => -sin_kernel(r),
        _ => -cos_kernel(r),
    }
}

/// The cosine of `x` radians, within 4 × `f64::EPSILON` of the true value for |x| < 2^20. For a
/// larger |x| or a non-finite `x` no accuracy is promised, and the result may leave [-1, 1].
pub fn cos(x: f64) -> f64 {
    let (quadrant, r) = reduce(x);
    match quadrant & 3 {
        0 => cos_kernel(r),
        1 => -sin_kernel(r),
        2 => -cos_kernel(r),
        _ => sin_kernel(r),
    }
}

/// `degrees` in radians.
pub fn radians(degrees: f64) -> f64 {
    degrees * (PI / 180.0)
}

/// `radians` in degrees.
pub fn degrees(radians: f64) -> f64 {
    radians * (180.0 / PI)
}

/// The angle of the point (`x`, `y`) from the positive x axis, in radians from -π to π (fdlibm
/// `__ieee754_atan2` on finite input, with only arithmetic, so every platform gives the same
/// bits); 0 for a non-finite input.
pub fn atan2(y: f64, x: f64) -> f64 {
    if !y.is_finite() || !x.is_finite() {
        return 0.0;
    }
    if y == 0.0 {
        return match (x.is_sign_negative(), y.is_sign_negative()) {
            (false, _) => y,
            (true, false) => PI,
            (true, true) => -PI,
        };
    }
    if x == 0.0 {
        return if y < 0.0 { -FRAC_PI_2 } else { FRAC_PI_2 };
    }
    let z = atan((y / x).abs());
    match (x < 0.0, y < 0.0) {
        (false, false) => z,
        (false, true) => -z,
        (true, false) => PI - (z - PI_LO),
        (true, true) => (z - PI_LO) - PI,
    }
}

/// The arc tangent of `x` (fdlibm `atan` coefficients and reduction).
fn atan(x: f64) -> f64 {
    let magnitude = x.abs();
    let negative = x.is_sign_negative();
    if magnitude >= 7.378_697_629_483_821e19 {
        // 2^66: the result is ±π/2 to the precision of a double.
        let half_pi = ATAN_HI[3] + ATAN_LO[3];
        return if negative { -half_pi } else { half_pi };
    }
    let (id, reduced) = if magnitude < 0.4375 {
        if magnitude < 7.450_580_596_923_828e-9 {
            // 2^-27: atan(x) is x.
            return x;
        }
        (None, x)
    } else if magnitude < 1.1875 {
        if magnitude < 0.6875 {
            (Some(0), (2.0 * magnitude - 1.0) / (2.0 + magnitude))
        } else {
            (Some(1), (magnitude - 1.0) / (magnitude + 1.0))
        }
    } else if magnitude < 2.4375 {
        (Some(2), (magnitude - 1.5) / (1.0 + 1.5 * magnitude))
    } else {
        (Some(3), -1.0 / magnitude)
    };
    let square = reduced * reduced;
    let fourth = square * square;
    let s1 = square
        * (ATAN_T[0]
            + fourth
                * (ATAN_T[2]
                    + fourth
                        * (ATAN_T[4]
                            + fourth * (ATAN_T[6] + fourth * (ATAN_T[8] + fourth * ATAN_T[10])))));
    let s2 = fourth
        * (ATAN_T[1]
            + fourth
                * (ATAN_T[3] + fourth * (ATAN_T[5] + fourth * (ATAN_T[7] + fourth * ATAN_T[9]))));
    let Some(id) = id else {
        return reduced - reduced * (s1 + s2);
    };
    let (hi, lo) = match id {
        0 => (ATAN_HI[0], ATAN_LO[0]),
        1 => (ATAN_HI[1], ATAN_LO[1]),
        2 => (ATAN_HI[2], ATAN_LO[2]),
        _ => (ATAN_HI[3], ATAN_LO[3]),
    };
    let angle = hi - ((reduced * (s1 + s2) - lo) - reduced);
    if negative { -angle } else { angle }
}

/// `x` as k·π/2 + reduced with |reduced| ≤ π/4, by Cody–Waite reduction with fdlibm's split of
/// π/2. The fdlibm constants are given by their bits.
#[expect(
    clippy::cast_possible_truncation,
    reason = "invariant: k is magnitude rounded float; out-of-range values saturate, and only k mod 4 is used"
)]
fn reduce(x: f64) -> (i64, f64) {
    let k = (x * FRAC_2_PI).round();
    (k as i64, (x - k * PIO2_1) - k * PIO2_1T)
}

/// sin on [-π/4, π/4] (fdlibm `__kernel_sin` coefficients).
fn sin_kernel(x: f64) -> f64 {
    let square = x * x;
    let reduced = SIN_2 + square * (SIN_3 + square * (SIN_4 + square * (SIN_5 + square * SIN_6)));
    x + square * x * (SIN_1 + square * reduced)
}

/// cos on [-π/4, π/4] (fdlibm `__kernel_cos` coefficients).
fn cos_kernel(x: f64) -> f64 {
    let square = x * x;
    let reduced = square
        * (COS_1
            + square
                * (COS_2
                    + square * (COS_3 + square * (COS_4 + square * (COS_5 + square * COS_6)))));
    let hz = 0.5 * square;
    let fourth = 1.0 - hz;
    fourth + (((1.0 - fourth) - hz) + square * reduced)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the test compares with std"
    )]
    fn reference(x: f64) -> (f64, f64) {
        x.sin_cos()
    }

    #[test]
    fn sine_and_cosine_follow_the_platform_within_four_epsilon() {
        let mut x = -1000.0_f64;
        while x < 1000.0 {
            let (s, c) = reference(x);
            assert!((sin(x) - s).abs() <= 4.0 * f64::EPSILON, "sin {x}");
            assert!((cos(x) - c).abs() <= 4.0 * f64::EPSILON, "cos {x}");
            x += 0.013_7;
        }
        // Up to the end of the documented range, in steps that grow with |x|.
        let mut x = 1000.0_f64;
        while x < 1_048_576.0 {
            for angle in [x, -x] {
                let (s, c) = reference(angle);
                assert!((sin(angle) - s).abs() <= 4.0 * f64::EPSILON, "sin {angle}");
                assert!((cos(angle) - c).abs() <= 4.0 * f64::EPSILON, "cos {angle}");
            }
            x *= 1.013_7;
        }
        assert_eq!(sin(0.0).to_bits(), 0.0_f64.to_bits());
        assert_eq!(cos(0.0), 1.0);
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the test compares with std"
    )]
    fn reference_atan2(y: f64, x: f64) -> f64 {
        y.atan2(x)
    }

    #[test]
    fn atan2_follows_the_platform_within_four_epsilon_in_every_quadrant() {
        let mut a = -10.0_f64;
        while a < 10.0 {
            let mut b = -10.0_f64;
            while b < 10.0 {
                let (ours, std) = (atan2(a, b), reference_atan2(a, b));
                assert!((ours - std).abs() <= 4.0 * f64::EPSILON, "atan2({a}, {b})");
                b += 0.173;
            }
            a += 0.211;
        }
        assert_eq!(atan2(0.0, 1.0), 0.0);
        assert_eq!(atan2(1.0, 0.0), FRAC_PI_2);
        assert_eq!(atan2(0.0, -1.0), PI);
        assert_eq!(atan2(f64::NAN, 1.0), 0.0);
        assert_eq!(degrees(PI), 180.0);
    }

    #[test]
    fn min_max_and_clamp_pick_a_fixed_side_on_ties() {
        assert_eq!(min(0.0, -0.0).to_bits(), (-0.0_f64).to_bits());
        assert_eq!(min(-0.0, 0.0).to_bits(), 0.0_f64.to_bits());
        assert_eq!(max(1.0, 2.0), 2.0);
        assert_eq!(clamp(5.0, 0.0, 1.0), 1.0);
        assert_eq!(clamp(-5.0, 0.0, 1.0), 0.0);
        assert_eq!(clamp(0.5, 0.0, 1.0), 0.5);
    }

    #[test]
    fn a_box_between_two_corners_is_the_same_whichever_corner_comes_first() {
        let low = Vec3::new(-1.0, 2.0, 3.0);
        let high = Vec3::new(4.0, 5.0, 6.0);
        let wanted = Aabb {
            min: low,
            max: high,
        };
        assert_eq!(Aabb::new(low, high), wanted);
        assert_eq!(Aabb::new(high, low), wanted);
        let mixed = Aabb::new(Vec3::new(-1.0, 5.0, 3.0), Vec3::new(4.0, 2.0, 6.0));
        assert_eq!(mixed, wanted);
        let nan = Aabb::new(Vec3::new(f64::NAN, 0.0, 0.0), Vec3::new(1.0, 1.0, 1.0));
        assert!(!nan.min.is_finite());
    }

    #[test]
    fn boxes_that_only_touch_do_not_intersect() {
        let a = Aabb::new(Vec3::ZERO, Vec3::new(1.0, 1.0, 1.0));
        assert!(!a.intersects(a.offset(Vec3::new(1.0, 0.0, 0.0))));
        assert!(a.intersects(a.offset(Vec3::new(0.5, 0.5, 0.5))));
        assert!(!a.overlaps(a.offset(Vec3::new(1.0 - 1e-6, 0.0, 0.0)), 1e-5));
        assert!(a.overlaps(a.offset(Vec3::new(0.5, 0.0, 0.0)), 1e-5));
    }

    #[test]
    fn clipping_stops_a_moving_box_at_an_obstacle() {
        let floor = Aabb::new(Vec3::new(0.0, -1.0, 0.0), Vec3::new(1.0, 0.0, 1.0));
        let item = Aabb::new(Vec3::new(0.4, 0.5, 0.4), Vec3::new(0.6, 0.75, 0.6));
        assert_eq!(floor.clip_y(item, -2.0), -0.5);
        assert_eq!(floor.clip_y(item, -0.25), -0.25);
        assert_eq!(floor.clip_y(item, 1.0), 1.0, "moving away");
        let beside = item.offset(Vec3::new(2.0, 0.0, 0.0));
        assert_eq!(floor.clip_y(beside, -2.0), -2.0, "no shared x range");
        let wall = Aabb::new(Vec3::new(1.0, 0.0, 0.0), Vec3::new(2.0, 1.0, 1.0));
        assert_eq!(wall.clip_x(item, 3.0), 1.0 - 0.6);
        assert_eq!(wall.clip_z(item, 3.0), 3.0, "not ahead on z");
        let stretched = item.stretch(Vec3::new(-1.0, 2.0, 0.0));
        assert_eq!(stretched.min, Vec3::new(-0.6, 0.5, 0.4));
        assert_eq!(stretched.max, Vec3::new(0.6, 2.75, 0.6));
        assert_eq!(
            Vec3::new(3.0, 0.0, 4.0).normalize(),
            Vec3::new(0.6, 0.0, 0.8)
        );
        assert_eq!(Vec3::ZERO.normalize(), Vec3::ZERO);
    }
}
