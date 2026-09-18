//! Quantized vector element encodings specified by `docs/FORMAT.md`.
//!
//! This module deliberately stops at one non-null vector's value bytes. Node
//! group validity bitmaps, payload runs, and catalog wiring remain the storage
//! layer's responsibility.

use thiserror::Error;

const I8_METADATA_LEN: usize = 8;
const B1_ROTATION_ROUNDS: usize = 4;
const INVERSE_SQRT_TWO: f32 = f32::from_bits(0x3f35_04f3);

/// An error found while decoding quantized vector bytes.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum VectorEncodingError {
    /// The byte length cannot represent the requested encoding or dimension.
    #[error("{encoding} byte length is {actual}, expected {expected}")]
    InvalidLength {
        /// Name of the element encoding being decoded.
        encoding: &'static str,
        /// Number of bytes supplied by the caller.
        actual: usize,
        /// Exact number of bytes required by the encoding.
        expected: usize,
    },

    /// An i8 vector contains invalid scale, offset, or reserved code bytes.
    #[error("i8 metadata or code bytes are invalid")]
    InvalidI8,

    /// Unused high bits in the final b1 byte are not zero.
    #[error("b1 padding bits are not zero for dimension {dimension}")]
    NonZeroB1Padding {
        /// Number of meaningful packed bits.
        dimension: usize,
    },

    /// Cosine distance is undefined for an empty or zero-length query.
    #[error("b1 cosine distance requires a non-empty query with non-zero norm")]
    ZeroNormQuery,
}

/// Per-vector metadata stored before i8 code bytes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct I8Metadata {
    /// Symmetric quantizer step. Zero denotes a constant vector.
    pub scale: f32,
    /// Center of the symmetric quantization interval.
    pub offset: f32,
}

impl I8Metadata {
    /// Encodes `scale` followed by `offset` as little-endian binary32.
    #[must_use]
    pub fn to_le_bytes(self) -> [u8; I8_METADATA_LEN] {
        let mut bytes = [0_u8; I8_METADATA_LEN];
        bytes[..4].copy_from_slice(&self.scale.to_le_bytes());
        bytes[4..].copy_from_slice(&self.offset.to_le_bytes());
        bytes
    }

    /// Decodes little-endian binary32 `scale` and `offset` metadata.
    #[must_use]
    pub fn from_le_bytes(bytes: [u8; I8_METADATA_LEN]) -> Self {
        Self {
            scale: f32::from_le_bytes(copy_array(&bytes[..4])),
            offset: f32::from_le_bytes(copy_array(&bytes[4..])),
        }
    }
}

/// Encodes finite binary32 values as little-endian IEEE-754 binary16.
///
/// Conversion uses round-to-nearest, ties-to-even. As required by the storage
/// boundary contract, finiteness is checked with a debug assertion here.
#[must_use]
pub fn encode_f16(values: &[f32]) -> Vec<u8> {
    debug_assert!(values.iter().all(|value| value.is_finite()));
    let mut encoded = Vec::with_capacity(values.len() * 2);
    for value in values {
        encoded.extend(f32_to_f16_bits(*value).to_le_bytes());
    }
    encoded
}

/// Decodes little-endian IEEE-754 binary16 values to binary32.
pub fn decode_f16(bytes: &[u8]) -> Result<Vec<f32>, VectorEncodingError> {
    if !bytes.len().is_multiple_of(2) {
        return Err(VectorEncodingError::InvalidLength {
            encoding: "f16",
            actual: bytes.len(),
            expected: bytes.len() + 1,
        });
    }
    Ok(bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|chunk| f16_bits_to_f32(u16::from_le_bytes(copy_array(chunk))))
        .collect())
}

