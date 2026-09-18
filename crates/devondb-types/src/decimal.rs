//! Exact fixed-point decimals (`docs/PLAN_IR.md` § Type system).
//!
//! `Decimal128` is an unscaled 128-bit integer paired with a scale: the
//! numeric value is `digits × 10^-scale`. Exactness is the point — money
//! must never use floating-point representation. Canonical form is enforced
//! on every construction path: scale ≤ MAX_SCALE, and the serde form is the
//! decimal string (`"123.45"`), never a JSON float, so no reader can lose
//! precision.

use std::fmt;
use std::str::FromStr;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{DevonError, DevonResult};

fn invalid_argument(context: String) -> DevonError {
    DevonError::InvalidArgument { context }
}

/// Maximum digits a `Decimal128` column may declare (i128 holds 38 full
/// decimal digits: |i128::MAX| = 1.70e38).
pub const MAX_PRECISION: u8 = 38;

/// Maximum scale (digits right of the point); scale ≤ precision always.
pub const MAX_SCALE: u8 = 38;

/// An exact fixed-point decimal: `digits × 10^-scale`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Decimal128 {
    digits: i128,
    scale: u8,
}

impl Decimal128 {
    /// Builds a decimal from an unscaled integer and a scale.
    pub fn new(digits: i128, scale: u8) -> DevonResult<Self> {
        if scale > MAX_SCALE {
            return Err(invalid_argument(format!(
                "decimal scale {scale} exceeds the maximum {MAX_SCALE}"
            )));
        }
        Ok(Self { digits, scale })
    }

    /// The unscaled integer.
    #[must_use]
    pub fn digits(self) -> i128 {
        self.digits
    }

    /// Digits right of the decimal point.
    #[must_use]
    pub fn scale(self) -> u8 {
        self.scale
    }

    /// The number of significant decimal digits in the unscaled integer
    /// (1 for zero), used to check a value against a column's declared
    /// precision.
    #[must_use]
    pub fn precision(self) -> u8 {
        let mut magnitude = self.digits.unsigned_abs();
        let mut count: u8 = 1;
        while magnitude >= 10 {
            magnitude /= 10;
            count += 1;
        }
        count
    }

    /// Whether this value fits a `Decimal(precision, scale)` column: the
    /// scales must match exactly (no silent rescaling) and the digit
    /// count must fit the declared precision.
    #[must_use]
    pub fn fits(self, precision: u8, scale: u8) -> bool {
        self.scale == scale && self.precision() <= precision
    }

    /// Rounds to `places` fractional digits using midpoint-away-from-zero,
    /// then pads back to this value's scale and checks `precision`.
    pub fn round_to_places(self, places: u8, precision: u8) -> DevonResult<Self> {
        if places > self.scale {
            return Err(invalid_argument(format!(
                "decimal rounding places {places} exceeds scale {}",
                self.scale
            )));
        }
        let divisor = power_of_ten(self.scale - places);
        let magnitude = self.digits.unsigned_abs();
        let quotient = magnitude / divisor;
        let remainder = magnitude % divisor;
        let rounded = round_magnitude(quotient, remainder, divisor)?;
        let digits = signed_magnitude(rounded, self.digits < 0)?;
        Self::new(digits, places)?.pad_to_scale(self.scale, precision)
    }

    /// Divides two decimals as one exact rational, rounds the quotient once
    /// at `places` using midpoint-away-from-zero, and pads to this numerator's
    /// scale while checking `precision`.
    pub fn round_div_to_places(
        self,
        denominator: Self,
        places: u8,
        precision: u8,
    ) -> DevonResult<Self> {
        if denominator.digits == 0 {
            return Err(invalid_argument("decimal division by zero".to_owned()));
        }
        if places > self.scale {
            return Err(invalid_argument(format!(
                "decimal division rounding places {places} exceeds result scale {}",
                self.scale
            )));
        }
        let exponent = i16::from(denominator.scale) + i16::from(places) - i16::from(self.scale);
        let magnitude = rounded_ratio_magnitude(
            self.digits.unsigned_abs(),
            denominator.digits.unsigned_abs(),
            exponent,
        )?;
        let negative = (self.digits < 0) != (denominator.digits < 0);
        let digits = signed_magnitude(magnitude, negative)?;
        Self::new(digits, places)?.pad_to_scale(self.scale, precision)
    }

