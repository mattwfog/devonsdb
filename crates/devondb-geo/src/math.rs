//! Deterministic floating-point math for DevonGrid assignment paths.
//!
//! Platform libm is **FORBIDDEN** in this crate's runtime paths. This module is
//! the only permitted trig door: all sine, cosine, inverse-trig, and `atan2`
//! calls used by DevonGrid must come through the functions below. The trig
//! implementations are pure-Rust ports of the musl-derived algorithms in
//! `libm` 0.2.16. `sqrt_det` alone wraps Rust's IEEE-754 intrinsic, as allowed
//! by `docs/GEO.md` section 4.
//!
//! The ported code originates in FreeBSD msun files carrying the following
//! notice (also retained function-by-function in provenance comments):
//!
//! Copyright (C) 1993 by Sun Microsystems, Inc. All rights reserved.
//!
//! Developed at SunPro/SunSoft, a Sun Microsystems, Inc. business. Permission
//! to use, copy, modify, and distribute this software is freely granted,
//! provided that this notice is preserved.

// These are source-faithful split constants and polynomial table entries,
// not replaceable uses of Rust's rounded whole-pi constants.
#![allow(clippy::approx_constant)]

const CANONICAL_NAN: f64 = f64::from_bits(0x7ff8_0000_0000_0000);

const SIN_S1: f64 = -1.666_666_666_666_663_2e-1;
const SIN_S2: f64 = 8.333_333_333_322_49e-3;
const SIN_S3: f64 = -1.984_126_982_985_795e-4;
const SIN_S4: f64 = 2.755_731_370_707_006_8e-6;
const SIN_S5: f64 = -2.505_076_025_340_686_3e-8;
const SIN_S6: f64 = 1.589_690_995_211_55e-10;

const COS_C1: f64 = 4.166_666_666_666_66e-2;
const COS_C2: f64 = -1.388_888_888_887_411e-3;
const COS_C3: f64 = 2.480_158_728_947_673e-5;
const COS_C4: f64 = -2.755_731_435_139_066_3e-7;
const COS_C5: f64 = 2.087_572_321_298_175e-9;
const COS_C6: f64 = -1.135_964_755_778_819_5e-11;

const ASIN_PIO2_HI: f64 = 1.570_796_326_794_896_6;
const ASIN_PIO2_LO: f64 = 6.123_233_995_736_766e-17;
const ASIN_PS0: f64 = 1.666_666_666_666_666_6e-1;
const ASIN_PS1: f64 = -3.255_658_186_224_009e-1;
const ASIN_PS2: f64 = 2.012_125_321_348_629_3e-1;
const ASIN_PS3: f64 = -4.005_553_450_067_941e-2;
const ASIN_PS4: f64 = 7.915_349_942_898_145e-4;
const ASIN_PS5: f64 = 3.479_331_075_960_212e-5;
const ASIN_QS1: f64 = -2.403_394_911_734_414;
const ASIN_QS2: f64 = 2.020_945_760_233_505_7;
const ASIN_QS3: f64 = -6.882_839_716_054_533e-1;
const ASIN_QS4: f64 = 7.703_815_055_590_194e-2;

const ATAN_HI: [f64; 4] = [
    4.636_476_090_008_061e-1,
    7.853_981_633_974_483e-1,
    9.827_937_232_473_29e-1,
    1.570_796_326_794_896_6,
];
const ATAN_LO: [f64; 4] = [
    2.269_877_745_296_168_7e-17,
    3.061_616_997_868_383e-17,
    1.390_331_103_123_099_8e-17,
    6.123_233_995_736_766e-17,
];
const ATAN_COEFFICIENTS: [f64; 11] = [
    3.333_333_333_333_293e-1,
    -1.999_999_999_987_648_3e-1,
    1.428_571_427_250_346_6e-1,
    -1.111_111_040_546_235_6e-1,
    9.090_887_133_436_507e-2,
    -7.691_876_205_044_83e-2,
    6.661_073_137_387_531e-2,
    -5.833_570_133_790_573_5e-2,
    4.976_877_994_615_932_4e-2,
    -3.653_157_274_421_691_6e-2,
    1.628_582_011_536_578_3e-2,
];

const REDUCE_EPSILON: f64 = 2.220_446_049_250_313e-16;
const REDUCE_TO_INT: f64 = 1.5 / REDUCE_EPSILON;
const REDUCE_INV_PIO2: f64 = 6.366_197_723_675_814e-1;
const REDUCE_PIO2_1: f64 = 1.570_796_326_734_125_6;
const REDUCE_PIO2_1T: f64 = 6.077_100_506_506_192e-11;
const REDUCE_PIO2_2: f64 = 6.077_100_506_303_966e-11;
const REDUCE_PIO2_2T: f64 = 2.022_266_248_795_950_6e-21;
const REDUCE_PIO2_3: f64 = 2.022_266_248_711_166_5e-21;
const REDUCE_PIO2_3T: f64 = 8.478_427_660_368_9e-32;

