//! Runtime-dispatched vector distance kernels.
//!
//! The scalar implementations are always compiled. Architecture-specific
//! kernels are selected on their first use and cached, so the binary never
//! requires CPU features beyond the target architecture's baseline.

#![allow(
    unsafe_code,
    reason = "std::arch SIMD kernels require unsafe loads and target-feature calls"
)]

use std::sync::OnceLock;

type DistanceKernel = fn(&[f32], &[f32]) -> f32;

static L2_KERNEL: OnceLock<DistanceKernel> = OnceLock::new();
static COSINE_KERNEL: OnceLock<DistanceKernel> = OnceLock::new();

/// Returns the squared Euclidean distance between two equal-length vectors.
///
/// The implementation is selected once at runtime, with an always-present
/// scalar fallback.
#[must_use]
pub fn l2_squared(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    L2_KERNEL.get_or_init(select_l2_kernel)(a, b)
}

/// Returns cosine distance (`1 - cosine similarity`) for equal-length vectors.
///
/// If either vector has zero norm, this returns `2.0`, the maximum cosine
/// distance. The implementation is selected once at runtime, with an
/// always-present scalar fallback.
#[must_use]
pub fn cosine_distance(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    COSINE_KERNEL.get_or_init(select_cosine_kernel)(a, b)
}

#[allow(
    dead_code,
    reason = "the required scalar fallback is unreachable on baseline-SSE2 x86_64 builds"
)]
fn scalar_l2_squared(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    a.iter()
        .zip(b)
        .map(|(left, right)| {
            let difference = left - right;
            difference * difference
        })
        .sum()
}

#[allow(
    dead_code,
    reason = "the required scalar fallback is unreachable on baseline-SSE2 x86_64 builds"
)]
fn scalar_cosine_distance(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    let (dot, norm_a, norm_b) = a.iter().zip(b).fold(
        (0.0_f32, 0.0_f32, 0.0_f32),
        |(dot, norm_a, norm_b), (left, right)| {
            (
                dot + left * right,
                norm_a + left * left,
                norm_b + right * right,
            )
        },
    );
    cosine_from_sums(dot, norm_a, norm_b)
}

fn cosine_from_sums(dot: f32, norm_a: f32, norm_b: f32) -> f32 {
    if norm_a == 0.0 || norm_b == 0.0 {
        2.0
    } else {
        1.0 - dot / (norm_a.sqrt() * norm_b.sqrt())
    }
}

#[cfg(target_arch = "x86_64")]
fn select_l2_kernel() -> DistanceKernel {
    if std::arch::is_x86_feature_detected!("avx2") {
        x86::avx2_l2_squared
    } else {
        x86::sse2_l2_squared
    }
}

#[cfg(target_arch = "x86_64")]
fn select_cosine_kernel() -> DistanceKernel {
    if std::arch::is_x86_feature_detected!("avx2") {
        x86::avx2_cosine_distance
    } else {
        x86::sse2_cosine_distance
    }
}

#[cfg(target_arch = "aarch64")]
fn select_l2_kernel() -> DistanceKernel {
    if std::arch::is_aarch64_feature_detected!("neon") {
        aarch64::neon_l2_squared
    } else {
        scalar_l2_squared
    }
}