    /// Adds two decimals of the same scale exactly, rejecting scale
    /// mismatches and any result past [`MAX_PRECISION`] digits (never a
    /// wrap, following the refuse-never-round law).
    pub fn checked_add_same_scale(self, other: Self) -> DevonResult<Self> {
        self.require_same_scale("addition", other)?;
        checked_result(self.digits.checked_add(other.digits), self.scale)
    }

    /// Subtracts two decimals of the same scale exactly, rejecting scale
    /// mismatches and any result past [`MAX_PRECISION`] digits (never a
    /// wrap, following the refuse-never-round law).
    pub fn checked_sub_same_scale(self, other: Self) -> DevonResult<Self> {
        self.require_same_scale("subtraction", other)?;
        checked_result(self.digits.checked_sub(other.digits), self.scale)
    }

    /// Multiplies two decimals exactly: the product scale is the sum of the
    /// operand scales and the product digits are the exact integer product,
    /// rejecting scale sums above [`MAX_SCALE`] and any result past
    /// [`MAX_PRECISION`] digits (never a wrap, following the
    /// refuse-never-round law).
    pub fn checked_mul(self, other: Self) -> DevonResult<Self> {
        let scale = self.scale + other.scale;
        if scale > MAX_SCALE {
            return Err(invalid_argument(format!(
                "decimal product scale {scale} exceeds the maximum {MAX_SCALE}"
            )));
        }
        checked_result(self.digits.checked_mul(other.digits), scale)
    }

    /// Promotes an `i64` to a decimal at `scale` exactly: the unscaled digits
    /// are `value × 10^scale`, checked so the promotion never rounds and
    /// never wraps. An `i64` always fits in `Decimal(19, 0)`, so only the
    /// 10^scale step can fail.
    pub fn from_i64_scaled(value: i64, scale: u8) -> DevonResult<Self> {
        if scale > MAX_SCALE {
            return Err(invalid_argument(format!(
                "decimal scale {scale} exceeds the maximum {MAX_SCALE}"
            )));
        }
        let factor = i128::try_from(power_of_ten(scale)).map_err(|_| {
            invalid_argument("decimal Int64 promotion factor exceeds i128".to_owned())
        })?;
        checked_result(i128::from(value).checked_mul(factor), scale)
    }

    fn require_same_scale(self, operator: &str, other: Self) -> DevonResult<()> {
        if self.scale == other.scale {
            return Ok(());
        }
        Err(invalid_argument(format!(
            "decimal {operator} requires equal scales; got {} and {}",
            self.scale, other.scale
        )))
    }

    /// Pads a decimal with trailing zero digits to `target_scale`, rejecting
    /// scale reduction, checked overflow, and values outside `precision`.
    pub fn pad_to_scale(self, target_scale: u8, precision: u8) -> DevonResult<Self> {
        if target_scale < self.scale {
            return Err(invalid_argument(format!(
                "cannot pad decimal scale {} down to {target_scale}",
                self.scale
            )));
        }
        if target_scale > MAX_SCALE {
            return Err(invalid_argument(format!(
                "decimal scale {target_scale} exceeds the maximum {MAX_SCALE}"
            )));
        }
        let factor = i128::try_from(power_of_ten(target_scale - self.scale)).map_err(|_| {
            invalid_argument("decimal scale padding factor exceeds i128".to_owned())
        })?;
        let digits = self
            .digits
            .checked_mul(factor)
            .ok_or_else(|| invalid_argument("decimal scale padding overflowed i128".to_owned()))?;
        let result = Self::new(digits, target_scale)?;
        if !result.fits(precision, target_scale) {
            return Err(invalid_argument(format!(
                "decimal value {result} does not fit Decimal({precision}, {target_scale})"
            )));
        }
        Ok(result)
    }
}

