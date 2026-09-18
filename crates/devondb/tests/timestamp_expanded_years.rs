use devondb::text::parser::{Parsed, parse};
use devondb::text::printer::print_statement;
use devondb::{Statement, Value};

const MICROS_PER_DAY: i64 = 86_400_000_000;

#[test]
fn every_timestamp_year_form_prints_and_parses_losslessly() {
    for micros in [
        i64::MIN,
        i64::MAX,
        micros_at_year(-1),
        micros_at_year(0),
        micros_at_year(10_000),
        0,
    ] {
        let literal = Value::Timestamp(micros).to_string();
        let input = format!("insert into Events values ({literal})");
        let Parsed::Statement(envelope) = parse(&input).unwrap() else {
            panic!("timestamp insert parsed as a query");
        };
        assert_eq!(timestamp_value(&envelope.stmt), Value::Timestamp(micros));
        assert_eq!(print_statement(&envelope).unwrap(), input);
    }
}

#[test]
fn expanded_year_syntax_is_canonical() {
    for input in [
        "+1970-01-01T00:00:00Z",
        "-0001-01-01T00:00:00Z",
        "+001970-01-01T00:00:00Z",
        "0000-01-01T00:00:00Z",
        "-000000-01-01T00:00:00Z",
    ] {
        let error = parse(&format!(
            "insert into Events values (timestamp(\"{input}\"))"
        ))
        .expect_err("non-canonical timestamp year must fail")
        .to_string();
        assert!(
            error.contains("timestamp") && error.contains("year"),
            "{input}: {error}"
        );
    }
}

fn timestamp_value(statement: &Statement) -> Value {
    let Statement::InsertNode { rows, .. } = statement else {
        panic!("expected node insert");
    };
    rows[0][0].clone()
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