const LARGE_INIT_JK: [usize; 4] = [3, 4, 4, 6];

// The first 66 24-bit chunks of 2/pi from libm's k_rem_pio2 table. A binary64
// argument needs at most 46 chunks; the retained margin also covers the
// algorithm's cancellation recomputation.
const LARGE_IPIO2: [i32; 66] = [
    0xA2F983, 0x6E4E44, 0x1529FC, 0x2757D1, 0xF534DD, 0xC0DB62, 0x95993C, 0x439041, 0xFE5163,
    0xABDEBB, 0xC561B7, 0x246E3A, 0x424DD2, 0xE00649, 0x2EEA09, 0xD1921C, 0xFE1DEB, 0x1CB129,
    0xA73EE8, 0x8235F5, 0x2EBB44, 0x84E99C, 0x7026B4, 0x5F7E41, 0x3991D6, 0x398353, 0x39F49C,
    0x845F8B, 0xBDF928, 0x3B1FF8, 0x97FFDE, 0x05980F, 0xEF2F11, 0x8B5A0A, 0x6D1F6D, 0x367ECF,
    0x27CB09, 0xB74F46, 0x3F669E, 0x5FEA2D, 0x7527BA, 0xC7EBE5, 0xF17B3D, 0x0739F7, 0x8A5292,
    0xEA6BFB, 0x5FB11F, 0x8D5D08, 0x560330, 0x46FC7B, 0x6BABF0, 0xCFBC20, 0x9AF436, 0x1DA9E3,
    0x91615E, 0xE61B08, 0x659985, 0x5F14A0, 0x68408D, 0xFFD880, 0x4D7327, 0x310606, 0x1556CA,
    0x73A8C9, 0x60E27B, 0xC08C6B,
];

const LARGE_PIO2: [f64; 8] = [
    1.570_796_251_296_997,
    7.549_789_415_861_596e-8,
    5.390_302_529_957_765e-15,
    3.282_003_415_807_913e-22,
    1.270_655_753_080_676e-29,
    1.229_333_089_811_113_3e-36,
    2.733_700_538_164_645_6e-44,
    2.167_416_838_778_048e-51,
];

/// A Cartesian vector used for points and directions on the unit sphere.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vec3 {
    /// Cartesian x component.
    pub x: f64,
    /// Cartesian y component.
    pub y: f64,
    /// Cartesian z component.
    pub z: f64,
}

impl Vec3 {
    /// Converts latitude and longitude in radians to a unit-sphere vector.
    #[must_use]
    pub fn from_lat_lng_rad(lat: f64, lng: f64) -> Self {
        let cos_lat = cos_det(lat);
        Self {
            x: cos_lat * cos_det(lng),
            y: cos_lat * sin_det(lng),
            z: sin_det(lat),
        }
    }

    /// Converts this vector to `(latitude, longitude)` in radians.
    ///
    /// The conversion is scale-independent for every nonzero finite vector.
    #[must_use]
    pub fn to_lat_lng_rad(self) -> (f64, f64) {
        let horizontal = sqrt_det(self.x * self.x + self.y * self.y);
        (atan2_det(self.z, horizontal), atan2_det(self.y, self.x))
    }

    /// Returns the scalar dot product with `other`.
    #[must_use]
    pub fn dot(self, other: Self) -> f64 {
        self.x * other.x + self.y * other.y + self.z * other.z
    }

    /// Returns the right-handed cross product with `other`.
    #[must_use]
    pub fn cross(self, other: Self) -> Self {
        Self {
            x: self.y * other.z - self.z * other.y,
            y: self.z * other.x - self.x * other.z,
            z: self.x * other.y - self.y * other.x,
        }
    }

    /// Returns a vector with the same direction and unit length.
    ///
    /// A zero or non-finite input follows IEEE-754 arithmetic and yields
    /// non-finite components rather than consulting a platform math library.
    #[must_use]
    pub fn normalize(self) -> Self {
        let inverse_length = 1.0 / sqrt_det(self.dot(self));
        Self {
            x: self.x * inverse_length,
            y: self.y * inverse_length,
            z: self.z * inverse_length,
        }
    }
}

/// Returns the deterministic sine of `x` radians.
///
/// Provenance: ported from `libm-0.2.16/src/math/sin.rs`, whose origin is
/// FreeBSD msun `s_sin.c`.
#[must_use]
pub fn sin_det(x: f64) -> f64 {
    let ix = (x.to_bits() >> 32) as u32 & 0x7fff_ffff;
    if ix <= 0x3fe9_21fb {
        if ix < 0x3e50_0000 {
            return x;
        }
        return sin_kernel(x, 0.0, false);
    }
    if ix >= 0x7ff0_0000 {
        return CANONICAL_NAN;
    }

    let (quadrant, high, low) = rem_pio2(x);
    match quadrant & 3 {
        0 => sin_kernel(high, low, true),
        1 => cos_kernel(high, low),
        2 => -sin_kernel(high, low, true),
        _ => -cos_kernel(high, low),
    }
}