/// Builds a decimal from a checked integer operation, rejecting i128
/// overflow and any result with more than [`MAX_PRECISION`] digits.
fn checked_result(digits: Option<i128>, scale: u8) -> DevonResult<Decimal128> {
    let digits = digits
        .ok_or_else(|| invalid_argument("decimal arithmetic result overflowed i128".to_owned()))?;
    let result = Decimal128::new(digits, scale)?;
    if result.precision() > MAX_PRECISION {
        return Err(invalid_argument(format!(
            "decimal arithmetic result {result} exceeds {MAX_PRECISION} digits"
        )));
    }
    Ok(result)
}

fn power_of_ten(exponent: u8) -> u128 {
    (0..exponent).fold(1, |factor, _| factor * 10)
}

fn round_magnitude(quotient: u128, remainder: u128, divisor: u128) -> DevonResult<u128> {
    if remainder >= divisor / 2 + divisor % 2 {
        quotient.checked_add(1).ok_or_else(|| {
            invalid_argument("decimal midpoint-away rounding overflowed u128".to_owned())
        })
    } else {
        Ok(quotient)
    }
}

fn signed_magnitude(magnitude: u128, negative: bool) -> DevonResult<i128> {
    if magnitude == 0 {
        return Ok(0);
    }
    if negative && magnitude == i128::MIN.unsigned_abs() {
        return Ok(i128::MIN);
    }
    let digits = i128::try_from(magnitude)
        .map_err(|_| invalid_argument("decimal result exceeds i128".to_owned()))?;
    if negative {
        digits
            .checked_neg()
            .ok_or_else(|| invalid_argument("decimal result exceeds i128".to_owned()))
    } else {
        Ok(digits)
    }
}

fn rounded_ratio_magnitude(numerator: u128, denominator: u128, exponent: i16) -> DevonResult<u128> {
    if exponent >= 0 {
        let shifts = u8::try_from(exponent).map_err(|_| {
            invalid_argument("decimal division scale exponent is out of range".to_owned())
        })?;
        let (quotient, remainder) = scaled_long_division(numerator, denominator, shifts)?;
        round_magnitude(quotient, remainder, denominator)
    } else {
        let shifts = u8::try_from(-exponent).map_err(|_| {
            invalid_argument("decimal division scale exponent is out of range".to_owned())
        })?;
        rounded_with_larger_denominator(numerator, denominator, shifts)
    }
}

fn scaled_long_division(
    numerator: u128,
    denominator: u128,
    shifts: u8,
) -> DevonResult<(u128, u128)> {
    let mut quotient = numerator / denominator;
    let mut remainder = numerator % denominator;
    for _ in 0..shifts {
        let (digit, next_remainder) = next_decimal_digit(remainder, denominator)?;
        quotient = quotient
            .checked_mul(10)
            .and_then(|value| value.checked_add(digit))
            .ok_or_else(|| invalid_argument("exact decimal quotient overflowed u128".to_owned()))?;
        remainder = next_remainder;
    }
    Ok((quotient, remainder))
}

fn next_decimal_digit(remainder: u128, denominator: u128) -> DevonResult<(u128, u128)> {
    let mut digit = 0;
    let mut next_remainder = 0u128;
    for _ in 0..10 {
        let sum = next_remainder.checked_add(remainder).ok_or_else(|| {
            invalid_argument("exact decimal remainder overflowed u128".to_owned())
        })?;
        if sum >= denominator {
            digit += 1;
            next_remainder = sum - denominator;
        } else {
            next_remainder = sum;
        }
    }
    Ok((digit, next_remainder))
}

