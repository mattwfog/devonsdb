//! Saved timeout regressions and long grounded traversal refusals.
use std::time::{Duration, Instant};

use devondb::{ColumnSummary, NodeTableSummary, RelTableSummary, SchemaSummary};
use devondb_nl::{Compiled, DeterministicCompiler, IntentCompiler, NL_VERSION};

const SAVED: [&[u8]; 3] = [
    b"\x9a\x65\x26\x5d\x7f\xd7\xab\xf8\x35\x12\xe2\xd7\xf3\xd6\x76\xcc\
        \x2d\x61\x6d\x65\x60\x20\x20\x27\x27\x20\xdb\x09\xd2\xab\x30\xe4\
        \x66\x69\x6c\x65\x73\x20\x60\x6e\x61\x6d\x65\x60\x20\x41\xd2\x5e\
        \x4c\xd9\x37\x68\x6f\x20\x01\x20\x64\x6f\x20\x22\x41\x64\x61\x5c\
        \x6e\x4c\x6f\x76\x65\x6c\x61\x63\x65\x22\x68\x65\x20\x70\x72\x6f\
        \x66\x69\x6c\x65\x73\x20\x60\x6e\x61\x6d\x65\x60\x20\x20\x27\x27\
        \x20\x4f\xe2\x80\x99\x4e\x65\x69\x6c\x20\x9e\x32\xa3\xfc\x4a\x41\
        \x02\x37\x35\x0f\x89\x28\xc1\x62\x4d\x13\x50\x30\x30\x30\x30\x30\
        \x30\x30\x30\x68\x65\x20\x70\x72\x6f\x66\x69\x6c\x65\x73\x20\x60\
        \x6e\x61\x6d\x65\x60\x20\x41\x64\x61\x20\x6b\x6e\x6f\x77\x73\x20\
        \x27\x27\x20\x6b\x6e\x6f\x77",
    b"\x51\x52\xad\xc6\xd7\x77\x77\xe8\x8a\xd1\x4a\xd5\x64\x8f\xf7\x05\
        \x5a\x4b\x2d\x30\x20\x77\x68\x6f\x20\x01\x20\x6b\x6e\x6f\x77\x19\
        \x31\xb5\xce\x4a\xed\x17\xe2\x2c\x8f\x77\x68\x6f\x30\x20\x77\x68\
        \x6f\x20\x01\x20\x6b\x6e\x6f\x77\x20\x21\x20\x30\x78\xcf\xec\x6e\
        \x64\x6f\x20\x74\x68\x65\x20\x41\x64\x61\x20\x32\x67\x32\x34\x2d\
        \x30\x32\x2d\x32\x39\x20\x96\x41\x8c\x1b\xc7\x82\xa5\xdd\x5b\xba\
        \x44\x1d\xbd\x3b\xec\x2a\xa7\x92\x7e\xbd\x3b\xec\x2a\xa7\x92\xd1\
        \x4a\xd5\x64\x8f\xf7\x05\x5a\x4b\x2d\x30\x20\x77\x68\x6f\x20\x01\
        \x20\x6b\x6e\x6f\x7e\xe8\x8a\xd1\x4a\xd5\x64\x8f\xf7\x05\x5a\x4b\
        \x2d\x30\x20\x77\x68\x6f\x20\x01\x20\x6b\x6e\x6f\x77\xe8\x8a\xd1\
        \x4a\xd5\x64\x8f\xf7\x05\x5a\x4b\x2d\x30\x20\x77\x68\x6f\x20\x01\
        \x20\x6b\x6e\x6f\x77",
    b"\xb3\xbd\x26\xec\xb7\x40\x64\x61\x20\x77\x68\x6f\x77\x20\x77\x68\
        \x6f\x20\x64\x03\x3b\x5b\x9b\x2c\xa9\x50\xdd\x16\x41\x56\xe5\x14\
        \xa7\xca\x71\x03\x3b\x5b\x9b\x2c\xa9\x50\xdd\x16\x41\x64\x61\x20\
        \x41\x64\x61\x20\x6b\x6e\x6f\x77\x20\x77\x68\x6f\x20\x64\x03\x3b\
        \x5b\x9b\x2c\xa9\x50\xdd\x16\x41\x64\x61\x20\x41\x5f\x21\xd3\xac\
        \x04\xa5\xc2\xd5\x63\x82\x6f\x65\x22\x22\x22\x22\x22\x22\x22\x22\
        \x22\x22\x22\x22\x22\x22\x20\x64\x03\x3b\x5b\x9b\x2c\xa9\x50\xdd\
        \x16\x41",
];