/// Returns the deterministic cosine of `x` radians.
///
/// Provenance: ported from `libm-0.2.16/src/math/cos.rs`, whose origin is
/// FreeBSD msun `s_cos.c`.
#[must_use]
pub fn cos_det(x: f64) -> f64 {
    let ix = (x.to_bits() >> 32) as u32 & 0x7fff_ffff;
    if ix <= 0x3fe9_21fb {
        if ix < 0x3e46_a09e {
            return 1.0;
        }
        return cos_kernel(x, 0.0);
    }
    if ix >= 0x7ff0_0000 {
        return CANONICAL_NAN;
    }

    let (quadrant, high, low) = rem_pio2(x);
    match quadrant & 3 {
        0 => cos_kernel(high, low),
        1 => -sin_kernel(high, low, true),
        2 => -cos_kernel(high, low),
        _ => sin_kernel(high, low, true),
    }
}

/// Returns the deterministic inverse sine of `x` in radians.
///
/// Values outside `[-1, 1]` return a canonical quiet NaN. Provenance: ported
/// from `libm-0.2.16/src/math/asin.rs`, whose origin is FreeBSD msun
/// `e_asin.c`.
#[must_use]
pub fn asin_det(mut x: f64) -> f64 {
    if x.is_nan() {
        return CANONICAL_NAN;
    }
    let high = (x.to_bits() >> 32) as u32;
    let magnitude_high = high & 0x7fff_ffff;
    if magnitude_high >= 0x3ff0_0000 {
        let low = x.to_bits() as u32;
        if ((magnitude_high - 0x3ff0_0000) | low) == 0 {
            return x * ASIN_PIO2_HI + f64::from_bits(0x3870_0000_0000_0000);
        }
        return CANONICAL_NAN;
    }
    if magnitude_high < 0x3fe0_0000 {
        if (0x0010_0000..0x3e50_0000).contains(&magnitude_high) {
            return x;
        }
        return x + x * asin_rational(x * x);
    }

    let z = (1.0 - abs_bits(x)) * 0.5;
    let root = sqrt_det(z);
    let rational = asin_rational(z);
    x = if magnitude_high >= 0x3fef_3333 {
        ASIN_PIO2_HI - (2.0 * (root + root * rational) - ASIN_PIO2_LO)
    } else {
        let truncated_root = with_low_word(root, 0);
        let correction = (z - truncated_root * truncated_root) / (root + truncated_root);
        0.5 * ASIN_PIO2_HI
            - (2.0 * root * rational
                - (ASIN_PIO2_LO - 2.0 * correction)
                - (0.5 * ASIN_PIO2_HI - 2.0 * truncated_root))
    };
    if high >> 31 != 0 { -x } else { x }
}

/// Returns the deterministic inverse cosine of `x` in radians.
///
/// Values outside `[-1, 1]` return a canonical quiet NaN. Provenance: ported
/// from `libm-0.2.16/src/math/acos.rs`, whose origin is FreeBSD msun
/// `e_acos.c`.
#[must_use]
pub fn acos_det(x: f64) -> f64 {
    if x.is_nan() {
        return CANONICAL_NAN;
    }
    let high = (x.to_bits() >> 32) as u32;
    let magnitude_high = high & 0x7fff_ffff;
    if magnitude_high >= 0x3ff0_0000 {
        let low = x.to_bits() as u32;
        if ((magnitude_high - 0x3ff0_0000) | low) == 0 {
            return if high >> 31 != 0 {
                2.0 * ASIN_PIO2_HI + f64::from_bits(0x3870_0000_0000_0000)
            } else {
                0.0
            };
        }
        return CANONICAL_NAN;
    }
    if magnitude_high < 0x3fe0_0000 {
        if magnitude_high <= 0x3c60_0000 {
            return ASIN_PIO2_HI + f64::from_bits(0x3870_0000_0000_0000);
        }
        return ASIN_PIO2_HI - (x - (ASIN_PIO2_LO - x * asin_rational(x * x)));
    }

    if high >> 31 != 0 {
        let z = (1.0 + x) * 0.5;
        let root = sqrt_det(z);
        let correction = asin_rational(z) * root - ASIN_PIO2_LO;
        return 2.0 * (ASIN_PIO2_HI - (root + correction));
    }
    let z = (1.0 - x) * 0.5;
    let root = sqrt_det(z);
    let truncated_root = f64::from_bits(root.to_bits() & 0xffff_ffff_0000_0000);
    let correction = (z - truncated_root * truncated_root) / (root + truncated_root);
    let residual = asin_rational(z) * root + correction;
    2.0 * (truncated_root + residual)
}