fn rounded_with_larger_denominator(
    numerator: u128,
    denominator: u128,
    shifts: u8,
) -> DevonResult<u128> {
    let factor = power_of_ten(shifts);
    let base_quotient = numerator / denominator;
    let base_remainder = numerator % denominator;
    let quotient = base_quotient / factor;
    let discarded = base_quotient % factor;
    let twice_discarded = discarded.checked_mul(2).ok_or_else(|| {
        invalid_argument("exact decimal quotient remainder overflowed u128".to_owned())
    })?;
    let rounds_up = twice_discarded >= factor
        || factor - twice_discarded == 1 && base_remainder >= denominator / 2 + denominator % 2;
    if rounds_up {
        quotient.checked_add(1).ok_or_else(|| {
            invalid_argument("decimal midpoint-away rounding overflowed u128".to_owned())
        })
    } else {
        Ok(quotient)
    }
}

impl fmt::Display for Decimal128 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.scale == 0 {
            return write!(f, "{}", self.digits);
        }
        let sign = if self.digits < 0 { "-" } else { "" };
        let magnitude = self.digits.unsigned_abs().to_string();
        let scale = self.scale as usize;
        if magnitude.len() > scale {
            let (integral, fractional) = magnitude.split_at(magnitude.len() - scale);
            write!(f, "{sign}{integral}.{fractional}")
        } else {
            write!(f, "{sign}0.{magnitude:0>scale$}")
        }
    }
}

impl FromStr for Decimal128 {
    type Err = DevonError;

    /// Parses the canonical decimal string form: optional sign, digits,
    /// optional `.` + digits. No exponents, no floats involved anywhere.
    fn from_str(text: &str) -> DevonResult<Self> {
        let bad = || invalid_argument(format!("`{text}` is not a decimal literal"));
        let (sign, body) = match text.strip_prefix('-') {
            Some(rest) => (-1i128, rest),
            None => (1i128, text.strip_prefix('+').unwrap_or(text)),
        };
        let (integral, fractional) = match body.split_once('.') {
            Some((integral, fractional)) => (integral, Some(fractional)),
            None => (body, None),
        };
        if integral.is_empty() || fractional.is_some_and(str::is_empty) {
            return Err(invalid_argument(format!(
                "`{text}` is not a decimal literal: the grammar is \
                 decimal(\"<sign?digits[.digits]>\") — integral digits are \
                 required and a dot must have fractional digits"
            )));
        }
        if !integral.bytes().all(|b| b.is_ascii_digit())
            || !fractional
                .unwrap_or("0")
                .bytes()
                .all(|b| b.is_ascii_digit())
        {
            return Err(bad());
        }
        let scale = u8::try_from(fractional.unwrap_or("").len()).map_err(|_| {
            invalid_argument(format!(
                "decimal literal `{text}` has more than {MAX_SCALE} fractional digits"
            ))
        })?;
        if scale > MAX_SCALE {
            return Err(invalid_argument(format!(
                "decimal literal `{text}` has more than {MAX_SCALE} fractional digits"
            )));
        }
        let mut digits: i128 = 0;
        for b in integral.bytes().chain(fractional.unwrap_or("").bytes()) {
            digits = digits
                .checked_mul(10)
                .and_then(|d| d.checked_add(i128::from(b - b'0')))
                .ok_or_else(|| {
                    invalid_argument(format!("decimal literal `{text}` overflows 38 digits"))
                })?;
        }
        Self::new(sign * digits, scale)
    }
}

