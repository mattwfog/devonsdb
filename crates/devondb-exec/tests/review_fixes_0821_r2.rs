//! Regression pins for the remaining verified 2026-08-21 executor findings:
//! spill rows own their envelope and value codec, correlated outer-binding
//! keys obey the plan-wide ASCII fold, and projection arity is constructor
//! validation rather than a per-chunk check.

use std::{
    collections::{HashMap, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use devondb_exec::{
    chunk::{Chunk, ChunkBuilder},
    operators::{Project, Sort, SpillConfig},
    source::{ChunkSource, OuterBindings, ScalarSubqueryExecutor, ScalarSubqueryResult},
};
use devondb_plan::{
    expr::Expr,
    ops::{Operator, ProjectionItem, SortOrder},
};
use devondb_storage::budget::MemoryBudget;
use devondb_types::{
    DevonError, DevonResult, GeoPoint, decimal::Decimal128, logical_type::LogicalType, value::Value,
};

const SPILL_ROW_MAGIC: &[u8; 8] = b"DVNSPROW";

struct VecSource {
    chunks: VecDeque<Chunk>,
}

impl VecSource {
    fn new(chunks: Vec<Chunk>) -> Self {
        Self {
            chunks: chunks.into(),
        }
    }
}

impl ChunkSource for VecSource {
    fn next_chunk(&mut self) -> DevonResult<Option<Chunk>> {
        Ok(self.chunks.pop_front())
    }
}

fn chunk(types: Vec<LogicalType>, rows: Vec<Vec<Value>>) -> Chunk {
    let mut builder = ChunkBuilder::new(types);
    for row in rows {
        builder.push_row(row).unwrap();
    }
    builder.finish()
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "devondb-review-fixes-0821-r2-{label}-{}-{nanos}",
            std::process::id()
        )))
    }

    fn config(&self, limit: usize) -> SpillConfig {
        SpillConfig {
            budget: Arc::new(MemoryBudget::new(limit)),
            tmp_dir: self.0.clone(),
        }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn special_value_types() -> Vec<LogicalType> {
    vec![
        LogicalType::Int64,
        LogicalType::Bool,
        LogicalType::Bool,
        LogicalType::Int64,
        LogicalType::Float64,
        LogicalType::Float64,
        LogicalType::Float64,
        LogicalType::String,
        LogicalType::Vector { dim: 4 },
        LogicalType::Timestamp,
        LogicalType::Bytes,
        LogicalType::Decimal {
            precision: 12,
            scale: 3,
        },
        LogicalType::Json,
        LogicalType::GeoPoint,
    ]
}

fn special_value_row(id: i64) -> Vec<Value> {
    vec![
        Value::Int64(id),
        Value::Null,
        Value::Bool(true),
        Value::Int64(i64::MIN),
        Value::Float64(f64::from_bits(0x7ff8_0000_0000_0042)),
        Value::Float64(f64::INFINITY),
        Value::Float64(f64::NEG_INFINITY),
        Value::String("quote: \" slash: \\ nul: \0 newline:\n".into()),
        Value::Vector(vec![0.0, -0.0, 1.25, -9.5]),
        Value::Timestamp(-1_234_567_890),
        Value::Bytes(vec![0, 1, 2, 0xfe, 0xff]),
        Value::Decimal(Decimal128::new(-1_234_567, 3).unwrap()),
        Value::Json(r#"{"control":"\u0000\n","nested":[true,null,3]}"#.into()),
        Value::GeoPoint(GeoPoint::from_canonical(45.5, -122.625).unwrap()),
    ]
}

fn null_value_row(id: i64) -> Vec<Value> {
    let mut row = vec![Value::Int64(id)];
    row.resize(special_value_types().len(), Value::Null);
    row
}

fn assert_value_bits_eq(actual: &Value, expected: &Value) {
    match (actual, expected) {
        (Value::Float64(actual), Value::Float64(expected)) => {
            assert_eq!(actual.to_bits(), expected.to_bits());
        }
        (Value::Vector(actual), Value::Vector(expected)) => {
            assert_eq!(actual.len(), expected.len());
            for (actual, expected) in actual.iter().zip(expected) {
                assert_eq!(actual.to_bits(), expected.to_bits());
            }
        }
        _ => assert_eq!(actual, expected),
    }
}

/// The spill codec must preserve every runtime value, including Float64
/// spellings JSON cannot carry, without consulting the WAL statement codec.
#[test]
fn spill_row_owns_envelope_and_round_trips_every_value_variant() {
    let directory = TempDir::new("round-trip");
    let expected = special_value_row(2);
    let input = chunk(
        special_value_types(),
        vec![expected.clone(), null_value_row(3), null_value_row(1)],
    );
    let mut sort = Sort::new(
        Box::new(VecSource::new(vec![input])),
        vec![(Expr::Col("r.id".into()), SortOrder::Asc)],
        HashMap::from([("r.id".into(), 0)]),
        directory.config(800),
    );

    let output = sort.next_chunk().unwrap().unwrap();
    assert_eq!(output.row_count(), 3);
    for (actual, expected) in output.rows().nth(1).unwrap().iter().zip(&expected) {
        assert_value_bits_eq(actual, expected);
    }
    assert!(sort.next_chunk().unwrap().is_none());
}

struct CorruptHeaderAfterFirstChunk {
    chunk: Option<Chunk>,
    spill_dir: PathBuf,
}

impl ChunkSource for CorruptHeaderAfterFirstChunk {
    fn next_chunk(&mut self) -> DevonResult<Option<Chunk>> {
        if let Some(chunk) = self.chunk.take() {
            return Ok(Some(chunk));
        }
        corrupt_first_spill_header(&self.spill_dir);
        Ok(None)
    }
}

fn corrupt_first_spill_header(directory: &Path) {
    let path = fs::read_dir(directory)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let mut bytes = fs::read(&path).unwrap();
    assert_eq!(&bytes[4..12], SPILL_ROW_MAGIC);
    bytes[4] ^= 0xff;
    fs::write(path, bytes).unwrap();
}

/// A malformed spill-owned header is corruption, not a panic or a JSON/WAL
/// decoding accident.
#[test]
fn corrupted_spill_row_header_returns_corrupt() {
    let directory = TempDir::new("corrupt-header");
    let input = chunk(
        vec![LogicalType::Int64],
        vec![vec![Value::Int64(2)], vec![Value::Int64(1)]],
    );
    let source = CorruptHeaderAfterFirstChunk {
        chunk: Some(input),
        spill_dir: directory.0.clone(),
    };
    let mut sort = Sort::new(
        Box::new(source),
        vec![(Expr::Col("r.id".into()), SortOrder::Asc)],
        HashMap::from([("r.id".into(), 0)]),
        directory.config(96),
    );

    let DevonError::Corrupt { context } = sort.next_chunk().unwrap_err() else {
        panic!("expected a Corrupt spill-header error");
    };
    assert!(context.contains("spill file"), "{context}");
    assert!(context.contains("header magic"), "{context}");
}

struct FoldedOuterEcho;

impl ScalarSubqueryExecutor for FoldedOuterEcho {
    fn memory_budget(&self) -> Arc<MemoryBudget> {
        Arc::new(MemoryBudget::unlimited())
    }

    fn execute(
        &self,
        _plan: &Operator,
        outer: &OuterBindings,
    ) -> DevonResult<ScalarSubqueryResult> {
        let value = outer
            .get("p.name")
            .cloned()
            .ok_or_else(|| DevonError::Corrupt {
                context: "outer binding `P.Name` was not stored as folded key `p.name`".into(),
            })?;
        Ok(ScalarSubqueryResult {
            output_type: LogicalType::String,
            values: vec![value],
        })
    }
}

/// Mixed-case correlated references and their outer chunk keys meet at the
/// same ASCII-folded `binding.column` spelling.
#[test]
fn correlated_scalar_reference_uses_folded_outer_binding_key() {
    let scalar_plan = Operator::Project {
        exprs: vec![ProjectionItem {
            expr: Expr::Col("P.Name".into()),
            alias: "name".into(),
        }],
        input: Box::new(Operator::ScanNodes {
            table: "Inner".into(),
            binding: "i".into(),
        }),
    };
    let input = chunk(
        vec![LogicalType::String],
        vec![vec![Value::String("Ada".into())]],
    );
    let mut project = Project::new(
        Box::new(VecSource::new(vec![input])),
        vec![(
            Expr::Scalar {
                plan: Box::new(scalar_plan),
            },
            "outer_name".into(),
        )],
        HashMap::from([("P.Name".into(), 0)]),
        vec![LogicalType::String],
    )
    .unwrap()
    .with_scalar_executor(Arc::new(FoldedOuterEcho));

    let output = project.next_chunk().unwrap().unwrap();
    assert_eq!(output.value(0, 0), Some(Value::String("Ada".into())));
}

/// Projection metadata disagreement is rejected before any upstream chunk is
/// pulled, so an invalid projection cannot exist.
#[test]
fn project_rejects_mismatched_arity_at_construction() {
    let result = Project::new(
        Box::new(VecSource::new(Vec::new())),
        vec![(Expr::Lit(Value::Int64(1)), "one".into())],
        HashMap::new(),
        Vec::new(),
    );

    let Err(DevonError::InvalidArgument { context }) = result else {
        panic!("expected InvalidArgument from Project::new");
    };
    assert!(
        context.contains("1 expressions but 0 output types"),
        "{context}"
    );
}