/// Returns the deterministic four-quadrant arctangent of `y / x` in radians.
///
/// Provenance: ported from `libm-0.2.16/src/math/atan2.rs`, whose origin is
/// FreeBSD msun `e_atan2.c`; its private arctangent kernel comes from
/// `libm-0.2.16/src/math/atan.rs` / FreeBSD msun `s_atan.c`.
#[must_use]
pub fn atan2_det(y: f64, x: f64) -> f64 {
    const PI: f64 = 3.141_592_653_589_793;
    const PI_LOW: f64 = 1.224_646_799_147_353_2e-16;

    if x.is_nan() || y.is_nan() {
        return CANONICAL_NAN;
    }
    let mut ix = (x.to_bits() >> 32) as u32;
    let low_x = x.to_bits() as u32;
    let mut iy = (y.to_bits() >> 32) as u32;
    let low_y = y.to_bits() as u32;
    if (ix.wrapping_sub(0x3ff0_0000) | low_x) == 0 {
        return atan_kernel(y);
    }
    let quadrant = ((iy >> 31) & 1) | ((ix >> 30) & 2);
    ix &= 0x7fff_ffff;
    iy &= 0x7fff_ffff;

    if (iy | low_y) == 0 {
        return match quadrant {
            0 | 1 => y,
            2 => PI,
            _ => -PI,
        };
    }
    if (ix | low_x) == 0 {
        return if quadrant & 1 != 0 {
            -PI / 2.0
        } else {
            PI / 2.0
        };
    }
    if ix == 0x7ff0_0000 {
        return atan2_infinite_x(quadrant, iy == 0x7ff0_0000, PI);
    }
    if ix.wrapping_add(64 << 20) < iy || iy == 0x7ff0_0000 {
        return if quadrant & 1 != 0 {
            -PI / 2.0
        } else {
            PI / 2.0
        };
    }

    let angle = if quadrant & 2 != 0 && iy.wrapping_add(64 << 20) < ix {
        0.0
    } else {
        atan_kernel(abs_bits(y / x))
    };
    match quadrant {
        0 => angle,
        1 => -angle,
        2 => PI - (angle - PI_LOW),
        _ => (angle - PI_LOW) - PI,
    }
}

/// Returns the correctly rounded IEEE-754 square root of `x`.
///
/// This is deliberately the sole non-vendored kernel: `docs/GEO.md` section 4
/// permits Rust's hardware/std intrinsic because IEEE square root is correctly
/// rounded — and therefore bit-deterministic — on the non-negative domain.
/// Negative and NaN inputs never reach the intrinsic: the resulting NaN's
/// sign and payload are platform-defined there (x86's negative "indefinite"
/// QNaN vs ARM's positive default NaN), so this guard returns the canonical
/// quiet NaN instead. `sqrt(-0.0)` is `-0.0` per IEEE on every platform and
/// passes through.
#[must_use]
pub fn sqrt_det(x: f64) -> f64 {
    if x.is_nan() || x < 0.0 {
        return CANONICAL_NAN;
    }
    x.sqrt()
}

// Provenance: libm-0.2.16/src/math/k_sin.rs, FreeBSD msun k_sin.c.
fn sin_kernel(x: f64, tail: f64, has_tail: bool) -> f64 {
    let square = x * x;
    let fourth = square * square;
    let polynomial =
        SIN_S2 + square * (SIN_S3 + square * SIN_S4) + square * fourth * (SIN_S5 + square * SIN_S6);
    let cube = square * x;
    if has_tail {
        x - ((square * (0.5 * tail - cube * polynomial) - tail) - cube * SIN_S1)
    } else {
        x + cube * (SIN_S1 + square * polynomial)
    }
}

// Provenance: libm-0.2.16/src/math/k_cos.rs, FreeBSD msun k_cos.c.
fn cos_kernel(x: f64, tail: f64) -> f64 {
    let square = x * x;
    let fourth = square * square;
    let polynomial = square * (COS_C1 + square * (COS_C2 + square * COS_C3))
        + fourth * fourth * (COS_C4 + square * (COS_C5 + square * COS_C6));
    let half_square = 0.5 * square;
    let leading = 1.0 - half_square;
    leading + (((1.0 - leading) - half_square) + (square * polynomial - x * tail))
}

