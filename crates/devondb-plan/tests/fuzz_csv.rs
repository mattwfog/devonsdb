use std::io::Write;

use devondb_plan::csv::CsvReader;
use devondb_types::{
    DevonError,
    logical_type::LogicalType,
    schema::{Column, NodeTableSchema},
    value::Value,
};
use proptest::{
    collection,
    prelude::*,
    test_runner::{RngSeed, TestCaseError},
};
use tempfile::NamedTempFile;

const PROPTEST_SEED: u64 = 0x4456_4353_565f_465a;
const MAX_INPUT_BYTES: usize = 4096;
const HEADER: &[u8] = b"id,score,flag,name\n";

fn arbitrary_csv_body() -> BoxedStrategy<Vec<u8>> {
    let max_body_bytes = MAX_INPUT_BYTES - HEADER.len();
    let arbitrary_bytes = collection::vec(any::<u8>(), 0..=max_body_bytes);
    let long_runs = (
        prop::sample::select(vec![b'"', b',', b'\r', b'\n', b'\0']),
        0..=max_body_bytes,
    )
        .prop_map(|(byte, length)| vec![byte; length]);
    let truncated_prefix = b"1,0.0,true,\"";
    let truncated_quotes = collection::vec(
        prop::sample::select(vec![b'a', b',', b'\r', b'\n', b'\0', 0xff]),
        0..=max_body_bytes - truncated_prefix.len(),
    )
    .prop_map(move |tail| {
        let mut input = truncated_prefix.to_vec();
        input.extend(tail);
        input
    });
    let malformed_rows = prop::sample::select(vec![
        b"1,0.0,true".to_vec(),
        b"1,not-a-float,true,Ada\n".to_vec(),
        b"1,0.0,true,Ada\r".to_vec(),
        b"1,0.0,true,A\xffda\n".to_vec(),
    ]);

    prop_oneof![
        8 => arbitrary_bytes,
        3 => long_runs,
        3 => truncated_quotes,
        2 => malformed_rows,
    ]
    .boxed()
}

fn arbitrary_csv_input() -> BoxedStrategy<Vec<u8>> {
    let arbitrary_bytes = collection::vec(any::<u8>(), 0..=MAX_INPUT_BYTES);
    let long_runs = (
        prop::sample::select(vec![b'"', b',', b'\r', b'\n', b'\0']),
        0..=MAX_INPUT_BYTES,
    )
        .prop_map(|(byte, length)| vec![byte; length]);
    let valid_header_then_body = arbitrary_csv_body().prop_map(|body| {
        let mut input = HEADER.to_vec();
        input.extend(body);
        input
    });

    prop_oneof![
        8 => arbitrary_bytes,
        3 => long_runs,
        4 => valid_header_then_body,
    ]
    .boxed()
}

#[derive(Clone, Debug)]
struct GeneratedRow {
    id_literal: String,
    score_literal: String,
    flag_literal: String,
    name: String,
}

fn integer_literal() -> BoxedStrategy<String> {
    any::<i64>().prop_map(|value| value.to_string()).boxed()
}

fn float_literal() -> BoxedStrategy<String> {
    let decimal = (any::<bool>(), 0_u32..=1_000_000, 0_u32..=999_999).prop_map(
        |(negative, whole, fraction)| {
            let sign = if negative { "-" } else { "" };
            format!("{sign}{whole}.{fraction:06}")
        },
    );
    let exponent = (any::<bool>(), 0_u32..=100_000, 0_u32..=999, -20_i32..=20).prop_map(
        |(negative, whole, fraction, exponent)| {
            let sign = if negative { "-" } else { "" };
            format!("{sign}{whole}.{fraction:03}e{exponent}")
        },
    );

    prop_oneof![4 => decimal, 2 => exponent].boxed()
}

fn boolean_literal() -> BoxedStrategy<String> {
    any::<bool>().prop_map(|value| value.to_string()).boxed()
}

fn csv_name() -> BoxedStrategy<String> {
    let arbitrary_unicode = collection::vec(any::<char>(), 0..=32)
        .prop_map(|characters| characters.into_iter().collect());
    let csv_specials = collection::vec(
        prop_oneof![
            6 => any::<char>(),
            2 => Just(','),
            2 => Just('\n'),
            2 => Just('"'),
            1 => Just('\r'),
        ],
        0..=32,
    )
    .prop_map(|characters| characters.into_iter().collect());
    let targeted = prop::sample::select(
        [
            "comma,name",
            "line\nbreak",
            "quote \"inside\"",
            "comma, quote \"and\"\nnewline",
            "\0",
            "",
        ]
        .map(str::to_owned)
        .to_vec(),
    );

    prop_oneof![5 => arbitrary_unicode, 4 => csv_specials, 2 => targeted].boxed()
}

fn generated_row() -> BoxedStrategy<GeneratedRow> {
    (
        integer_literal(),
        float_literal(),
        boolean_literal(),
        csv_name(),
    )
        .prop_map(
            |(id_literal, score_literal, flag_literal, name)| GeneratedRow {
                id_literal,
                score_literal,
                flag_literal,
                name,
            },
        )
        .boxed()
}