/// Encodes one finite vector using symmetric per-vector i8 quantization.
///
/// The returned bytes are `scale`, `offset`, then one two's-complement i8 code
/// per input value. Code -128 is reserved and is never emitted.
#[must_use]
pub fn encode_i8(values: &[f32]) -> Vec<u8> {
    debug_assert!(values.iter().all(|value| value.is_finite()));
    let metadata = i8_metadata(values);
    let mut encoded = Vec::with_capacity(I8_METADATA_LEN + values.len());
    encoded.extend(metadata.to_le_bytes());
    encoded.extend(
        values
            .iter()
            .map(|value| quantize_i8(*value, metadata) as u8),
    );
    encoded
}

/// Reads the scale and offset at the start of an encoded i8 vector.
pub fn decode_i8_metadata(bytes: &[u8]) -> Result<I8Metadata, VectorEncodingError> {
    if bytes.len() < I8_METADATA_LEN {
        return Err(VectorEncodingError::InvalidLength {
            encoding: "i8 metadata",
            actual: bytes.len(),
            expected: I8_METADATA_LEN,
        });
    }
    Ok(I8Metadata::from_le_bytes(copy_array(
        &bytes[..I8_METADATA_LEN],
    )))
}

/// Decodes a symmetric per-vector i8 value slot to binary32 values.
pub fn decode_i8(bytes: &[u8]) -> Result<Vec<f32>, VectorEncodingError> {
    let metadata = decode_i8_metadata(bytes)?;
    let codes = &bytes[I8_METADATA_LEN..];
    validate_i8(metadata, codes)?;
    if metadata.scale == 0.0 {
        return Ok(vec![metadata.offset; codes.len()]);
    }
    Ok(codes
        .iter()
        .map(|code| f32::from(*code as i8) * metadata.scale + metadata.offset)
        .collect())
}

/// Applies the seeded b1 rotation without quantizing its output.
///
/// This is exposed so distance kernels and format tests can share exactly the
/// transform used by [`encode_b1`].
#[must_use]
pub fn rotate_b1(values: &[f32], seed: u64) -> Vec<f32> {
    debug_assert!(values.iter().all(|value| value.is_finite()));
    let schedule = RotationSchedule::new(values.len(), seed);
    schedule.apply(values)
}

/// Encodes the signs of a seeded deterministic rotation at one bit per value.
///
/// Bits are packed least-significant-bit first; a set bit means non-negative.
/// Unused high bits in the last byte remain zero.
#[must_use]
pub fn encode_b1(values: &[f32], seed: u64) -> Vec<u8> {
    let rotated = rotate_b1(values, seed);
    let mut encoded = vec![0_u8; values.len().div_ceil(8)];
    for (index, value) in rotated.iter().enumerate() {
        if *value >= 0.0 {
            encoded[index / 8] |= 1 << (index % 8);
        }
    }
    encoded
}

/// Decodes packed b1 values as `-1.0` or `1.0` in rotated coordinates.
pub fn decode_b1_signs(bytes: &[u8], dimension: usize) -> Result<Vec<f32>, VectorEncodingError> {
    validate_b1(bytes, dimension)?;
    Ok((0..dimension)
        .map(|index| {
            if bytes[index / 8] & (1 << (index % 8)) == 0 {
                -1.0
            } else {
                1.0
            }
        })
        .collect())
}

/// Dequantizes packed b1 values back into the original coordinate system.
pub fn decode_b1(
    bytes: &[u8],
    dimension: usize,
    seed: u64,
) -> Result<Vec<f32>, VectorEncodingError> {
    let signs = decode_b1_signs(bytes, dimension)?;
    Ok(RotationSchedule::new(dimension, seed).apply_inverse(&signs))
}