fn column(name: &str, ty: &str, primary_key: bool) -> ColumnSummary {
    ColumnSummary {
        name: name.into(),
        ty: ty.into(),
        primary_key,
    }
}

// Reproduce the exact schema selected by these inputs in the standard NL
// harness, including its header-derived spelling and vector dimension.
fn saved_schema(header: &[u8]) -> SchemaSummary {
    assert_eq!(header[4] % 2, 1, "saved inputs do not use ontology classes");
    let table = ["Person", "Contact", "Profile", "Count"][usize::from(header[0]) % 4];
    let label = ["name", "title", "label", "Count"][usize::from(header[1]) % 4];
    let measure = ["score", "amount", "rating", "Total"][usize::from(header[2]) % 4];
    SchemaSummary {
        node_tables: vec![
            NodeTableSummary {
                name: table.into(),
                columns: vec![
                    column("id", "Int64", true),
                    column(label, "String", false),
                    column("age", "Int64", false),
                    column(measure, "Float64", false),
                    column("occurred", "Int64", false),
                    column("created_at", "Timestamp", false),
                    column(
                        "embedding",
                        &format!("Vector({})", header[3] % 8 + 1),
                        false,
                    ),
                    column("location", "GeoPoint", false),
                ],
            },
            NodeTableSummary {
                name: "Company".into(),
                columns: vec![
                    column("id", "Int64", true),
                    column("name", "String", false),
                    column("valuation", "Float64", false),
                    column("founded_at", "Timestamp", false),
                    column("headquarters", "GeoPoint", false),
                ],
            },
        ],
        rel_tables: vec![
            RelTableSummary {
                name: "Knows".into(),
                from: table.into(),
                to: table.into(),
                columns: vec![column("since", "Timestamp", false)],
            },
            RelTableSummary {
                name: "Works_At".into(),
                from: table.into(),
                to: "Company".into(),
                columns: vec![column("since", "Int64", false)],
            },
        ],
        classes: None,
        pins: Vec::new(),
    }
}

#[test]
fn saved_watchdog_inputs_finish_with_deterministic_refusals() {
    for input in SAVED {
        let schema = saved_schema(&input[..16]);
        let question = String::from_utf8_lossy(&input[16..]);
        let start = Instant::now();
        let first = DeterministicCompiler.compile(&question, &schema);
        assert!(matches!(first, Compiled::NoParse(_)));
        assert_eq!(first, DeterministicCompiler.compile(&question, &schema));
        assert_eq!(
            DeterministicCompiler.compile_statement(&question, &schema),
            DeterministicCompiler.compile_statement(&question, &schema)
        );
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "saved {}-byte watchdog input",
            input.len()
        );
    }
}

#[test]
fn long_valid_paths_refuse_without_truncating_or_changing_hints() {
    let schema = saved_schema(&[0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    for (question, expected) in [
        (
            format!("{}Ada", "knows of ".repeat(256)),
            "knows of knows of Ada",
        ),
        (
            format!("who do the people Ada {}", "knows ".repeat(256)),
            "who do the people Ada knows knows",
        ),
    ] {
        let Compiled::NoParse(report) = DeterministicCompiler.compile(&question, &schema) else {
            panic!("long path compiled");
        };
        assert_eq!(report.unrecognized.len(), 1);
        assert_eq!(
            report.unrecognized[0].suggestion.as_deref(),
            Some("multi-hop traversal has a hard 2-hop cap")
        );
        assert_eq!(report.nearest[0].example, expected);
    }
    assert_eq!(NL_VERSION, 10);
}