// Provenance: libm-0.2.16/src/math/asin.rs and acos.rs, FreeBSD msun
// e_asin.c and e_acos.c.
fn asin_rational(z: f64) -> f64 {
    let numerator = z
        * (ASIN_PS0
            + z * (ASIN_PS1 + z * (ASIN_PS2 + z * (ASIN_PS3 + z * (ASIN_PS4 + z * ASIN_PS5)))));
    let denominator = 1.0 + z * (ASIN_QS1 + z * (ASIN_QS2 + z * (ASIN_QS3 + z * ASIN_QS4)));
    numerator / denominator
}

// Provenance: libm-0.2.16/src/math/atan.rs, FreeBSD msun s_atan.c.
fn atan_kernel(mut x: f64) -> f64 {
    let mut high = (x.to_bits() >> 32) as u32;
    let sign = high >> 31;
    high &= 0x7fff_ffff;
    if high >= 0x4410_0000 {
        let angle = ATAN_HI[3] + f64::from_bits(0x3870_0000_0000_0000);
        return if sign != 0 { -angle } else { angle };
    }
    if high < 0x3e40_0000 {
        return x;
    }

    let interval = atan_reduce(&mut x, high);
    let square = x * x;
    let fourth = square * square;
    let odd = square
        * (ATAN_COEFFICIENTS[0]
            + fourth
                * (ATAN_COEFFICIENTS[2]
                    + fourth
                        * (ATAN_COEFFICIENTS[4]
                            + fourth
                                * (ATAN_COEFFICIENTS[6]
                                    + fourth
                                        * (ATAN_COEFFICIENTS[8]
                                            + fourth * ATAN_COEFFICIENTS[10])))));
    let even = fourth
        * (ATAN_COEFFICIENTS[1]
            + fourth
                * (ATAN_COEFFICIENTS[3]
                    + fourth
                        * (ATAN_COEFFICIENTS[5]
                            + fourth * (ATAN_COEFFICIENTS[7] + fourth * ATAN_COEFFICIENTS[9]))));
    if interval < 0 {
        return x - x * (odd + even);
    }
    let index = interval as usize;
    let angle = ATAN_HI[index] - (x * (odd + even) - ATAN_LO[index] - x);
    if sign != 0 { -angle } else { angle }
}

// Provenance: argument-reduction portion of libm-0.2.16/src/math/atan.rs,
// FreeBSD msun s_atan.c.
fn atan_reduce(x: &mut f64, high: u32) -> i32 {
    if high < 0x3fdc_0000 {
        return -1;
    }
    *x = abs_bits(*x);
    if high < 0x3ff3_0000 {
        if high < 0x3fe6_0000 {
            *x = (2.0 * *x - 1.0) / (2.0 + *x);
            0
        } else {
            *x = (*x - 1.0) / (*x + 1.0);
            1
        }
    } else if high < 0x4003_8000 {
        *x = (*x - 1.5) / (1.0 + 1.5 * *x);
        2
    } else {
        *x = -1.0 / *x;
        3
    }
}

// Provenance: special-case table in libm-0.2.16/src/math/atan2.rs,
// FreeBSD msun e_atan2.c.
fn atan2_infinite_x(quadrant: u32, y_is_infinite: bool, pi: f64) -> f64 {
    if y_is_infinite {
        match quadrant {
            0 => pi / 4.0,
            1 => -pi / 4.0,
            2 => 3.0 * pi / 4.0,
            _ => -3.0 * pi / 4.0,
        }
    } else {
        match quadrant {
            0 => 0.0,
            1 => -0.0,
            2 => pi,
            _ => -pi,
        }
    }
}

// Provenance: libm-0.2.16/src/math/rem_pio2.rs, FreeBSD msun e_rem_pio2.c.
fn rem_pio2(x: f64) -> (i32, f64, f64) {
    let sign = (x.to_bits() >> 63) as i32;
    let magnitude_high = (x.to_bits() >> 32) as u32 & 0x7fff_ffff;
    if magnitude_high <= 0x400f_6a7a {
        return rem_pio2_near(x, magnitude_high, sign);
    }
    if magnitude_high <= 0x401c_463b {
        return rem_pio2_near_two_pi(x, magnitude_high, sign);
    }
    if magnitude_high < 0x4139_21fb {
        return rem_pio2_medium(x, magnitude_high);
    }

    let mut bits = x.to_bits() & 0x000f_ffff_ffff_ffff;
    bits |= ((0x3ff + 23) as u64) << 52;
    let mut chunk_source = f64::from_bits(bits);
    let mut chunks = [0.0; 3];
    for chunk in &mut chunks[..2] {
        *chunk = chunk_source as i32 as f64;
        chunk_source = (chunk_source - *chunk) * f64::from_bits(0x4170_0000_0000_0000);
    }
    chunks[2] = chunk_source;
    let mut last = 2;
    while last != 0 && chunks[last] == 0.0 {
        last -= 1;
    }
    let mut remainder = [0.0; 3];
    let quadrant = rem_pio2_large(
        &chunks[..=last],
        &mut remainder,
        ((magnitude_high as i32) >> 20) - (0x3ff + 23),
        1,
    );
    if sign != 0 {
        (-quadrant, -remainder[0], -remainder[1])
    } else {
        (quadrant, remainder[0], remainder[1])
    }
}