#[cfg(target_arch = "aarch64")]
fn select_cosine_kernel() -> DistanceKernel {
    if std::arch::is_aarch64_feature_detected!("neon") {
        aarch64::neon_cosine_distance
    } else {
        scalar_cosine_distance
    }
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn select_l2_kernel() -> DistanceKernel {
    scalar_l2_squared
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn select_cosine_kernel() -> DistanceKernel {
    scalar_cosine_distance
}

#[cfg(target_arch = "x86_64")]
mod x86 {
    use std::arch::x86_64::{
        __m128, __m256, _mm_add_ps, _mm_loadu_ps, _mm_mul_ps, _mm_setzero_ps, _mm_storeu_ps,
        _mm_sub_ps, _mm256_add_ps, _mm256_loadu_ps, _mm256_mul_ps, _mm256_setzero_ps,
        _mm256_storeu_ps, _mm256_sub_ps,
    };

    use super::cosine_from_sums;

    pub(super) fn sse2_l2_squared(a: &[f32], b: &[f32]) -> f32 {
        // SAFETY: SSE2 is guaranteed by the x86_64 architecture baseline.
        unsafe { sse2_l2_squared_impl(a, b) }
    }

    pub(super) fn sse2_cosine_distance(a: &[f32], b: &[f32]) -> f32 {
        // SAFETY: SSE2 is guaranteed by the x86_64 architecture baseline.
        unsafe { sse2_cosine_distance_impl(a, b) }
    }

    pub(super) fn avx2_l2_squared(a: &[f32], b: &[f32]) -> f32 {
        // SAFETY: this wrapper is installed only after runtime AVX2 detection.
        unsafe { avx2_l2_squared_impl(a, b) }
    }

    pub(super) fn avx2_cosine_distance(a: &[f32], b: &[f32]) -> f32 {
        // SAFETY: this wrapper is installed only after runtime AVX2 detection.
        unsafe { avx2_cosine_distance_impl(a, b) }
    }

    #[target_feature(enable = "sse2")]
    unsafe fn sse2_l2_squared_impl(a: &[f32], b: &[f32]) -> f32 {
        let mut sum = _mm_setzero_ps();
        let mut index = 0;
        while index + 4 <= a.len() {
            // SAFETY: the loop condition proves that four elements remain in
            // each equal-length slice; unaligned loads accept any alignment.
            let (left, right) = unsafe {
                (
                    _mm_loadu_ps(a.as_ptr().add(index)),
                    _mm_loadu_ps(b.as_ptr().add(index)),
                )
            };
            let difference = _mm_sub_ps(left, right);
            sum = _mm_add_ps(sum, _mm_mul_ps(difference, difference));
            index += 4;
        }
        horizontal_sum_128(sum) + scalar_l2_tail(a, b, index)
    }

    #[target_feature(enable = "sse2")]
    unsafe fn sse2_cosine_distance_impl(a: &[f32], b: &[f32]) -> f32 {
        let mut dot = _mm_setzero_ps();
        let mut norm_a = _mm_setzero_ps();
        let mut norm_b = _mm_setzero_ps();
        let mut index = 0;
        while index + 4 <= a.len() {
            // SAFETY: the loop condition proves that four elements remain in
            // each equal-length slice; unaligned loads accept any alignment.
            let (left, right) = unsafe {
                (
                    _mm_loadu_ps(a.as_ptr().add(index)),
                    _mm_loadu_ps(b.as_ptr().add(index)),
                )
            };
            dot = _mm_add_ps(dot, _mm_mul_ps(left, right));
            norm_a = _mm_add_ps(norm_a, _mm_mul_ps(left, left));
            norm_b = _mm_add_ps(norm_b, _mm_mul_ps(right, right));
            index += 4;
        }
        let (dot, norm_a, norm_b) = cosine_tail(
            a,
            b,
            index,
            horizontal_sum_128(dot),
            horizontal_sum_128(norm_a),
            horizontal_sum_128(norm_b),
        );
        cosine_from_sums(dot, norm_a, norm_b)
    }

    #[target_feature(enable = "avx2")]
    unsafe fn avx2_l2_squared_impl(a: &[f32], b: &[f32]) -> f32 {
        let mut sum = _mm256_setzero_ps();
        let mut index = 0;
        while index + 8 <= a.len() {
            // SAFETY: the loop condition proves that eight elements remain in
            // each equal-length slice; unaligned loads accept any alignment.
            let (left, right) = unsafe {
                (
                    _mm256_loadu_ps(a.as_ptr().add(index)),
                    _mm256_loadu_ps(b.as_ptr().add(index)),
                )
            };
            let difference = _mm256_sub_ps(left, right);
            sum = _mm256_add_ps(sum, _mm256_mul_ps(difference, difference));
            index += 8;
        }
        horizontal_sum_256(sum) + scalar_l2_tail(a, b, index)
    }

    #[target_feature(enable = "avx2")]
    unsafe fn avx2_cosine_distance_impl(a: &[f32], b: &[f32]) -> f32 {
        let mut dot = _mm256_setzero_ps();
        let mut norm_a = _mm256_setzero_ps();
        let mut norm_b = _mm256_setzero_ps();
        let mut index = 0;
        while index + 8 <= a.len() {
            // SAFETY: the loop condition proves that eight elements remain in
            // each equal-length slice; unaligned loads accept any alignment.
            let (left, right) = unsafe {
                (
                    _mm256_loadu_ps(a.as_ptr().add(index)),
                    _mm256_loadu_ps(b.as_ptr().add(index)),
                )
            };
            dot = _mm256_add_ps(dot, _mm256_mul_ps(left, right));
            norm_a = _mm256_add_ps(norm_a, _mm256_mul_ps(left, left));
            norm_b = _mm256_add_ps(norm_b, _mm256_mul_ps(right, right));
            index += 8;
        }
        let (dot, norm_a, norm_b) = cosine_tail(
            a,
            b,
            index,
            horizontal_sum_256(dot),
            horizontal_sum_256(norm_a),
            horizontal_sum_256(norm_b),
        );
        cosine_from_sums(dot, norm_a, norm_b)
    }

    fn horizontal_sum_128(value: __m128) -> f32 {
        let mut lanes = [0.0; 4];
        // SAFETY: `lanes` has space for four f32 values; unaligned stores
        // accept its alignment.
        unsafe { _mm_storeu_ps(lanes.as_mut_ptr(), value) };
        lanes.into_iter().sum()
    }

    fn horizontal_sum_256(value: __m256) -> f32 {
        let mut lanes = [0.0; 8];
        // SAFETY: `lanes` has space for eight f32 values; unaligned stores
        // accept its alignment.
        unsafe { _mm256_storeu_ps(lanes.as_mut_ptr(), value) };
        lanes.into_iter().sum()
    }

    fn scalar_l2_tail(a: &[f32], b: &[f32], start: usize) -> f32 {
        a[start..]
            .iter()
            .zip(&b[start..])
            .map(|(left, right)| {
                let difference = left - right;
                difference * difference
            })
            .sum()
    }

    fn cosine_tail(
        a: &[f32],
        b: &[f32],
        start: usize,
        mut dot: f32,
        mut norm_a: f32,
        mut norm_b: f32,
    ) -> (f32, f32, f32) {
        for (left, right) in a[start..].iter().zip(&b[start..]) {
            dot += left * right;
            norm_a += left * left;
            norm_b += right * right;
        }
        (dot, norm_a, norm_b)
    }
}

#[cfg(target_arch = "aarch64")]
mod aarch64 {
    use std::arch::aarch64::{float32x4_t, vaddq_f32, vld1q_f32, vmulq_f32, vst1q_f32, vsubq_f32};

    use super::cosine_from_sums;

    pub(super) fn neon_l2_squared(a: &[f32], b: &[f32]) -> f32 {
        // SAFETY: this wrapper is installed only after runtime NEON detection.
        unsafe { neon_l2_squared_impl(a, b) }
    }

    pub(super) fn neon_cosine_distance(a: &[f32], b: &[f32]) -> f32 {
        // SAFETY: this wrapper is installed only after runtime NEON detection.
        unsafe { neon_cosine_distance_impl(a, b) }
    }

    #[target_feature(enable = "neon")]
    unsafe fn neon_l2_squared_impl(a: &[f32], b: &[f32]) -> f32 {
        // SAFETY: an all-zero bit pattern is a valid zero-valued NEON vector.
        let mut sum = unsafe { std::mem::zeroed() };
        let mut index = 0;
        while index + 4 <= a.len() {
            // SAFETY: the loop condition proves four readable elements remain
            // in each equal-length slice; NEON loads permit unaligned pointers.
            let (left, right) = unsafe {
                (
                    vld1q_f32(a.as_ptr().add(index)),
                    vld1q_f32(b.as_ptr().add(index)),
                )
            };
            let difference = vsubq_f32(left, right);
            sum = vaddq_f32(sum, vmulq_f32(difference, difference));
            index += 4;
        }
        horizontal_sum(sum) + scalar_l2_tail(a, b, index)
    }

    #[target_feature(enable = "neon")]
    unsafe fn neon_cosine_distance_impl(a: &[f32], b: &[f32]) -> f32 {
        // SAFETY: an all-zero bit pattern is a valid zero-valued NEON vector.
        let mut dot = unsafe { std::mem::zeroed() };
        // SAFETY: an all-zero bit pattern is a valid zero-valued NEON vector.
        let mut norm_a = unsafe { std::mem::zeroed() };
        // SAFETY: an all-zero bit pattern is a valid zero-valued NEON vector.
        let mut norm_b = unsafe { std::mem::zeroed() };
        let mut index = 0;
        while index + 4 <= a.len() {
            // SAFETY: the loop condition proves four readable elements remain
            // in each equal-length slice; NEON loads permit unaligned pointers.
            let (left, right) = unsafe {
                (
                    vld1q_f32(a.as_ptr().add(index)),
                    vld1q_f32(b.as_ptr().add(index)),
                )
            };
            dot = vaddq_f32(dot, vmulq_f32(left, right));
            norm_a = vaddq_f32(norm_a, vmulq_f32(left, left));
            norm_b = vaddq_f32(norm_b, vmulq_f32(right, right));
            index += 4;
        }
        let (dot, norm_a, norm_b) = cosine_tail(
            a,
            b,
            index,
            horizontal_sum(dot),
            horizontal_sum(norm_a),
            horizontal_sum(norm_b),
        );
        cosine_from_sums(dot, norm_a, norm_b)
    }

    fn horizontal_sum(value: float32x4_t) -> f32 {
        let mut lanes = [0.0; 4];
        // SAFETY: `lanes` has space for four f32 values.
        unsafe { vst1q_f32(lanes.as_mut_ptr(), value) };
        lanes.into_iter().sum()
    }

    fn scalar_l2_tail(a: &[f32], b: &[f32], start: usize) -> f32 {
        a[start..]
            .iter()
            .zip(&b[start..])
            .map(|(left, right)| {
                let difference = left - right;
                difference * difference
            })
            .sum()
    }

    fn cosine_tail(
        a: &[f32],
        b: &[f32],
        start: usize,
        mut dot: f32,
        mut norm_a: f32,
        mut norm_b: f32,
    ) -> (f32, f32, f32) {
        for (left, right) in a[start..].iter().zip(&b[start..]) {
            dot += left * right;
            norm_a += left * left;
            norm_b += right * right;
        }
        (dot, norm_a, norm_b)
    }
}

#[cfg(test)]
mod tests {
    use super::{cosine_distance, l2_squared, scalar_cosine_distance, scalar_l2_squared};

    const LENGTHS: [usize; 7] = [1, 3, 7, 8, 9, 17, 768];

    struct Lcg(u64);

    impl Lcg {
        fn new(seed: u64) -> Self {
            Self(seed)
        }

        fn next_f32(&mut self) -> f32 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let fraction = (self.0 >> 40) as f32 / (1_u32 << 24) as f32;
            2.0 * fraction - 1.0
        }
    }

    #[test]
    fn scalar_known_answers_and_zero_norm_edge() {
        assert_eq!(scalar_l2_squared(&[1.0, 2.0], &[4.0, 6.0]), 25.0);
        assert_relative_eq(scalar_cosine_distance(&[1.0, 0.0], &[0.0, 1.0]), 1.0);
        assert_eq!(scalar_cosine_distance(&[0.0, 0.0], &[1.0, 2.0]), 2.0);
        assert_eq!(scalar_cosine_distance(&[], &[]), 2.0);
    }

    #[test]
    fn dispatched_l2_agrees_with_scalar_on_seeded_remainders() {
        assert_kernel_matches(l2_squared, scalar_l2_squared, 0x5eed_1234);
    }

    #[test]
    fn dispatched_cosine_agrees_with_scalar_on_seeded_remainders() {
        assert_kernel_matches(cosine_distance, scalar_cosine_distance, 0xc051_9e5e);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn sse2_kernels_agree_with_scalar_on_seeded_remainders() {
        assert_kernel_matches(super::x86::sse2_l2_squared, scalar_l2_squared, 0x55e2);
        assert_kernel_matches(
            super::x86::sse2_cosine_distance,
            scalar_cosine_distance,
            0xc055e2,
        );
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn avx2_kernels_agree_with_scalar_when_available() {
        if !std::arch::is_x86_feature_detected!("avx2") {
            return;
        }
        assert_kernel_matches(super::x86::avx2_l2_squared, scalar_l2_squared, 0xa72001);
        assert_kernel_matches(
            super::x86::avx2_cosine_distance,
            scalar_cosine_distance,
            0xc05a_7202,
        );
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn neon_kernels_agree_with_scalar_when_available() {
        if !std::arch::is_aarch64_feature_detected!("neon") {
            return;
        }
        assert_kernel_matches(
            super::aarch64::neon_l2_squared,
            scalar_l2_squared,
            0x4e30_0001,
        );
        assert_kernel_matches(
            super::aarch64::neon_cosine_distance,
            scalar_cosine_distance,
            0xc054_e302,
        );
    }

    fn assert_kernel_matches(
        kernel: fn(&[f32], &[f32]) -> f32,
        reference: fn(&[f32], &[f32]) -> f32,
        seed: u64,
    ) {
        let mut random = Lcg::new(seed);
        for length in LENGTHS {
            let left = (0..length).map(|_| random.next_f32()).collect::<Vec<_>>();
            let right = (0..length).map(|_| random.next_f32()).collect::<Vec<_>>();
            assert_relative_eq(kernel(&left, &right), reference(&left, &right));
        }
    }

    fn assert_relative_eq(actual: f32, expected: f32) {
        let tolerance = 1.0e-4 * expected.abs().max(1.0);
        assert!(
            (actual - expected).abs() <= tolerance,
            "actual {actual}, expected {expected}, tolerance {tolerance}"
        );
    }
}
