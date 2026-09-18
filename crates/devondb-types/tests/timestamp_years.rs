use devondb_types::value::Value;

const MICROS_PER_DAY: i64 = 86_400_000_000;

#[test]
fn ordinary_year_timestamp_spellings_stay_byte_identical() {
    assert_eq!(
        Value::Timestamp(0).to_string(),
        r#"timestamp("1970-01-01T00:00:00Z")"#
    );
    assert_eq!(
        Value::Timestamp(1_723_161_600_123_456).to_string(),
        r#"timestamp("2024-08-09T00:00:00.123456Z")"#
    );
    assert_eq!(
        Value::Timestamp(-1_000_000).to_string(),
        r#"timestamp("1969-12-31T23:59:59Z")"#
    );
}

#[test]
fn out_of_range_years_use_signed_six_digit_form() {
    let cases = [
        (
            micros_at_year(-1),
            r#"timestamp("-000001-01-01T00:00:00Z")"#,
        ),
        (micros_at_year(0), r#"timestamp("+000000-01-01T00:00:00Z")"#),
        (
            micros_at_year(10_000),
            r#"timestamp("+010000-01-01T00:00:00Z")"#,
        ),
        (i64::MIN, r#"timestamp("-290308-12-21T19:59:05.224192Z")"#),
        (i64::MAX, r#"timestamp("+294247-01-10T04:00:54.775807Z")"#),
    ];

    for (micros, expected) in cases {
        assert_eq!(Value::Timestamp(micros).to_string(), expected);
    }
}

fn micros_at_year(year: i64) -> i64 {
    days_from_civil(year, 1, 1) * MICROS_PER_DAY
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let adjusted_year = year - i64::from(month <= 2);
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year.rem_euclid(400);
    let shifted_month = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}