/// Estimates cosine distance asymmetrically from an f32 query and a b1 code.
///
/// The candidate is modeled by its rotated sign vector normalized by
/// `sqrt(dimension)`. The result is suitable for oversampling; an optional
/// separate rescore payload supplies the final exact or higher-precision rank.
pub fn estimate_b1_cosine_distance(
    query: &[f32],
    encoded: &[u8],
    seed: u64,
) -> Result<f32, VectorEncodingError> {
    debug_assert!(query.iter().all(|value| value.is_finite()));
    let signs = decode_b1_signs(encoded, query.len())?;
    let norm_squared: f64 = query.iter().map(|value| f64::from(*value).powi(2)).sum();
    if query.is_empty() || norm_squared == 0.0 {
        return Err(VectorEncodingError::ZeroNormQuery);
    }
    let rotated = rotate_b1(query, seed);
    let dot: f64 = rotated
        .iter()
        .zip(signs)
        .map(|(query_value, sign)| f64::from(*query_value) * f64::from(sign))
        .sum();
    let denominator = norm_squared.sqrt() * (query.len() as f64).sqrt();
    let similarity = (dot / denominator).clamp(-1.0, 1.0);
    Ok((1.0 - similarity) as f32)
}

fn f32_to_f16_bits(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exponent = ((bits >> 23) & 0xff) as i32;
    let fraction = bits & 0x007f_ffff;
    let unbiased = exponent - 127;

    if exponent == 0xff {
        return sign | 0x7c00 | ((fraction != 0) as u16);
    }
    if unbiased > 15 {
        return sign | 0x7c00;
    }
    if unbiased >= -14 {
        return encode_normal_f16(sign, unbiased, fraction);
    }
    encode_subnormal_f16(sign, exponent, unbiased, fraction)
}

fn encode_normal_f16(sign: u16, unbiased: i32, fraction: u32) -> u16 {
    let mut half_exponent = (unbiased + 15) as u16;
    let mut half_fraction = (fraction >> 13) as u16;
    if round_up(fraction & 0x1fff, 0x1000, half_fraction) {
        half_fraction += 1;
        if half_fraction == 0x400 {
            half_fraction = 0;
            half_exponent += 1;
        }
    }
    sign | (half_exponent << 10) | half_fraction
}

fn encode_subnormal_f16(sign: u16, exponent: i32, unbiased: i32, fraction: u32) -> u16 {
    if exponent == 0 || unbiased < -25 {
        return sign;
    }
    let significand = 0x0080_0000 | fraction;
    let shift = (-(unbiased + 1)) as u32;
    let mut half_fraction = significand >> shift;
    let remainder_mask = (1_u32 << shift) - 1;
    let remainder = significand & remainder_mask;
    if round_up(remainder, 1_u32 << (shift - 1), half_fraction as u16) {
        half_fraction += 1;
    }
    sign | half_fraction as u16
}

fn round_up(remainder: u32, halfway: u32, retained: u16) -> bool {
    remainder > halfway || (remainder == halfway && retained & 1 != 0)
}

fn f16_bits_to_f32(bits: u16) -> f32 {
    let sign = u32::from(bits & 0x8000) << 16;
    let exponent = (bits >> 10) & 0x1f;
    let fraction = bits & 0x03ff;
    let decoded = match exponent {
        0 if fraction == 0 => sign,
        0 => decode_subnormal_f16(sign, fraction),
        0x1f => sign | 0x7f80_0000 | (u32::from(fraction) << 13),
        _ => sign | (u32::from(exponent + 112) << 23) | (u32::from(fraction) << 13),
    };
    f32::from_bits(decoded)
}

fn decode_subnormal_f16(sign: u32, fraction: u16) -> u32 {
    let mut significand = fraction;
    let mut shift = 0_u32;
    while significand & 0x0400 == 0 {
        significand <<= 1;
        shift += 1;
    }
    let exponent = 113_u32 - shift;
    sign | (exponent << 23) | (u32::from(significand & 0x03ff) << 13)
}

fn i8_metadata(values: &[f32]) -> I8Metadata {
    let Some((&first, rest)) = values.split_first() else {
        return I8Metadata {
            scale: 0.0,
            offset: 0.0,
        };
    };
    let (minimum, maximum) = rest
        .iter()
        .fold((first, first), |(minimum, maximum), value| {
            (minimum.min(*value), maximum.max(*value))
        });
    if minimum == maximum {
        return I8Metadata {
            scale: 0.0,
            offset: first,
        };
    }
    let offset = minimum * 0.5 + maximum * 0.5;
    let radius = (minimum - offset).abs().max((maximum - offset).abs());
    I8Metadata {
        scale: radius / 127.0,
        offset,
    }
}