// Provenance: small-argument branches in libm-0.2.16/src/math/rem_pio2.rs,
// FreeBSD msun e_rem_pio2.c.
fn rem_pio2_near(x: f64, magnitude_high: u32, sign: i32) -> (i32, f64, f64) {
    if (magnitude_high & 0x000f_ffff) == 0x0009_21fb {
        return rem_pio2_medium(x, magnitude_high);
    }
    if magnitude_high <= 0x4002_d97c {
        return subtract_pio2(x, sign, 1.0, 1);
    }
    subtract_pio2(x, sign, 2.0, 2)
}

// Provenance: small-argument branches in libm-0.2.16/src/math/rem_pio2.rs,
// FreeBSD msun e_rem_pio2.c.
fn rem_pio2_near_two_pi(x: f64, magnitude_high: u32, sign: i32) -> (i32, f64, f64) {
    if magnitude_high <= 0x4015_fdbc {
        if magnitude_high == 0x4012_d97c {
            return rem_pio2_medium(x, magnitude_high);
        }
        return subtract_pio2(x, sign, 3.0, 3);
    }
    if magnitude_high == 0x4019_21fb {
        return rem_pio2_medium(x, magnitude_high);
    }
    subtract_pio2(x, sign, 4.0, 4)
}

// Provenance: repeated small reduction cases in
// libm-0.2.16/src/math/rem_pio2.rs, FreeBSD msun e_rem_pio2.c.
fn subtract_pio2(x: f64, sign: i32, multiple: f64, quadrant: i32) -> (i32, f64, f64) {
    if sign == 0 {
        let residual = x - multiple * REDUCE_PIO2_1;
        let high = residual - multiple * REDUCE_PIO2_1T;
        let low = (residual - high) - multiple * REDUCE_PIO2_1T;
        (quadrant, high, low)
    } else {
        let residual = x + multiple * REDUCE_PIO2_1;
        let high = residual + multiple * REDUCE_PIO2_1T;
        let low = (residual - high) + multiple * REDUCE_PIO2_1T;
        (-quadrant, high, low)
    }
}

// Provenance: medium-size reduction in libm-0.2.16/src/math/rem_pio2.rs,
// FreeBSD msun e_rem_pio2.c.
fn rem_pio2_medium(x: f64, magnitude_high: u32) -> (i32, f64, f64) {
    let rounded = x * REDUCE_INV_PIO2 + REDUCE_TO_INT;
    let multiple = rounded - REDUCE_TO_INT;
    let quadrant = multiple as i32;
    let mut residual = x - multiple * REDUCE_PIO2_1;
    let mut correction = multiple * REDUCE_PIO2_1T;
    let mut high = residual - correction;
    let exponent_x = (magnitude_high >> 20) as i32;
    let mut exponent_y = ((high.to_bits() >> 52) & 0x7ff) as i32;
    if exponent_x - exponent_y > 16 {
        let previous = residual;
        correction = multiple * REDUCE_PIO2_2;
        residual = previous - correction;
        correction = multiple * REDUCE_PIO2_2T - ((previous - residual) - correction);
        high = residual - correction;
        exponent_y = ((high.to_bits() >> 52) & 0x7ff) as i32;
        if exponent_x - exponent_y > 49 {
            let previous = residual;
            correction = multiple * REDUCE_PIO2_3;
            residual = previous - correction;
            correction = multiple * REDUCE_PIO2_3T - ((previous - residual) - correction);
            high = residual - correction;
        }
    }
    let low = (residual - high) - correction;
    (quadrant, high, low)
}