fn generated_rows() -> BoxedStrategy<Vec<GeneratedRow>> {
    collection::vec(generated_row(), 0..=12).boxed()
}

fn malformed_csv_case() -> BoxedStrategy<(String, Vec<u8>, usize)> {
    prop::sample::select(vec![
        (
            "unterminated quote".to_owned(),
            b"id,score,flag,name\n1,1.0,true,\"unterminated\nstill open".to_vec(),
            2,
        ),
        (
            "wrong field count".to_owned(),
            b"id,score,flag,name\n1,1.0,true,Ada\n2,2.0,false\n".to_vec(),
            3,
        ),
        (
            "bad literal in row 2".to_owned(),
            b"id,score,flag,name\n1,not-a-float,true,Ada\n".to_vec(),
            2,
        ),
    ])
    .boxed()
}

fn schema() -> NodeTableSchema {
    NodeTableSchema::new(
        "People".into(),
        vec![
            column("id", LogicalType::Int64, true),
            column("score", LogicalType::Float64, false),
            column("flag", LogicalType::Bool, false),
            column("name", LogicalType::String, false),
        ],
    )
    .unwrap()
}

fn column(name: &str, ty: LogicalType, primary_key: bool) -> Column {
    Column {
        name: name.into(),
        ty,
        primary_key,
    }
}

fn csv_file(contents: &[u8]) -> NamedTempFile {
    let mut file = NamedTempFile::new().unwrap();
    file.write_all(contents).unwrap();
    file.flush().unwrap();
    file
}

fn open(file: &NamedTempFile, schema: &NodeTableSchema) -> Result<CsvReader, DevonError> {
    CsvReader::open(file.path().to_str().unwrap(), schema)
}

fn well_formed_csv(rows: &[GeneratedRow]) -> String {
    let mut csv = String::from_utf8(HEADER.to_vec()).unwrap();
    for row in rows {
        csv.push_str(&row.id_literal);
        csv.push(',');
        csv.push_str(&row.score_literal);
        csv.push(',');
        csv.push_str(&row.flag_literal);
        csv.push(',');
        push_quoted_field(&mut csv, &row.name);
        csv.push('\n');
    }
    csv
}

fn push_quoted_field(csv: &mut String, field: &str) {
    csv.push('"');
    for character in field.chars() {
        if character == '"' {
            csv.push_str("\"\"");
        } else {
            csv.push(character);
        }
    }
    csv.push('"');
}

fn expected_rows(rows: &[GeneratedRow]) -> Vec<Vec<Value>> {
    rows.iter()
        .map(|row| {
            vec![
                Value::Int64(row.id_literal.parse::<i64>().unwrap()),
                Value::Float64(row.score_literal.parse::<f64>().unwrap()),
                Value::Bool(row.flag_literal.parse::<bool>().unwrap()),
                Value::String(row.name.clone()),
            ]
        })
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 512,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(PROPTEST_SEED),
        ..ProptestConfig::default()
    })]

    #[test]
    fn never_panics_on_arbitrary_bytes(input in arbitrary_csv_input()) {
        prop_assert!(input.len() <= MAX_INPUT_BYTES);
        let file = csv_file(&input);
        if let Ok(reader) = open(&file, &schema()) {
            for result in reader {
                drop(result);
            }
        }
    }

    #[test]
    fn fused_after_first_error(body in arbitrary_csv_body()) {
        let mut input = HEADER.to_vec();
        input.extend(body);
        prop_assert!(input.len() <= MAX_INPUT_BYTES);

        let file = csv_file(&input);
        let mut reader = open(&file, &schema())
            .map_err(|error| TestCaseError::fail(error.to_string()))?;
        while let Some(result) = reader.next() {
            if result.is_err() {
                prop_assert!(
                    reader.next().is_none(),
                    "reader was not fused after input {:?}",
                    input
                );
                break;
            }
        }
    }

    #[test]
    fn round_trip_well_formed(rows in generated_rows()) {
        let csv = well_formed_csv(&rows);
        let file = csv_file(csv.as_bytes());
        let actual = open(&file, &schema())
            .map_err(|error| TestCaseError::fail(error.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| TestCaseError::fail(error.to_string()))?;

        prop_assert_eq!(actual, expected_rows(&rows), "CSV input: {:?}", csv);
    }

    #[test]
    fn error_positions_are_1_based(
        (description, input, expected_line) in malformed_csv_case()
    ) {
        let file = csv_file(&input);
        let mut reader = open(&file, &schema())
            .map_err(|error| TestCaseError::fail(error.to_string()))?;
        let error = reader
            .find_map(Result::err)
            .ok_or_else(|| TestCaseError::fail(format!("{description} was accepted")))?;
        let DevonError::InvalidArgument { context } = error else {
            return Err(TestCaseError::fail(format!(
                "{description} returned a non-InvalidArgument error: {error}"
            )));
        };
        let expected_prefix = format!("CSV line {expected_line}");

        prop_assert!(
            context.starts_with(&expected_prefix),
            "{} reported the wrong position: {}",
            description,
            context
        );
    }
}