impl Serialize for Decimal128 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Decimal128 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(|e: DevonError| D::Error::custom(e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_display_roundtrip() {
        for text in ["0", "1", "-1", "123.45", "-0.05", "0.000001", "42.0"] {
            let value: Decimal128 = text.parse().expect(text);
            assert_eq!(value.to_string(), text, "canonical form must roundtrip");
        }
    }

    #[test]
    fn plus_sign_normalizes_away() {
        let value: Decimal128 = "+7.5".parse().expect("+7.5");
        assert_eq!(value.to_string(), "7.5");
    }

    #[test]
    fn literal_grammar_requires_integral_and_fractional_digits() {
        for text in ["1.", ".5", "-.5"] {
            let error = text
                .parse::<Decimal128>()
                .expect_err("dangling decimal point must be rejected");
            assert!(
                error
                    .to_string()
                    .contains("decimal(\"<sign?digits[.digits]>\")"),
                "error must name the pinned grammar: {error}"
            );
        }
    }

    #[test]
    fn literal_grammar_accepts_signs_and_significant_scale() {
        assert_eq!("+1".parse::<Decimal128>().unwrap().to_string(), "1");
        assert_eq!("1.50".parse::<Decimal128>().unwrap().to_string(), "1.50");
        assert_eq!("1.50".parse::<Decimal128>().unwrap().scale(), 2);
    }

    #[test]
    fn rejects_non_decimals() {
        for text in ["", ".", "-", "1e5", "1.2.3", "abc", "1,5", "0x1f"] {
            assert!(text.parse::<Decimal128>().is_err(), "`{text}` must fail");
        }
    }

    #[test]
    fn overflow_is_an_error_not_a_wrap() {
        let over = "9".repeat(39);
        assert!(over.parse::<Decimal128>().is_err());
    }

    #[test]
    fn precision_and_fits() {
        let value: Decimal128 = "123.45".parse().expect("123.45");
        assert_eq!(value.precision(), 5);
        assert!(value.fits(5, 2));
        assert!(value.fits(38, 2));
        assert!(!value.fits(4, 2), "5 digits must not fit precision 4");
        assert!(!value.fits(10, 3), "scale must match exactly");
    }

    #[test]
    fn serde_is_the_string_form() {
        let value: Decimal128 = "-19.99".parse().expect("-19.99");
        let json = serde_json::to_string(&value).expect("serialize");
        assert_eq!(json, r#""-19.99""#);
        let back: Decimal128 = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, value);
        assert!(
            serde_json::from_str::<Decimal128>("19.99").is_err(),
            "JSON floats are refused — exactness law"
        );
    }

    #[test]
    fn round_to_places_is_midpoint_away_and_pads_back_to_scale() {
        for (digits, expected) in [
            (1249, "12.00"),
            (1250, "13.00"),
            (-1249, "-12.00"),
            (-1250, "-13.00"),
        ] {
            let value = Decimal128::new(digits, 2).unwrap();
            assert_eq!(value.round_to_places(0, 6).unwrap().to_string(), expected);
        }

        let one_place = Decimal128::new(125, 2).unwrap();
        assert_eq!(one_place.round_to_places(1, 4).unwrap().to_string(), "1.30");
    }

    #[test]
    fn round_to_places_rejects_precision_carry_and_invalid_places() {
        let value = Decimal128::new(999, 2).unwrap();
        assert!(value.round_to_places(0, 3).is_err());
        assert!(value.round_to_places(3, 4).is_err());
        assert_eq!(value.round_to_places(0, 4).unwrap().to_string(), "10.00");
    }

    #[test]
    fn checked_add_sub_same_scale_are_exact_and_checked() {
        let left = Decimal128::new(99999, 2).unwrap();
        let one_cent = Decimal128::new(1, 2).unwrap();
        assert_eq!(
            left.checked_add_same_scale(one_cent).unwrap().to_string(),
            "1000.00"
        );
        assert_eq!(
            one_cent.checked_sub_same_scale(left).unwrap().to_string(),
            "-999.98"
        );

        let other_scale = Decimal128::new(1, 3).unwrap();
        assert!(left.checked_add_same_scale(other_scale).is_err());
        assert!(left.checked_sub_same_scale(other_scale).is_err());
    }

    #[test]
    fn checked_add_sub_refuse_results_past_38_digits() {
        let max_38 = Decimal128::new(10i128.pow(38) - 1, 2).unwrap();
        let one = Decimal128::new(1, 2).unwrap();
        let error = max_38.checked_add_same_scale(one).unwrap_err();
        assert!(
            error.to_string().contains("exceeds 38 digits"),
            "39-digit sums are refused, never wrapped: {error}"
        );
        assert!(max_38.checked_add_same_scale(max_38).is_err());
        assert!(
            Decimal128::new(-(10i128.pow(38)), 2)
                .unwrap()
                .checked_sub_same_scale(max_38)
                .is_err()
        );
    }

    #[test]
    fn checked_mul_adds_scales_and_refuses_overflow() {
        let price = Decimal128::new(123, 2).unwrap();
        let qty = Decimal128::new(20, 1).unwrap();
        let product = price.checked_mul(qty).unwrap();
        assert_eq!(product.to_string(), "2.460");
        assert_eq!(product.scale(), 3);

        let big = Decimal128::new(10i128.pow(19), 2).unwrap();
        assert!(big.checked_mul(big).is_err(), "10^38 digits are refused");

        let scaley = Decimal128::new(1, 30).unwrap();
        assert!(
            scaley.checked_mul(Decimal128::new(1, 9).unwrap()).is_err(),
            "scale sums past 38 are refused"
        );
    }

    #[test]
    fn from_i64_scaled_promotes_exactly_and_checked() {
        assert_eq!(
            Decimal128::from_i64_scaled(-42, 0).unwrap().to_string(),
            "-42"
        );
        let scaled = Decimal128::from_i64_scaled(150, 2).unwrap();
        assert_eq!(scaled.to_string(), "150.00");
        assert_eq!(scaled.scale(), 2);

        let max_at_19 = Decimal128::from_i64_scaled(i64::MAX, 19).unwrap();
        assert_eq!(max_at_19.precision(), 38);
        assert!(
            Decimal128::from_i64_scaled(i64::MAX, 20).is_err(),
            "19 + 20 digits cannot be exact in 38"
        );
        assert!(Decimal128::from_i64_scaled(1, 39).is_err());
    }

    #[test]
    fn pad_to_scale_is_checked_and_never_reduces_scale() {
        let value = Decimal128::new(12, 1).unwrap();
        assert_eq!(value.pad_to_scale(3, 4).unwrap().to_string(), "1.200");
        assert!(value.pad_to_scale(0, 4).is_err());
        assert!(value.pad_to_scale(3, 3).is_err());

        let large = Decimal128::new(i128::MAX, 0).unwrap();
        assert!(large.pad_to_scale(1, MAX_PRECISION).is_err());
    }

    #[test]
    fn round_div_uses_one_exact_rational_rounding_step() {
        let numerator = Decimal128::new(100, 2).unwrap();
        for (denominator, expected) in [(800, "0.13"), (-800, "-0.13")] {
            let denominator = Decimal128::new(denominator, 2).unwrap();
            assert_eq!(
                numerator
                    .round_div_to_places(denominator, 2, 4)
                    .unwrap()
                    .to_string(),
                expected
            );
        }

        let different_scale = Decimal128::new(2, 1).unwrap();
        assert_eq!(
            numerator
                .round_div_to_places(different_scale, 1, 4)
                .unwrap()
                .to_string(),
            "5.00"
        );
    }

    #[test]
    fn exact_division_avoids_scaled_intermediate_overflow() {
        let tiny = Decimal128::new(4, 38).unwrap();
        let almost_one = Decimal128::new(10i128.pow(38) - 1, 38).unwrap();
        assert_eq!(tiny.round_div_to_places(almost_one, 38, 38).unwrap(), tiny);

        let four_tenths = Decimal128::new(4 * 10i128.pow(37), 38).unwrap();
        let four = Decimal128::new(4, 0).unwrap();
        assert_eq!(
            four_tenths
                .round_div_to_places(four, 0, 38)
                .unwrap()
                .to_string(),
            "0.00000000000000000000000000000000000000"
        );
    }

    #[test]
    fn round_div_rejects_zero_overflow_and_unrepresentable_results() {
        let one = Decimal128::new(100, 2).unwrap();
        let zero = Decimal128::new(0, 2).unwrap();
        assert!(one.round_div_to_places(zero, 1, 4).is_err());

        let tiny_denominator = Decimal128::new(1, 2).unwrap();
        assert!(
            Decimal128::new(i128::MAX, 2)
                .unwrap()
                .round_div_to_places(tiny_denominator, 2, 38)
                .is_err()
        );

        let ten = Decimal128::new(1000, 2).unwrap();
        assert!(ten.round_div_to_places(one, 0, 3).is_err());
    }
}