// Provenance: libm-0.2.16/src/math/rem_pio2_large.rs, whose origin is
// FreeBSD msun k_rem_pio2.c. Adapted to binary64-only table bounds.
fn rem_pio2_large(x: &[f64], y: &mut [f64], exponent: i32, precision: usize) -> i32 {
    let two_to_24 = f64::from_bits(0x4170_0000_0000_0000);
    let two_to_minus_24 = f64::from_bits(0x3e70_0000_0000_0000);
    let chunk_count = x.len();
    let jk = LARGE_INIT_JK[precision];
    let last_x = chunk_count - 1;
    let mut table_start = (exponent - 3) / 24;
    if table_start < 0 {
        table_start = 0;
    }
    let mut q_exponent = exponent - 24 * (table_start + 1);
    let table_start = table_start as usize;

    let mut factors = [0.0; 20];
    let mut products = [0.0; 20];
    let mut integer_chunks = [0; 20];
    let first_table_index = table_start as i32 - last_x as i32;
    for (offset, factor) in factors[..=last_x + jk].iter_mut().enumerate() {
        let table_index = first_table_index + offset as i32;
        *factor = if table_index < 0 {
            0.0
        } else {
            LARGE_IPIO2[table_index as usize] as f64
        };
    }
    for product_index in 0..=jk {
        let mut sum = 0.0;
        for source_index in 0..=last_x {
            sum += x[source_index] * factors[last_x + product_index - source_index];
        }
        products[product_index] = sum;
    }

    let mut last_product = jk;
    let (integer, fraction, complement) = loop {
        let distilled = distill_large_product(
            &products,
            &mut integer_chunks,
            last_product,
            q_exponent,
            two_to_24,
            two_to_minus_24,
        );
        if distilled.3 != 0 {
            let extra = distilled.3;
            extend_large_product(
                x,
                &mut factors,
                &mut products,
                last_x,
                last_product,
                extra,
                table_start,
            );
            last_product += extra;
            continue;
        }
        break (distilled.0, distilled.1, distilled.2);
    };

    let last_product = normalize_large_fraction(
        fraction,
        &mut integer_chunks,
        last_product,
        &mut q_exponent,
        two_to_24,
        two_to_minus_24,
    );
    compress_large_remainder(
        y,
        &integer_chunks,
        last_product,
        q_exponent,
        precision,
        complement,
        two_to_minus_24,
    );
    integer & 7
}

// Provenance: product-distillation loop in libm-0.2.16/src/math/rem_pio2_large.rs,
// FreeBSD msun k_rem_pio2.c.
fn distill_large_product(
    products: &[f64; 20],
    chunks: &mut [i32; 20],
    last: usize,
    exponent: i32,
    two_to_24: f64,
    two_to_minus_24: f64,
) -> (i32, f64, i32, usize) {
    let mut value = products[last];
    for (reverse_index, chunk) in chunks.iter_mut().take(last).enumerate() {
        let source_index = last - reverse_index;
        let carry = (two_to_minus_24 * value) as i32 as f64;
        *chunk = (value - two_to_24 * carry) as i32;
        value = products[source_index - 1] + carry;
    }
    value = scalbn_small(value, exponent);
    value -= 8.0 * floor_bits(value * 0.125);
    let mut integer = value as i32;
    value -= f64::from(integer);
    let complement = large_complement_indicator(chunks, last, exponent, value, &mut integer);
    if complement > 0 {
        integer += 1;
        let carried = complement_large_chunks(chunks, last, exponent);
        if complement == 2 {
            value = 1.0 - value;
            if carried {
                value -= scalbn_small(1.0, exponent);
            }
        }
    }
    let extra = large_recomputation_count(chunks, last, LARGE_INIT_JK[1], value);
    (integer, value, complement, extra)
}

// Provenance: integer/complement classification in
// libm-0.2.16/src/math/rem_pio2_large.rs, FreeBSD msun k_rem_pio2.c.
fn large_complement_indicator(
    chunks: &mut [i32; 20],
    last: usize,
    exponent: i32,
    fraction: f64,
    integer: &mut i32,
) -> i32 {
    if exponent > 0 {
        let leading = chunks[last - 1] >> (24 - exponent);
        *integer += leading;
        chunks[last - 1] -= leading << (24 - exponent);
        chunks[last - 1] >> (23 - exponent)
    } else if exponent == 0 {
        chunks[last - 1] >> 23
    } else if fraction >= 0.5 {
        2
    } else {
        0
    }
}

// Provenance: cancellation recomputation test in
// libm-0.2.16/src/math/rem_pio2_large.rs, FreeBSD msun k_rem_pio2.c.
fn large_recomputation_count(chunks: &[i32; 20], last: usize, jk: usize, fraction: f64) -> usize {
    if fraction != 0.0 || chunks[jk..last].iter().any(|chunk| *chunk != 0) {
        return 0;
    }
    let mut extra = 1;
    while chunks[jk - extra] == 0 {
        extra += 1;
    }
    extra
}

// Provenance: cancellation recomputation extension in
// libm-0.2.16/src/math/rem_pio2_large.rs, FreeBSD msun k_rem_pio2.c.
fn extend_large_product(
    x: &[f64],
    factors: &mut [f64; 20],
    products: &mut [f64; 20],
    last_x: usize,
    last: usize,
    extra: usize,
    table_start: usize,
) {
    for product_index in last + 1..=last + extra {
        factors[last_x + product_index] = LARGE_IPIO2[table_start + product_index] as f64;
        let mut sum = 0.0;
        for source_index in 0..=last_x {
            sum += x[source_index] * factors[last_x + product_index - source_index];
        }
        products[product_index] = sum;
    }
}