fn quantize_i8(value: f32, metadata: I8Metadata) -> i8 {
    if metadata.scale == 0.0 {
        return 0;
    }
    ((value - metadata.offset) / metadata.scale)
        .round_ties_even()
        .clamp(-127.0, 127.0) as i8
}

fn validate_i8(metadata: I8Metadata, codes: &[u8]) -> Result<(), VectorEncodingError> {
    if !metadata.scale.is_finite()
        || metadata.scale < 0.0
        || !metadata.offset.is_finite()
        || codes.contains(&0x80)
        || (metadata.scale == 0.0 && codes.iter().any(|code| *code != 0))
    {
        return Err(VectorEncodingError::InvalidI8);
    }
    Ok(())
}

fn validate_b1(bytes: &[u8], dimension: usize) -> Result<(), VectorEncodingError> {
    let expected = dimension.div_ceil(8);
    if bytes.len() != expected {
        return Err(VectorEncodingError::InvalidLength {
            encoding: "b1",
            actual: bytes.len(),
            expected,
        });
    }
    let used_bits = dimension % 8;
    if used_bits != 0 && bytes.last().is_some_and(|byte| byte >> used_bits != 0) {
        return Err(VectorEncodingError::NonZeroB1Padding { dimension });
    }
    Ok(())
}

struct RotationSchedule {
    negative_signs: Vec<bool>,
    permutations: Vec<Vec<usize>>,
}

impl RotationSchedule {
    fn new(dimension: usize, seed: u64) -> Self {
        let mut generator = SplitMix64::new(seed);
        let negative_signs = (0..dimension).map(|_| generator.next() & 1 != 0).collect();
        let permutations = (0..B1_ROTATION_ROUNDS)
            .map(|_| shuffled_indices(dimension, &mut generator))
            .collect();
        Self {
            negative_signs,
            permutations,
        }
    }

    fn apply(&self, values: &[f32]) -> Vec<f32> {
        let mut current: Vec<f32> = values
            .iter()
            .zip(&self.negative_signs)
            .map(|(value, negative)| if *negative { -*value } else { *value })
            .collect();
        for permutation in &self.permutations {
            let mut next: Vec<f32> = permutation.iter().map(|index| current[*index]).collect();
            butterfly_pairs(&mut next);
            current = next;
        }
        current
    }

    fn apply_inverse(&self, values: &[f32]) -> Vec<f32> {
        let mut current = values.to_vec();
        for permutation in self.permutations.iter().rev() {
            butterfly_pairs(&mut current);
            let mut next = vec![0.0; current.len()];
            for (output, input) in permutation.iter().enumerate() {
                next[*input] = current[output];
            }
            current = next;
        }
        for (value, negative) in current.iter_mut().zip(&self.negative_signs) {
            if *negative {
                *value = -*value;
            }
        }
        current
    }
}

fn shuffled_indices(dimension: usize, generator: &mut SplitMix64) -> Vec<usize> {
    let mut permutation: Vec<usize> = (0..dimension).collect();
    for upper in (1..dimension).rev() {
        let selected = (generator.next() % (upper as u64 + 1)) as usize;
        permutation.swap(upper, selected);
    }
    permutation
}

fn butterfly_pairs(values: &mut [f32]) {
    for pair in values.as_chunks_mut::<2>().0 {
        let left = pair[0];
        let right = pair[1];
        pair[0] = (left + right) * INVERSE_SQRT_TWO;
        pair[1] = (left - right) * INVERSE_SQRT_TWO;
    }
}

struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }
}

fn copy_array<const N: usize>(bytes: &[u8]) -> [u8; N] {
    let mut array = [0_u8; N];
    array.copy_from_slice(bytes);
    array
}