// Provenance: one's-complement chunk step in
// libm-0.2.16/src/math/rem_pio2_large.rs, FreeBSD msun k_rem_pio2.c.
fn complement_large_chunks(chunks: &mut [i32; 20], last: usize, exponent: i32) -> bool {
    let mut carry = 0;
    for chunk in &mut chunks[..last] {
        let previous = *chunk;
        if carry == 0 && previous != 0 {
            carry = 1;
            *chunk = 0x0100_0000 - previous;
        } else if carry != 0 {
            *chunk = 0x00ff_ffff - previous;
        }
    }
    if exponent > 0 {
        match exponent {
            1 => chunks[last - 1] &= 0x007f_ffff,
            2 => chunks[last - 1] &= 0x003f_ffff,
            _ => {}
        }
    }
    carry != 0
}

// Provenance: fractional-chunk normalization in
// libm-0.2.16/src/math/rem_pio2_large.rs, FreeBSD msun k_rem_pio2.c.
fn normalize_large_fraction(
    fraction: f64,
    chunks: &mut [i32; 20],
    mut last: usize,
    exponent: &mut i32,
    two_to_24: f64,
    two_to_minus_24: f64,
) -> usize {
    if fraction == 0.0 {
        last -= 1;
        *exponent -= 24;
        while chunks[last] == 0 {
            last -= 1;
            *exponent -= 24;
        }
        return last;
    }
    let scaled = scalbn_small(fraction, -*exponent);
    if scaled >= two_to_24 {
        let carry = (two_to_minus_24 * scaled) as i32 as f64;
        chunks[last] = (scaled - two_to_24 * carry) as i32;
        last += 1;
        *exponent += 24;
        chunks[last] = carry as i32;
    } else {
        chunks[last] = scaled as i32;
    }
    last
}

// Provenance: final PI/2 multiplication and compression in
// libm-0.2.16/src/math/rem_pio2_large.rs, FreeBSD msun k_rem_pio2.c.
fn compress_large_remainder(
    output: &mut [f64],
    chunks: &[i32; 20],
    last: usize,
    exponent: i32,
    precision: usize,
    complement: i32,
    two_to_minus_24: f64,
) {
    let mut values = [0.0; 20];
    let mut scale = scalbn_small(1.0, exponent);
    for index in (0..=last).rev() {
        values[index] = scale * f64::from(chunks[index]);
        scale *= two_to_minus_24;
    }
    let mut products = [0.0; 20];
    for source_index in (0..=last).rev() {
        let mut sum = 0.0;
        let max_constant = LARGE_INIT_JK[precision].min(last - source_index);
        for constant_index in 0..=max_constant {
            sum += LARGE_PIO2[constant_index] * values[source_index + constant_index];
        }
        products[last - source_index] = sum;
    }
    let mut sum = 0.0;
    for product in products[..=last].iter().rev() {
        sum += product;
    }
    let sign = if complement == 0 { 1.0 } else { -1.0 };
    output[0] = sign * sum;
    if precision == 1 || precision == 2 {
        let mut tail = products[0] - sum;
        for product in &products[1..=last] {
            tail += product;
        }
        output[1] = sign * tail;
    }
}

// Provenance: specialized binary64 form of libm-0.2.16/src/math/generic/scalbn.rs.
// All callers here use normal, representable scale factors.
fn scalbn_small(x: f64, exponent: i32) -> f64 {
    debug_assert!((-1022..=1023).contains(&exponent));
    let biased = (1023 + exponent) as u64;
    x * f64::from_bits(biased << 52)
}

// Provenance: specialized binary64 port of
// libm-0.2.16/src/math/generic/floor.rs, whose origin is musl floor.c.
fn floor_bits(x: f64) -> f64 {
    let bits = x.to_bits();
    let exponent = ((bits >> 52) & 0x7ff) as i32 - 1023;
    if exponent >= 52 {
        return x;
    }
    if exponent < 0 {
        if bits << 1 == 0 {
            return x;
        }
        return if bits >> 63 == 0 { 0.0 } else { -1.0 };
    }
    let fractional_mask = 0x000f_ffff_ffff_ffff_u64 >> exponent;
    if bits & fractional_mask == 0 {
        return x;
    }
    let adjusted = if bits >> 63 != 0 {
        bits + fractional_mask
    } else {
        bits
    };
    f64::from_bits(adjusted & !fractional_mask)
}

// Provenance: binary64 specialization of libm-0.2.16/src/math/fabs.rs.
fn abs_bits(x: f64) -> f64 {
    f64::from_bits(x.to_bits() & 0x7fff_ffff_ffff_ffff)
}

// Provenance: libm-0.2.16/src/math/mod.rs helper with_set_low_word.
fn with_low_word(x: f64, low: u32) -> f64 {
    f64::from_bits((x.to_bits() & 0xffff_ffff_0000_0000) | u64::from(low))
}
