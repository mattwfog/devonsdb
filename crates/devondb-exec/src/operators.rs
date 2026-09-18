//! Executor operators over [`crate::source::ChunkSource`].
//!
//! Each operator wraps an upstream source and is itself a `ChunkSource`,
//! so plans become pull pipelines. `Filter` keeps only rows whose
//! predicate is exactly `Bool(true)`. Storage-backed scans arrive with
//! the `Database` facade.

use std::{
    cmp::Ordering,
    collections::{HashMap, VecDeque},
    fs::{self, File, OpenOptions},
    io::{BufReader, BufWriter, ErrorKind, Read, Seek, Write},
    mem::size_of,
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering as AtomicOrdering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use devondb_plan::expr::Expr;
use devondb_plan::ops::{AggregateFunction, Operator, SortOrder};
use devondb_storage::budget::MemoryBudget;
use devondb_types::{
    DevonError, DevonResult,
    decimal::{Decimal128, MAX_PRECISION},
    logical_type::LogicalType,
    schema::fold,
    value::Value,
};

use crate::{
    chunk::{CHUNK_CAPACITY, Chunk, ChunkBuilder},
    column::{Bitmap, Column},
    eval::{classof_column_key, evaluate, scoreof_column_key},
    source::{ChunkSource, OuterBindings, ScalarSubqueryExecutor},
};

/// A pull operator that retains rows whose predicate is exactly `Bool(true)`.
pub struct Filter {
    upstream: Box<dyn ChunkSource>,
    predicate: Expr,
    columns: HashMap<String, usize>,
    scalar: ScalarExpressions,
}

impl Filter {
    /// Creates a filter over `upstream` using the input-column reference map.
    #[must_use]
    pub fn new(
        upstream: Box<dyn ChunkSource>,
        predicate: Expr,
        columns: HashMap<String, usize>,
    ) -> Self {
        Self {
            upstream,
            predicate,
            columns,
            scalar: ScalarExpressions::default(),
        }
    }

    /// Installs the same-snapshot runner used by scalar expressions.
    #[must_use]
    pub fn with_scalar_executor(mut self, executor: Arc<dyn ScalarSubqueryExecutor>) -> Self {
        self.scalar = ScalarExpressions::new(executor);
        self
    }
}

impl ChunkSource for Filter {
    fn next_chunk(&mut self) -> DevonResult<Option<Chunk>> {
        loop {
            let Some(chunk) = self.upstream.next_chunk()? else {
                return Ok(None);
            };
            let predicate = self
                .scalar
                .evaluate(&self.predicate, &chunk, &self.columns)?;
            let selected_rows = selected_rows(&predicate)?;
            if !selected_rows.is_empty() {
                return copy_rows(&chunk, selected_rows).map(Some);
            }
        }
    }
}

/// A pull operator that evaluates expressions into ordered output columns.
pub struct Project {
    upstream: Box<dyn ChunkSource>,
    exprs: Vec<(Expr, String)>,
    columns: HashMap<String, usize>,
    types: Vec<LogicalType>,
    scalar: ScalarExpressions,
}

impl Project {
    /// Creates a projection over `upstream`.
    ///
    /// # Errors
    ///
    /// Returns [`DevonError::InvalidArgument`] when the expression and output
    /// type counts differ.
    pub fn new(
        upstream: Box<dyn ChunkSource>,
        exprs: Vec<(Expr, String)>,
        columns: HashMap<String, usize>,
        types: Vec<LogicalType>,
    ) -> DevonResult<Self> {
        if exprs.len() != types.len() {
            return Err(invalid_argument(format!(
                "projection has {} expressions but {} output types",
                exprs.len(),
                types.len()
            )));
        }
        Ok(Self {
            upstream,
            exprs,
            columns,
            types,
            scalar: ScalarExpressions::default(),
        })
    }

    /// Installs the same-snapshot runner used by scalar expressions.
    #[must_use]
    pub fn with_scalar_executor(mut self, executor: Arc<dyn ScalarSubqueryExecutor>) -> Self {
        self.scalar = ScalarExpressions::new(executor);
        self
    }
}

impl ChunkSource for Project {
    fn next_chunk(&mut self) -> DevonResult<Option<Chunk>> {
        let Some(chunk) = self.upstream.next_chunk()? else {
            return Ok(None);
        };
        let mut columns = Vec::with_capacity(self.exprs.len());
        for (expr, _) in &self.exprs {
            columns.push(self.scalar.evaluate(expr, &chunk, &self.columns)?);
        }
        projected_chunk(&columns, &self.types, chunk.row_count()).map(Some)
    }
}

/// A pull operator that skips an offset and returns at most a fixed row count.
pub struct Limit {
    upstream: Box<dyn ChunkSource>,
    remaining: u64,
    offset: u64,
}

impl Limit {
    /// Creates a limiting operator over `upstream`.
    #[must_use]
    pub fn new(upstream: Box<dyn ChunkSource>, count: u64, offset: u64) -> Self {
        Self {
            upstream,
            remaining: count,
            offset,
        }
    }
}

impl ChunkSource for Limit {
    fn next_chunk(&mut self) -> DevonResult<Option<Chunk>> {
        if self.remaining == 0 {
            return Ok(None);
        }

        loop {
            let Some(chunk) = self.upstream.next_chunk()? else {
                return Ok(None);
            };
            let row_count = chunk.row_count();
            if self.offset >= row_count as u64 {
                self.offset -= row_count as u64;
                continue;
            }

            let start = self.offset as usize;
            self.offset = 0;
            let available = row_count - start;
            let take = if self.remaining >= available as u64 {
                available
            } else {
                self.remaining as usize
            };
            self.remaining -= take as u64;
            return copy_rows(&chunk, start..start + take).map(Some);
        }
    }
}

/// Deferred constructor for one ordinary node-table scan.
type InterfaceSourceFactory = dyn FnOnce() -> DevonResult<Box<dyn ChunkSource>>;

/// One ordinary node-table scan participating in an interface scan.
pub struct InterfaceScanInput {
    source: Option<Box<dyn ChunkSource>>,
    factory: Option<Box<InterfaceSourceFactory>>,
    projection: Vec<usize>,
    class_name: String,
}

impl InterfaceScanInput {
    /// Creates one implementing-table input.
    ///
    /// `projection` maps interface-column order to the ordinary scan's
    /// physical property-column indexes. The scan's private node-offset
    /// column is intentionally not projected.
    #[must_use]
    pub fn new(source: Box<dyn ChunkSource>, projection: Vec<usize>, class_name: String) -> Self {
        Self {
            source: Some(source),
            factory: None,
            projection,
            class_name,
        }
    }

    /// Creates an input whose ordinary table scan opens only when reached.
    ///
    /// Deferring open keeps interface scans at one table's scan working set
    /// at a time, including committed overlay rows owned by that scan.
    #[must_use]
    pub fn deferred(
        factory: impl FnOnce() -> DevonResult<Box<dyn ChunkSource>> + 'static,
        projection: Vec<usize>,
        class_name: String,
    ) -> Self {
        Self {
            source: None,
            factory: Some(Box::new(factory)),
            projection,
            class_name,
        }
    }

    fn source(&mut self) -> DevonResult<&mut Box<dyn ChunkSource>> {
        if self.source.is_none() {
            let factory = self
                .factory
                .take()
                .ok_or_else(|| invalid_argument("interface scan input has no source factory"))?;
            self.source = Some(factory()?);
        }
        self.source
            .as_mut()
            .ok_or_else(|| invalid_argument("interface scan source disappeared after open"))
    }
}

/// A streaming concatenation of the node-table scans implementing an interface.
///
/// Inputs are pulled in catalog declaration order. Each input chunk is
/// projected to the interface declaration and receives one trailing private
/// String discriminator consumed by `classof(binding)`.
pub struct ScanInterface {
    inputs: VecDeque<InterfaceScanInput>,
    interface_types: Vec<LogicalType>,
}

impl ScanInterface {
    /// Creates an interface scan from ordered ordinary table scans.
    #[must_use]
    pub fn new(inputs: Vec<InterfaceScanInput>, interface_types: Vec<LogicalType>) -> Self {
        Self {
            inputs: inputs.into(),
            interface_types,
        }
    }

    fn project_chunk(&self, input: &InterfaceScanInput, chunk: &Chunk) -> DevonResult<Chunk> {
        if input.projection.len() != self.interface_types.len() {
            return Err(invalid_argument(format!(
                "interface class `{}` projects {} columns into an interface with {} columns",
                input.class_name,
                input.projection.len(),
                self.interface_types.len()
            )));
        }
        for (output, source) in self.interface_types.iter().zip(&input.projection) {
            let actual = chunk.types().get(*source).ok_or_else(|| {
                invalid_argument(format!(
                    "interface class `{}` projects missing chunk column {source}",
                    input.class_name
                ))
            })?;
            if actual != output {
                return Err(invalid_argument(format!(
                    "interface class `{}` projects chunk type {actual} where {output} is required",
                    input.class_name
                )));
            }
        }
        let mut types = self.interface_types.clone();
        types.push(LogicalType::String);
        let mut builder = ChunkBuilder::new(types);
        for row in 0..chunk.row_count() {
            let mut projected = Vec::with_capacity(input.projection.len() + 1);
            for column in &input.projection {
                projected.push(
                    chunk
                        .value(row, *column)
                        .ok_or_else(|| invalid_argument("interface scan row is incomplete"))?,
                );
            }
            projected.push(Value::String(input.class_name.clone()));
            builder.push_row(projected)?;
        }
        Ok(builder.finish())
    }
}

impl ChunkSource for ScanInterface {
    fn next_chunk(&mut self) -> DevonResult<Option<Chunk>> {
        loop {
            let Some(input) = self.inputs.front_mut() else {
                return Ok(None);
            };
            match input.source()?.next_chunk()? {
                Some(chunk) => {
                    let input = self
                        .inputs
                        .front()
                        .ok_or_else(|| invalid_argument("interface input disappeared"))?;
                    return self.project_chunk(input, &chunk).map(Some);
                }
                None => {
                    self.inputs.pop_front();
                }
            }
        }
    }
}

/// Configuration shared by one spill-capable blocking operator.
#[derive(Clone, Debug)]
pub struct SpillConfig {
    /// The statement-wide memory accountant charged by buffered rows.
    pub budget: Arc<MemoryBudget>,
    /// Directory in which temporary sorted runs are created.
    pub tmp_dir: PathBuf,
}

impl SpillConfig {
    /// Creates a config that preserves the pre-spill executor behavior.
    #[must_use]
    pub fn unbounded() -> Self {
        Self {
            budget: Arc::new(MemoryBudget::unlimited()),
            tmp_dir: std::env::temp_dir().join("devondb-exec.tmp"),
        }
    }
}

/// Writer-policy estimate for a buffered row's vectors and allocator metadata.
/// Value payloads are charged separately through [`Value::approx_bytes`].
const BUFFERED_ROW_OVERHEAD_BYTES: usize = 64;
const MIN_SPILL_ROW_BYTES: usize = 2;
const MAX_SPILL_ROW_BYTES: usize = 256 * 1024 * 1024;
const SPILL_ROW_MAGIC: &[u8; 8] = b"DVNSPROW";
const SPILL_ROW_FORMAT_VERSION: u16 = 1;
const SPILL_ROW_HEADER_BYTES: usize = SPILL_ROW_MAGIC.len() + size_of::<u16>() + size_of::<u32>();
static NEXT_SPILL_OPERATOR_ID: AtomicU64 = AtomicU64::new(0);

/// Distinguishes this process incarnation in spill-run file names: after a
/// crash, a recycled pid plus a restarted operator-id sequence would
/// otherwise collide with leftover files under `create_new` and kill an
/// unrelated query with `AlreadyExists`.
static SPILL_PROCESS_TOKEN: OnceLock<u64> = OnceLock::new();

/// Monotone count of spill run files this process has created. This provides
/// direct observability even though `<db>.tmp` remains present while a handle
/// is open.
static SPILL_RUNS_CREATED: AtomicU64 = AtomicU64::new(0);

/// The number of spill run files this process has created so far.
#[must_use]
pub fn spill_runs_created() -> u64 {
    SPILL_RUNS_CREATED.load(AtomicOrdering::Relaxed)
}

fn spill_process_token() -> u64 {
    *SPILL_PROCESS_TOKEN.get_or_init(|| {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        (nanos as u64) ^ ((nanos >> 64) as u64)
    })
}

#[derive(Default)]
struct ScalarExpressions {
    executor: Option<Arc<dyn ScalarSubqueryExecutor>>,
    budget: Option<Arc<MemoryBudget>>,
    uncorrelated: HashMap<usize, ScalarValue>,
    cached_charge: usize,
}

#[derive(Clone)]
struct ScalarValue {
    output_type: LogicalType,
    value: Value,
}

struct ResolvedScalarRow {
    values: Vec<Value>,
    types: Vec<LogicalType>,
    columns: HashMap<String, usize>,
    outer_width: usize,
    next_column: usize,
}

impl ScalarExpressions {
    fn new(executor: Arc<dyn ScalarSubqueryExecutor>) -> Self {
        let budget = executor.memory_budget();
        Self {
            executor: Some(executor),
            budget: Some(budget),
            uncorrelated: HashMap::new(),
            cached_charge: 0,
        }
    }

    fn evaluate(
        &mut self,
        expr: &Expr,
        chunk: &Chunk,
        columns: &HashMap<String, usize>,
    ) -> DevonResult<Vec<Value>> {
        if !expression_contains_scalar(expr) {
            return evaluate(expr, chunk, columns);
        }
        (0..chunk.row_count())
            .map(|row| self.evaluate_row(expr, chunk, columns, row))
            .collect()
    }

    fn evaluate_row(
        &mut self,
        expr: &Expr,
        chunk: &Chunk,
        columns: &HashMap<String, usize>,
        row: usize,
    ) -> DevonResult<Value> {
        let mut resolved = ResolvedScalarRow::new(chunk, columns, row)?;
        let expr = self.resolve_row(expr, &mut resolved)?;
        resolved.evaluate_one(&expr)
    }

    fn resolve_row(&mut self, expr: &Expr, row: &mut ResolvedScalarRow) -> DevonResult<Expr> {
        match expr {
            Expr::Col(_) | Expr::Lit(_) | Expr::ClassOf(_) | Expr::ScoreOf(_) => Ok(expr.clone()),
            Expr::Scalar { plan } => self.resolve_scalar(plan, row),
            Expr::If {
                cond,
                then_expr,
                else_expr,
            } => {
                let cond = self.resolve_row(cond, row)?;
                match row.evaluate_one(&cond)? {
                    Value::Bool(true) => self.resolve_row(then_expr, row),
                    Value::Bool(false) | Value::Null => self.resolve_row(else_expr, row),
                    value => Err(invalid_argument(format!(
                        "operator `if` requires a Bool or Null condition; got {}",
                        value_variant(&value)
                    ))),
                }
            }
            Expr::Coalesce(expressions) => self.resolve_coalesce(expressions, row),
            _ => self.resolve_eager(expr, row),
        }
    }

    fn resolve_eager(&mut self, expr: &Expr, row: &mut ResolvedScalarRow) -> DevonResult<Expr> {
        match expr {
            Expr::Binary { op, left, right } => Ok(Expr::Binary {
                op: *op,
                left: Box::new(self.resolve_row(left, row)?),
                right: Box::new(self.resolve_row(right, row)?),
            }),
            Expr::Not(operand) => Ok(Expr::Not(Box::new(self.resolve_row(operand, row)?))),
            Expr::Distance {
                left,
                right,
                metric,
            } => Ok(Expr::Distance {
                left: Box::new(self.resolve_row(left, row)?),
                right: Box::new(self.resolve_row(right, row)?),
                metric: *metric,
            }),
            Expr::Least(expressions) => Ok(Expr::Least(self.resolve_all(expressions, row)?)),
            Expr::Greatest(expressions) => Ok(Expr::Greatest(self.resolve_all(expressions, row)?)),
            Expr::DateTrunc { unit, value } => Ok(Expr::DateTrunc {
                unit: *unit,
                value: Box::new(self.resolve_row(value, row)?),
            }),
            Expr::DateAdd {
                unit,
                value,
                amount,
            } => Ok(Expr::DateAdd {
                unit: *unit,
                value: Box::new(self.resolve_row(value, row)?),
                amount: Box::new(self.resolve_row(amount, row)?),
            }),
            Expr::Round { value, places } => Ok(Expr::Round {
                value: Box::new(self.resolve_row(value, row)?),
                places: *places,
            }),
            Expr::RoundDiv {
                numerator,
                denominator,
                places,
            } => Ok(Expr::RoundDiv {
                numerator: Box::new(self.resolve_row(numerator, row)?),
                denominator: Box::new(self.resolve_row(denominator, row)?),
                places: *places,
            }),
            _ => Err(DevonError::Corrupt {
                context: "scalar resolver dispatched a lazy expression as eager".into(),
            }),
        }
    }

    fn resolve_coalesce(
        &mut self,
        expressions: &[Expr],
        row: &mut ResolvedScalarRow,
    ) -> DevonResult<Expr> {
        if expressions.len() < 2 {
            return Err(invalid_argument(
                "operator `coalesce` requires at least two operands",
            ));
        }
        for expression in expressions {
            let resolved = self.resolve_row(expression, row)?;
            if !matches!(row.evaluate_one(&resolved)?, Value::Null) {
                return Ok(resolved);
            }
        }
        Ok(Expr::Lit(Value::Null))
    }

    fn resolve_all(
        &mut self,
        expressions: &[Expr],
        row: &mut ResolvedScalarRow,
    ) -> DevonResult<Vec<Expr>> {
        expressions
            .iter()
            .map(|expression| self.resolve_row(expression, row))
            .collect()
    }

    fn resolve_scalar(
        &mut self,
        plan: &Operator,
        row: &mut ResolvedScalarRow,
    ) -> DevonResult<Expr> {
        let correlated = operator_references_outer(plan, &row.columns);
        let key = std::ptr::from_ref(plan).addr();
        let value = if !correlated {
            match self.uncorrelated.get(&key) {
                Some(value) => value.clone(),
                None => {
                    let value = self.execute_scalar(plan, &OuterBindings::new())?;
                    self.try_cache(key, &value)?;
                    value
                }
            }
        } else {
            self.execute_scalar(plan, &row.outer_bindings()?)?
        };
        Ok(Expr::Col(row.push_scalar(value)?))
    }

    fn execute_scalar(&self, plan: &Operator, outer: &OuterBindings) -> DevonResult<ScalarValue> {
        let executor = self.executor.as_ref().ok_or_else(|| {
            invalid_argument("operator `scalar` has no same-snapshot subquery executor")
        })?;
        let result = executor.execute(plan, outer)?;
        let value = match result.values.as_slice() {
            [] => Value::Null,
            [value] => value.clone(),
            _ => {
                return Err(invalid_argument(
                    "scalar subquery returned more than one row",
                ));
            }
        };
        if !value.matches_type(&result.output_type) {
            return Err(DevonError::Corrupt {
                context: format!(
                    "scalar subquery returned value {value} outside its declared {} output",
                    result.output_type
                ),
            });
        }
        Ok(ScalarValue {
            output_type: result.output_type,
            value,
        })
    }

    fn try_cache(&mut self, key: usize, value: &ScalarValue) -> DevonResult<()> {
        let charge = BUFFERED_ROW_OVERHEAD_BYTES
            .checked_add(value.value.approx_bytes())
            .ok_or_else(|| invalid_argument("scalar cache memory estimate exceeds usize::MAX"))?;
        let Some(budget) = &self.budget else {
            return Ok(());
        };
        if !budget.charge_or_reclaim(charge) {
            return Ok(());
        }
        self.cached_charge = self.cached_charge.checked_add(charge).ok_or_else(|| {
            budget.release(charge);
            invalid_argument("scalar cache charge exceeds usize::MAX")
        })?;
        self.uncorrelated.insert(key, value.clone());
        Ok(())
    }
}

impl Drop for ScalarExpressions {
    fn drop(&mut self) {
        if let Some(budget) = &self.budget {
            budget.release(self.cached_charge);
        }
    }
}

impl ResolvedScalarRow {
    fn new(chunk: &Chunk, columns: &HashMap<String, usize>, row: usize) -> DevonResult<Self> {
        let values = clone_row(chunk, row)?;
        let outer_width = values.len();
        Ok(Self {
            values,
            types: chunk.types().to_vec(),
            columns: columns.clone(),
            outer_width,
            next_column: 0,
        })
    }

    fn evaluate_one(&self, expr: &Expr) -> DevonResult<Value> {
        let mut builder = ChunkBuilder::new(self.types.clone());
        builder.push_row(self.values.clone())?;
        let values = evaluate(expr, &builder.finish(), &self.columns)?;
        values
            .into_iter()
            .next()
            .ok_or_else(|| DevonError::Corrupt {
                context: "one-row scalar expression produced no value".into(),
            })
    }

    fn push_scalar(&mut self, scalar: ScalarValue) -> DevonResult<String> {
        let name = loop {
            let candidate = format!("__scalar.value{}", self.next_column);
            self.next_column = self
                .next_column
                .checked_add(1)
                .ok_or_else(|| invalid_argument("scalar temporary-column count overflowed"))?;
            if !self.columns.contains_key(&candidate) {
                break candidate;
            }
        };
        let index = self.values.len();
        self.values.push(scalar.value);
        self.types.push(scalar.output_type);
        self.columns.insert(name.clone(), index);
        Ok(name)
    }

    fn outer_bindings(&self) -> DevonResult<OuterBindings> {
        self.columns
            .iter()
            .filter(|(_, index)| **index < self.outer_width)
            .map(|(name, index)| {
                self.values
                    .get(*index)
                    .cloned()
                    .map(|value| (fold(name).into_owned(), value))
                    .ok_or_else(|| {
                        invalid_argument(format!(
                            "outer binding `{name}` maps to missing column {index}"
                        ))
                    })
            })
            .collect()
    }
}

fn expression_contains_scalar(expr: &Expr) -> bool {
    match expr {
        Expr::Scalar { .. } => true,
        Expr::Binary { left, right, .. } | Expr::Distance { left, right, .. } => {
            expression_contains_scalar(left) || expression_contains_scalar(right)
        }
        Expr::If {
            cond,
            then_expr,
            else_expr,
        } => {
            expression_contains_scalar(cond)
                || expression_contains_scalar(then_expr)
                || expression_contains_scalar(else_expr)
        }
        Expr::Coalesce(expressions) | Expr::Least(expressions) | Expr::Greatest(expressions) => {
            expressions.iter().any(expression_contains_scalar)
        }
        Expr::DateTrunc { value, .. } | Expr::Round { value, .. } => {
            expression_contains_scalar(value)
        }
        Expr::DateAdd { value, amount, .. } => {
            expression_contains_scalar(value) || expression_contains_scalar(amount)
        }
        Expr::RoundDiv {
            numerator,
            denominator,
            ..
        } => expression_contains_scalar(numerator) || expression_contains_scalar(denominator),
        Expr::Not(operand) => expression_contains_scalar(operand),
        Expr::Col(_) | Expr::Lit(_) | Expr::ClassOf(_) | Expr::ScoreOf(_) => false,
    }
}

fn operator_references_outer(operator: &Operator, outer_columns: &HashMap<String, usize>) -> bool {
    match operator {
        Operator::ScanNodes { .. }
        | Operator::ScanInterface { .. }
        | Operator::KnnScan { .. }
        | Operator::TextScan { .. }
        | Operator::WithinScan { .. } => false,
        Operator::ExpandRel { input, .. }
        | Operator::Expand { input, .. }
        | Operator::Limit { input, .. } => operator_references_outer(input, outer_columns),
        Operator::Filter { predicate, input } => {
            expression_references_outer(predicate, outer_columns)
                || operator_references_outer(input, outer_columns)
        }
        Operator::Project { exprs, input } => {
            exprs
                .iter()
                .any(|item| expression_references_outer(&item.expr, outer_columns))
                || operator_references_outer(input, outer_columns)
        }
        Operator::Sort { keys, input } => {
            keys.iter()
                .any(|key| expression_references_outer(&key.expr, outer_columns))
                || operator_references_outer(input, outer_columns)
        }
        Operator::Aggregate {
            group_by,
            aggs,
            input,
        } => {
            group_by
                .iter()
                .any(|expr| expression_references_outer(expr, outer_columns))
                || aggs
                    .iter()
                    .any(|agg| expression_references_outer(&agg.expr, outer_columns))
                || operator_references_outer(input, outer_columns)
        }
        Operator::HashJoin {
            on, left, right, ..
        } => {
            on.iter().any(|key| {
                expression_references_outer(&key.left, outer_columns)
                    || expression_references_outer(&key.right, outer_columns)
            }) || operator_references_outer(left, outer_columns)
                || operator_references_outer(right, outer_columns)
        }
    }
}

fn expression_references_outer(expr: &Expr, outer_columns: &HashMap<String, usize>) -> bool {
    match expr {
        Expr::Col(reference) => outer_columns
            .keys()
            .any(|outer| fold(outer) == fold(reference)),
        Expr::ClassOf(binding) => outer_columns.contains_key(&classof_column_key(binding)),
        Expr::ScoreOf(binding) => outer_columns.contains_key(&scoreof_column_key(binding)),
        Expr::Scalar { plan } => operator_references_outer(plan, outer_columns),
        Expr::Binary { left, right, .. } | Expr::Distance { left, right, .. } => {
            expression_references_outer(left, outer_columns)
                || expression_references_outer(right, outer_columns)
        }
        Expr::If {
            cond,
            then_expr,
            else_expr,
        } => {
            expression_references_outer(cond, outer_columns)
                || expression_references_outer(then_expr, outer_columns)
                || expression_references_outer(else_expr, outer_columns)
        }
        Expr::Coalesce(expressions) | Expr::Least(expressions) | Expr::Greatest(expressions) => {
            expressions
                .iter()
                .any(|expr| expression_references_outer(expr, outer_columns))
        }
        Expr::DateTrunc { value, .. } | Expr::Round { value, .. } => {
            expression_references_outer(value, outer_columns)
        }
        Expr::DateAdd { value, amount, .. } => {
            expression_references_outer(value, outer_columns)
                || expression_references_outer(amount, outer_columns)
        }
        Expr::RoundDiv {
            numerator,
            denominator,
            ..
        } => {
            expression_references_outer(numerator, outer_columns)
                || expression_references_outer(denominator, outer_columns)
        }
        Expr::Not(operand) => expression_references_outer(operand, outer_columns),
        Expr::Lit(_) => false,
    }
}

/// A blocking pull operator that stably orders rows by evaluated keys.
pub struct Sort {
    upstream: Box<dyn ChunkSource>,
    keys: Vec<(Expr, SortOrder)>,
    columns: HashMap<String, usize>,
    config: SpillConfig,
    output: Option<SortOutput>,
    // Terminal-state latch: once `initialize` fails, this replayable copy of
    // the error makes every later `next_chunk` fail the same way without
    // re-pulling the upstream.
    initialization_error: Option<DevonError>,
    spill_files: SpillFiles,
    scalar: ScalarExpressions,
}

impl Sort {
    /// Creates a sort over `upstream` using the input-column reference map.
    #[must_use]
    pub fn new(
        upstream: Box<dyn ChunkSource>,
        keys: Vec<(Expr, SortOrder)>,
        columns: HashMap<String, usize>,
        config: SpillConfig,
    ) -> Self {
        let spill_files = SpillFiles::new(&config.tmp_dir);
        Self {
            upstream,
            keys,
            columns,
            config,
            output: None,
            spill_files,
            scalar: ScalarExpressions::default(),
            initialization_error: None,
        }
    }

    /// Installs the same-snapshot runner used by scalar expressions.
    #[must_use]
    pub fn with_scalar_executor(mut self, executor: Arc<dyn ScalarSubqueryExecutor>) -> Self {
        self.scalar = ScalarExpressions::new(executor);
        self
    }

    fn initialize(&mut self) -> DevonResult<()> {
        let mut rows = ChargedRows::new(Arc::clone(&self.config.budget));
        let mut types = None;
        while let Some(chunk) = self.upstream.next_chunk()? {
            if types.is_none() {
                types = Some(chunk.types().to_vec());
            }
            let evaluated = evaluate_sort_rows(
                std::slice::from_ref(&chunk),
                &self.keys,
                &self.columns,
                &mut self.scalar,
            )?;
            for row in evaluated {
                let charge = sort_row_charge(&row)?;
                if !self.config.budget.charge_or_reclaim(charge) {
                    spill_sort_run(&mut rows, &mut self.spill_files, &self.keys)?;
                    charge_after_spill(&self.config.budget, charge, "Sort row")?;
                }
                rows.push_charged(row, charge);
            }
        }

        let Some(types) = types else {
            self.output = Some(SortOutput::InMemory(VecDeque::new()));
            return Ok(());
        };
        if self.spill_files.is_empty() {
            stable_sort_by(rows.as_mut_vec(), |left, right| {
                compare_sort_rows(left, right, &self.keys)
            })?;
            let rows = rows.take_rows();
            self.output = Some(SortOutput::InMemory(build_output_chunks(
                &types,
                rows.into_iter().map(|row| row.values),
            )?));
        } else {
            spill_sort_run(&mut rows, &mut self.spill_files, &self.keys)?;
            self.output = Some(SortOutput::External(SortMerge::new(
                types,
                self.keys.clone(),
                self.spill_files.paths(),
                Arc::clone(&self.config.budget),
            )?));
        }
        Ok(())
    }
}

impl ChunkSource for Sort {
    fn next_chunk(&mut self) -> DevonResult<Option<Chunk>> {
        if self.output.is_none() {
            if let Some(error) = &self.initialization_error {
                return Err(replay_terminal_error(error));
            }
            if let Err(error) = self.initialize() {
                self.initialization_error = Some(replay_terminal_error(&error));
                return Err(error);
            }
        }
        match self.output.as_mut() {
            Some(SortOutput::InMemory(output)) => Ok(output.pop_front()),
            Some(SortOutput::External(output)) => output.next_chunk(),
            None => Err(invalid_sort_permutation()),
        }
    }
}

/// A blocking pull operator that groups rows and computes aggregate values.
pub struct Aggregate {
    upstream: Box<dyn ChunkSource>,
    group_by: Vec<Expr>,
    aggs: Vec<(AggregateFunction, Expr)>,
    columns: HashMap<String, usize>,
    output_types: Vec<LogicalType>,
    config: SpillConfig,
    output: Option<AggregateOutput>,
    // Terminal-state latch: once `initialize` fails, this replayable copy of
    // the error makes every later `next_chunk` fail the same way without
    // re-pulling the upstream.
    initialization_error: Option<DevonError>,
    spill_files: SpillFiles,
    scalar: ScalarExpressions,
}

impl Aggregate {
    /// Creates an aggregate over `upstream` with its ordered output types.
    #[must_use]
    pub fn new(
        upstream: Box<dyn ChunkSource>,
        group_by: Vec<Expr>,
        aggs: Vec<(AggregateFunction, Expr)>,
        columns: HashMap<String, usize>,
        output_types: Vec<LogicalType>,
        config: SpillConfig,
    ) -> Self {
        let spill_files = SpillFiles::new(&config.tmp_dir);
        Self {
            upstream,
            group_by,
            aggs,
            columns,
            output_types,
            config,
            output: None,
            spill_files,
            scalar: ScalarExpressions::default(),
            initialization_error: None,
        }
    }

    /// Installs the same-snapshot runner used by scalar expressions.
    #[must_use]
    pub fn with_scalar_executor(mut self, executor: Arc<dyn ScalarSubqueryExecutor>) -> Self {
        self.scalar = ScalarExpressions::new(executor);
        self
    }

    fn initialize(&mut self) -> DevonResult<()> {
        if self.group_by.is_empty()
            && self
                .aggs
                .iter()
                .all(|(function, _)| has_constant_aggregate_state(*function))
        {
            self.initialize_streaming()
        } else {
            self.initialize_buffered()
        }
    }

    fn initialize_streaming(&mut self) -> DevonResult<()> {
        let mut states = self
            .aggs
            .iter()
            .map(|(function, _)| initial_aggregate_state(*function))
            .collect::<DevonResult<Vec<_>>>()?;
        while let Some(chunk) = self.upstream.next_chunk()? {
            update_streaming_chunk(
                &mut states,
                &self.aggs,
                &chunk,
                &self.columns,
                &mut self.scalar,
            )?;
        }
        let output_row = finish_aggregate_states(states, &self.output_types)?;
        self.output = Some(AggregateOutput::InMemory(build_output_chunks(
            &self.output_types,
            [output_row],
        )?));
        Ok(())
    }

    fn initialize_buffered(&mut self) -> DevonResult<()> {
        if let Some(layout) = typed_grouped_layout(&self.group_by, &self.aggs, &self.columns)? {
            let fallback = self.initialize_typed_grouped(&layout)?;
            if let Some(chunks) = fallback {
                return self.initialize_buffered_legacy(chunks);
            }
            return Ok(());
        }
        self.initialize_buffered_legacy(Vec::new())
    }

    fn initialize_typed_grouped(
        &mut self,
        layout: &TypedAggregateLayout,
    ) -> DevonResult<Option<Vec<Chunk>>> {
        let mut charge = TypedAggregateCharge::new(Arc::clone(&self.config.budget));
        let mut consumed = Vec::new();
        let mut lookup = HashMap::new();
        let mut groups = Vec::new();
        while let Some(chunk) = self.upstream.next_chunk()? {
            if !layout.supports(&chunk) {
                consumed.push(chunk);
                charge.release();
                return Ok(Some(consumed));
            }
            let bytes = typed_aggregate_chunk_charge(&chunk, layout)?;
            if !charge.try_grow(bytes)? {
                consumed.push(chunk);
                charge.release();
                return Ok(Some(consumed));
            }
            append_typed_aggregate_chunk(&mut lookup, &mut groups, layout, &self.aggs, &chunk)?;
            consumed.push(chunk);
        }
        charge.release();
        drop(consumed);
        let rows = finish_typed_groups(groups, self.group_by.len(), &self.output_types)?;
        self.output = Some(AggregateOutput::InMemory(build_output_chunks(
            &self.output_types,
            rows,
        )?));
        Ok(None)
    }

    fn initialize_buffered_legacy(&mut self, initial_chunks: Vec<Chunk>) -> DevonResult<()> {
        let mut rows = ChargedRows::new(Arc::clone(&self.config.budget));
        for chunk in initial_chunks {
            self.buffer_aggregate_chunk(&mut rows, &chunk)?;
        }
        while let Some(chunk) = self.upstream.next_chunk()? {
            self.buffer_aggregate_chunk(&mut rows, &chunk)?;
        }

        if self.spill_files.is_empty() {
            stable_sort_by(rows.as_mut_vec(), |left, right| {
                compare_key_tuples(&left.group_key, &right.group_key)
            })?;
            let mut rows = rows.take_rows();
            let output_rows = aggregate_groups(
                &mut rows,
                self.group_by.len(),
                &self.aggs,
                &self.output_types,
            )?;
            self.output = Some(AggregateOutput::InMemory(build_output_chunks(
                &self.output_types,
                output_rows,
            )?));
        } else {
            spill_aggregate_run(&mut rows, &mut self.spill_files, &self.aggs)?;
            self.output = Some(AggregateOutput::External(AggregateMerge::new(
                self.output_types.clone(),
                self.group_by.len(),
                self.aggs.clone(),
                self.spill_files.paths(),
                Arc::clone(&self.config.budget),
            )?));
        }
        Ok(())
    }

    fn buffer_aggregate_chunk(
        &mut self,
        rows: &mut ChargedRows<AggregateInputRow>,
        chunk: &Chunk,
    ) -> DevonResult<()> {
        let evaluated = evaluate_aggregate_rows(
            std::slice::from_ref(chunk),
            &self.group_by,
            &self.aggs,
            &self.columns,
            &mut self.scalar,
        )?;
        for row in evaluated {
            let charge = aggregate_row_charge(&row)?;
            if !self.config.budget.charge_or_reclaim(charge) {
                spill_aggregate_run(rows, &mut self.spill_files, &self.aggs)?;
                charge_after_spill(&self.config.budget, charge, "Aggregate input row")?;
            }
            rows.push_charged(row, charge);
        }
        Ok(())
    }
}

impl ChunkSource for Aggregate {
    fn next_chunk(&mut self) -> DevonResult<Option<Chunk>> {
        let expected_types = self.group_by.len() + self.aggs.len();
        if self.output_types.len() != expected_types {
            return Err(invalid_argument(format!(
                "aggregate has {expected_types} outputs but {} output types",
                self.output_types.len()
            )));
        }
        if self.output.is_none() {
            if let Some(error) = &self.initialization_error {
                return Err(replay_terminal_error(error));
            }
            if let Err(error) = self.initialize() {
                self.initialization_error = Some(replay_terminal_error(&error));
                return Err(error);
            }
        }
        match self.output.as_mut() {
            Some(AggregateOutput::InMemory(output)) => Ok(output.pop_front()),
            Some(AggregateOutput::External(output)) => output.next_chunk(),
            None => Err(invalid_sort_permutation()),
        }
    }
}

enum SortOutput {
    InMemory(VecDeque<Chunk>),
    External(SortMerge),
}

enum AggregateOutput {
    InMemory(VecDeque<Chunk>),
    External(AggregateMerge),
}

struct SortRow {
    values: Vec<Value>,
    keys: Vec<Value>,
}

struct AggregateInputRow {
    group_key: Vec<Value>,
    record: AggregateRecord,
}

enum AggregateRecord {
    Input(Vec<Value>),
    PercentileCount { index: usize, count: i64 },
    PercentileValue { index: usize, value: Decimal128 },
}

enum AggregateState {
    Count(i64),
    Sum(Option<Value>),
    Min(Option<Value>),
    Max(Option<Value>),
    Avg {
        sum: Option<Value>,
        count: u64,
    },
    Percentile {
        expected: u64,
        seen: u64,
        scale: Option<u8>,
        lower: Option<Decimal128>,
        upper: Option<Decimal128>,
    },
}

struct TypedAggregateLayout {
    group_columns: Vec<usize>,
    aggregate_columns: Vec<usize>,
}

impl TypedAggregateLayout {
    fn supports(&self, chunk: &Chunk) -> bool {
        self.group_columns
            .iter()
            .chain(&self.aggregate_columns)
            .all(|index| chunk.column(*index).is_some_and(is_primitive_column))
    }

    fn all_columns(&self) -> impl Iterator<Item = usize> + '_ {
        self.group_columns
            .iter()
            .chain(&self.aggregate_columns)
            .copied()
    }
}

struct TypedGroup {
    key: Vec<Value>,
    states: Vec<AggregateState>,
}

#[derive(Debug, PartialEq, Eq, Hash)]
enum TypedGroupKey {
    Single(TypedGroupValue),
    Composite(Vec<TypedGroupValue>),
}

#[derive(Debug, PartialEq, Eq, Hash)]
enum TypedGroupValue {
    Null,
    Bool(bool),
    Int64(i64),
    Float64(u64),
    Timestamp(i64),
    Decimal(Decimal128),
}

struct TypedAggregateCharge {
    budget: Arc<MemoryBudget>,
    bytes: usize,
}

impl TypedAggregateCharge {
    fn new(budget: Arc<MemoryBudget>) -> Self {
        Self { budget, bytes: 0 }
    }

    fn try_grow(&mut self, bytes: usize) -> DevonResult<bool> {
        let total = self.bytes.checked_add(bytes).ok_or_else(|| {
            invalid_argument("blocking-operator row memory estimate exceeds usize::MAX")
        })?;
        if !self.budget.try_charge(bytes) {
            return Ok(false);
        }
        self.bytes = total;
        Ok(true)
    }

    fn release(&mut self) {
        self.budget.release(self.bytes);
        self.bytes = 0;
    }
}

impl Drop for TypedAggregateCharge {
    fn drop(&mut self) {
        self.release();
    }
}

const fn has_constant_aggregate_state(function: AggregateFunction) -> bool {
    matches!(
        function,
        AggregateFunction::Count
            | AggregateFunction::Sum
            | AggregateFunction::Min
            | AggregateFunction::Max
            | AggregateFunction::Avg
    )
}

fn typed_grouped_layout(
    group_by: &[Expr],
    aggs: &[(AggregateFunction, Expr)],
    columns: &HashMap<String, usize>,
) -> DevonResult<Option<TypedAggregateLayout>> {
    if group_by.is_empty()
        || aggs
            .iter()
            .any(|(function, _)| *function == AggregateFunction::PercentileCont)
    {
        return Ok(None);
    }
    let Some(group_columns) = direct_expression_columns(group_by.iter(), columns)? else {
        return Ok(None);
    };
    let Some(aggregate_columns) =
        direct_expression_columns(aggs.iter().map(|(_, expression)| expression), columns)?
    else {
        return Ok(None);
    };
    Ok(Some(TypedAggregateLayout {
        group_columns,
        aggregate_columns,
    }))
}

fn direct_expression_columns<'a>(
    expressions: impl IntoIterator<Item = &'a Expr>,
    columns: &HashMap<String, usize>,
) -> DevonResult<Option<Vec<usize>>> {
    let mut indices = Vec::new();
    for expression in expressions {
        let Expr::Col(reference) = expression else {
            return Ok(None);
        };
        let index = columns
            .get(reference)
            .copied()
            .ok_or_else(|| invalid_argument(format!("unknown column reference `{reference}`")))?;
        indices.push(index);
    }
    Ok(Some(indices))
}

const fn is_primitive_column(column: &Column) -> bool {
    !matches!(column, Column::Boxed(_))
}

fn typed_aggregate_chunk_charge(
    chunk: &Chunk,
    layout: &TypedAggregateLayout,
) -> DevonResult<usize> {
    let mut total = 0_usize;
    for row in 0..chunk.row_count() {
        let mut row_charge = BUFFERED_ROW_OVERHEAD_BYTES;
        for index in layout.all_columns() {
            let column = chunk.column(index).ok_or_else(|| {
                invalid_argument(format!("aggregate column {index} is out of range"))
            })?;
            row_charge = row_charge
                .checked_add(typed_value_charge(column, row)?)
                .ok_or_else(blocking_row_charge_overflow)?;
        }
        total = total
            .checked_add(row_charge)
            .ok_or_else(blocking_row_charge_overflow)?;
    }
    Ok(total)
}

fn typed_value_charge(column: &Column, row: usize) -> DevonResult<usize> {
    match column {
        Column::Int64 { .. }
        | Column::Float64 { .. }
        | Column::Bool { .. }
        | Column::Timestamp { .. } => Ok(16),
        Column::Decimal { validity, .. } => Ok(if is_valid(validity, row) { 24 } else { 16 }),
        Column::Boxed(_) => Err(invalid_argument(
            "typed aggregate charge received a Boxed column",
        )),
    }
}

fn blocking_row_charge_overflow() -> DevonError {
    invalid_argument("blocking-operator row memory estimate exceeds usize::MAX")
}

fn append_typed_aggregate_chunk(
    lookup: &mut HashMap<TypedGroupKey, usize>,
    groups: &mut Vec<TypedGroup>,
    layout: &TypedAggregateLayout,
    aggs: &[(AggregateFunction, Expr)],
    chunk: &Chunk,
) -> DevonResult<()> {
    let mut group_indices = Vec::with_capacity(chunk.row_count());
    for row in 0..chunk.row_count() {
        let key = typed_group_key(chunk, &layout.group_columns, row)?;
        let index = if let Some(index) = lookup.get(&key) {
            *index
        } else {
            let index = groups.len();
            lookup.insert(key, index);
            groups.push(TypedGroup {
                key: materialize_group_key(chunk, &layout.group_columns, row)?,
                states: initial_aggregate_states(aggs)?,
            });
            index
        };
        group_indices.push(index);
    }
    for (state_index, column_index) in layout.aggregate_columns.iter().copied().enumerate() {
        let column = chunk.column(column_index).ok_or_else(|| {
            invalid_argument(format!("aggregate column {column_index} is out of range"))
        })?;
        update_grouped_typed_column(groups, &group_indices, state_index, column)?;
    }
    Ok(())
}

fn initial_aggregate_states(
    aggs: &[(AggregateFunction, Expr)],
) -> DevonResult<Vec<AggregateState>> {
    aggs.iter()
        .map(|(function, _)| initial_aggregate_state(*function))
        .collect()
}

fn typed_group_key(chunk: &Chunk, columns: &[usize], row: usize) -> DevonResult<TypedGroupKey> {
    if let [index] = columns {
        let column = chunk.column(*index).ok_or_else(|| {
            invalid_argument(format!("aggregate group column {index} is out of range"))
        })?;
        return primitive_group_value(column, row).map(TypedGroupKey::Single);
    }
    let values = columns
        .iter()
        .map(|index| {
            let column = chunk.column(*index).ok_or_else(|| {
                invalid_argument(format!("aggregate group column {index} is out of range"))
            })?;
            primitive_group_value(column, row)
        })
        .collect::<DevonResult<Vec<_>>>()?;
    Ok(TypedGroupKey::Composite(values))
}

fn primitive_group_value(column: &Column, row: usize) -> DevonResult<TypedGroupValue> {
    match column {
        Column::Int64 { values, validity } => Ok(if is_valid(validity, row) {
            TypedGroupValue::Int64(values[row])
        } else {
            TypedGroupValue::Null
        }),
        Column::Float64 { values, validity } => Ok(if is_valid(validity, row) {
            TypedGroupValue::Float64(values[row].to_bits())
        } else {
            TypedGroupValue::Null
        }),
        Column::Bool { values, validity } => Ok(if is_valid(validity, row) {
            TypedGroupValue::Bool(values[row])
        } else {
            TypedGroupValue::Null
        }),
        Column::Timestamp { values, validity } => Ok(if is_valid(validity, row) {
            TypedGroupValue::Timestamp(values[row])
        } else {
            TypedGroupValue::Null
        }),
        Column::Decimal {
            values,
            scale,
            validity,
        } => {
            if is_valid(validity, row) {
                Ok(TypedGroupValue::Decimal(Decimal128::new(
                    values[row],
                    *scale,
                )?))
            } else {
                Ok(TypedGroupValue::Null)
            }
        }
        Column::Boxed(_) => Err(invalid_argument(
            "typed aggregate grouping received a Boxed column",
        )),
    }
}

fn materialize_group_key(chunk: &Chunk, columns: &[usize], row: usize) -> DevonResult<Vec<Value>> {
    columns
        .iter()
        .map(|index| {
            chunk
                .column(*index)
                .map(|column| column.value_at(row))
                .ok_or_else(|| {
                    invalid_argument(format!("aggregate group column {index} is out of range"))
                })
        })
        .collect()
}

fn finish_typed_groups(
    mut groups: Vec<TypedGroup>,
    group_count: usize,
    output_types: &[LogicalType],
) -> DevonResult<Vec<Vec<Value>>> {
    stable_sort_by(&mut groups, |left, right| {
        compare_key_tuples(&left.key, &right.key)
    })?;
    groups
        .into_iter()
        .map(|group| {
            let mut row = group.key;
            row.extend(finish_aggregate_states(
                group.states,
                &output_types[group_count..],
            )?);
            Ok(row)
        })
        .collect()
}

fn evaluate_sort_rows(
    chunks: &[Chunk],
    keys: &[(Expr, SortOrder)],
    columns: &HashMap<String, usize>,
    scalar: &mut ScalarExpressions,
) -> DevonResult<Vec<SortRow>> {
    let mut rows = Vec::new();
    for chunk in chunks {
        let key_columns = keys
            .iter()
            .map(|(expr, _)| scalar.evaluate(expr, chunk, columns))
            .collect::<DevonResult<Vec<_>>>()?;
        for row in 0..chunk.row_count() {
            let row_keys = values_at(&key_columns, row, "sort key")?;
            validate_sort_keys(&row_keys)?;
            rows.push(SortRow {
                values: clone_row(chunk, row)?,
                keys: row_keys,
            });
        }
    }
    Ok(rows)
}

fn validate_sort_keys(keys: &[Value]) -> DevonResult<()> {
    for key in keys {
        if !matches!(key, Value::Null) {
            compare_order_values(key, key)?;
        }
    }
    Ok(())
}

fn compare_sort_rows(
    left: &SortRow,
    right: &SortRow,
    keys: &[(Expr, SortOrder)],
) -> DevonResult<Ordering> {
    for ((left, right), (_, order)) in left.keys.iter().zip(&right.keys).zip(keys) {
        let ordering = compare_order_values(left, right)?;
        let ordering = match order {
            SortOrder::Asc => ordering,
            SortOrder::Desc => ordering.reverse(),
        };
        if ordering != Ordering::Equal {
            return Ok(ordering);
        }
    }
    Ok(Ordering::Equal)
}

fn evaluate_aggregate_rows(
    chunks: &[Chunk],
    group_by: &[Expr],
    aggs: &[(AggregateFunction, Expr)],
    columns: &HashMap<String, usize>,
    scalar: &mut ScalarExpressions,
) -> DevonResult<Vec<AggregateInputRow>> {
    let mut rows = Vec::new();
    for chunk in chunks {
        let group_columns = group_by
            .iter()
            .map(|expr| scalar.evaluate(expr, chunk, columns))
            .collect::<DevonResult<Vec<_>>>()?;
        let aggregate_columns = evaluate_aggregate_columns(chunk, aggs, columns, scalar)?;
        append_aggregate_rows(&mut rows, chunk, &group_columns, &aggregate_columns)?;
    }
    Ok(rows)
}

fn evaluate_aggregate_columns(
    chunk: &Chunk,
    aggs: &[(AggregateFunction, Expr)],
    columns: &HashMap<String, usize>,
    scalar: &mut ScalarExpressions,
) -> DevonResult<Vec<Vec<Value>>> {
    aggs.iter()
        .map(|(_, expr)| scalar.evaluate(expr, chunk, columns))
        .collect()
}

fn update_streaming_chunk(
    states: &mut [AggregateState],
    aggs: &[(AggregateFunction, Expr)],
    chunk: &Chunk,
    columns: &HashMap<String, usize>,
    scalar: &mut ScalarExpressions,
) -> DevonResult<()> {
    for (state, (_, expression)) in states.iter_mut().zip(aggs) {
        if let Expr::Col(reference) = expression {
            let index = columns.get(reference).copied().ok_or_else(|| {
                invalid_argument(format!("unknown column reference `{reference}`"))
            })?;
            let column = chunk.column(index).ok_or_else(|| {
                invalid_argument(format!(
                    "column reference `{reference}` maps to out-of-range chunk column {index}"
                ))
            })?;
            update_typed_aggregate_column(state, column)?;
        } else {
            let values = scalar.evaluate(expression, chunk, columns)?;
            update_materialized_aggregate_column(state, &values, chunk.row_count())?;
        }
    }
    Ok(())
}

fn update_materialized_aggregate_column(
    state: &mut AggregateState,
    values: &[Value],
    row_count: usize,
) -> DevonResult<()> {
    for row in 0..row_count {
        let value = values.get(row).ok_or_else(|| {
            invalid_argument(format!(
                "aggregate expression returned fewer than {} rows",
                row + 1
            ))
        })?;
        update_aggregate_state(state, value)?;
    }
    Ok(())
}

fn append_aggregate_rows(
    rows: &mut Vec<AggregateInputRow>,
    chunk: &Chunk,
    group_columns: &[Vec<Value>],
    aggregate_columns: &[Vec<Value>],
) -> DevonResult<()> {
    for row in 0..chunk.row_count() {
        rows.push(AggregateInputRow {
            group_key: values_at(group_columns, row, "aggregate group expression")?,
            record: AggregateRecord::Input(values_at(
                aggregate_columns,
                row,
                "aggregate expression",
            )?),
        });
    }
    Ok(())
}

struct ChargedRows<T> {
    rows: Vec<T>,
    budget: Arc<MemoryBudget>,
    charged: usize,
}

impl<T> ChargedRows<T> {
    fn new(budget: Arc<MemoryBudget>) -> Self {
        Self {
            rows: Vec::new(),
            budget,
            charged: 0,
        }
    }

    fn push_charged(&mut self, row: T, bytes: usize) {
        self.charged += bytes;
        self.rows.push(row);
    }

    fn as_mut_vec(&mut self) -> &mut Vec<T> {
        &mut self.rows
    }

    fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    fn take_rows(&mut self) -> Vec<T> {
        let rows = std::mem::take(&mut self.rows);
        self.release();
        rows
    }

    fn release(&mut self) {
        self.budget.release(self.charged);
        self.charged = 0;
    }
}

impl<T> Drop for ChargedRows<T> {
    fn drop(&mut self) {
        self.release();
    }
}

struct SpillFiles {
    tmp_dir: PathBuf,
    operator_id: u64,
    paths: Vec<PathBuf>,
}

impl SpillFiles {
    fn new(tmp_dir: &Path) -> Self {
        Self {
            tmp_dir: tmp_dir.to_path_buf(),
            operator_id: NEXT_SPILL_OPERATOR_ID.fetch_add(1, AtomicOrdering::Relaxed),
            paths: Vec::new(),
        }
    }

    fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    fn paths(&self) -> Vec<PathBuf> {
        self.paths.clone()
    }

    fn write_run(&mut self, rows: impl IntoIterator<Item = Vec<Value>>) -> DevonResult<()> {
        self.write_generated_run(|writer| write_spill_rows(writer, rows))
    }

    fn write_generated_run(
        &mut self,
        write: impl FnOnce(&mut BufWriter<File>) -> DevonResult<()>,
    ) -> DevonResult<()> {
        fs::create_dir_all(&self.tmp_dir)?;
        let file_stem = format!(
            "sort-{}-{:016x}-{}-{}",
            std::process::id(),
            spill_process_token(),
            self.operator_id,
            self.paths.len()
        );
        let (path, file) = create_spill_run_file(&self.tmp_dir, &file_stem)?;
        let mut writer = BufWriter::new(file);
        let result = write(&mut writer).and_then(|()| writer.flush().map_err(Into::into));
        if let Err(error) = result {
            drop(writer);
            let _ = fs::remove_file(&path);
            return Err(error);
        }
        SPILL_RUNS_CREATED.fetch_add(1, AtomicOrdering::Relaxed);
        self.paths.push(path);
        Ok(())
    }
}

impl Drop for SpillFiles {
    fn drop(&mut self) {
        for path in &self.paths {
            let _ = fs::remove_file(path);
        }
    }
}

/// `create_new` guards against clobbering another handle's runs; a name
/// collision (a crashed incarnation's leftover under a recycled pid and a
/// coinciding token) retries under a disambiguating suffix instead of
/// killing the query.
fn create_spill_run_file(tmp_dir: &Path, file_stem: &str) -> DevonResult<(PathBuf, File)> {
    const MAX_ATTEMPTS: u32 = 16;
    for attempt in 0..MAX_ATTEMPTS {
        let file_name = if attempt == 0 {
            format!("{file_stem}.run")
        } else {
            format!("{file_stem}-r{attempt}.run")
        };
        let path = tmp_dir.join(file_name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    Err(std::io::Error::other(format!(
        "could not create a spill run file named from {file_stem} after {MAX_ATTEMPTS} attempts"
    ))
    .into())
}

fn spill_sort_run(
    rows: &mut ChargedRows<SortRow>,
    spill_files: &mut SpillFiles,
    keys: &[(Expr, SortOrder)],
) -> DevonResult<()> {
    if rows.is_empty() {
        return Ok(());
    }
    stable_sort_by(rows.as_mut_vec(), |left, right| {
        compare_sort_rows(left, right, keys)
    })?;
    spill_files.write_run(rows.rows.iter().map(|row| {
        let mut encoded = row.values.clone();
        encoded.extend(row.keys.iter().cloned());
        encoded
    }))?;
    drop(rows.take_rows());
    Ok(())
}

fn spill_aggregate_run(
    rows: &mut ChargedRows<AggregateInputRow>,
    spill_files: &mut SpillFiles,
    aggs: &[(AggregateFunction, Expr)],
) -> DevonResult<()> {
    if rows.is_empty() {
        return Ok(());
    }
    stable_sort_by(rows.as_mut_vec(), |left, right| {
        compare_key_tuples(&left.group_key, &right.group_key)
    })?;
    spill_files.write_run(rows.rows.iter().map(encode_aggregate_input_record))?;
    for (index, (function, _)) in aggs.iter().enumerate() {
        if *function == AggregateFunction::PercentileCont {
            spill_percentile_run(rows, spill_files, index)?;
        }
    }
    drop(rows.take_rows());
    Ok(())
}

fn encode_aggregate_input_record(row: &AggregateInputRow) -> Vec<Value> {
    let mut encoded = row.group_key.clone();
    encoded.push(Value::Int64(-1));
    if let AggregateRecord::Input(values) = &row.record {
        encoded.extend(values.iter().cloned());
    }
    encoded
}

fn spill_percentile_run(
    rows: &mut ChargedRows<AggregateInputRow>,
    spill_files: &mut SpillFiles,
    index: usize,
) -> DevonResult<()> {
    stable_sort_by(rows.as_mut_vec(), |left, right| {
        compare_percentile_rows(left, right, index)
    })?;
    spill_files.write_generated_run(|writer| {
        let mut start = 0;
        while start < rows.rows.len() {
            let end = group_end(&rows.rows, start)?;
            validate_percentile_rows(&rows.rows, start, end, index)?;
            write_percentile_group(writer, &rows.rows[start..end], index)?;
            start = end;
        }
        Ok(())
    })
}

fn write_percentile_group(
    writer: &mut impl Write,
    rows: &[AggregateInputRow],
    index: usize,
) -> DevonResult<()> {
    let count = rows
        .iter()
        .filter(|row| percentile_decimal(row, index).is_some())
        .count();
    let count = i64::try_from(count)
        .map_err(|_| invalid_argument("aggregate `percentile_cont` input count overflowed"))?;
    write_spill_row(
        writer,
        &encode_percentile_record(&rows[0].group_key, index, false, Value::Int64(count))?,
    )?;
    for row in rows {
        if let Some(value) = percentile_decimal(row, index) {
            write_spill_row(
                writer,
                &encode_percentile_record(&row.group_key, index, true, Value::Decimal(value))?,
            )?;
        }
    }
    Ok(())
}

fn encode_percentile_record(
    group_key: &[Value],
    index: usize,
    value_record: bool,
    value: Value,
) -> DevonResult<Vec<Value>> {
    let doubled = index
        .checked_mul(2)
        .and_then(|tag| tag.checked_add(usize::from(value_record)))
        .ok_or_else(|| invalid_argument("aggregate spill percentile tag overflowed usize"))?;
    let tag = i64::try_from(doubled)
        .map_err(|_| invalid_argument("aggregate spill percentile tag exceeds Int64"))?;
    let mut encoded = group_key.to_vec();
    encoded.extend([Value::Int64(tag), value]);
    Ok(encoded)
}

fn sort_row_charge(row: &SortRow) -> DevonResult<usize> {
    buffered_values_charge(row.values.iter().chain(&row.keys))
}

fn aggregate_row_charge(row: &AggregateInputRow) -> DevonResult<usize> {
    match &row.record {
        AggregateRecord::Input(values) => {
            buffered_values_charge(row.group_key.iter().chain(values))
        }
        AggregateRecord::PercentileCount { count, .. } => {
            let count = Value::Int64(*count);
            buffered_values_charge(row.group_key.iter().chain(std::iter::once(&count)))
        }
        AggregateRecord::PercentileValue { value, .. } => {
            let value = Value::Decimal(*value);
            buffered_values_charge(row.group_key.iter().chain(std::iter::once(&value)))
        }
    }
}

fn buffered_values_charge<'a>(values: impl IntoIterator<Item = &'a Value>) -> DevonResult<usize> {
    values
        .into_iter()
        .try_fold(BUFFERED_ROW_OVERHEAD_BYTES, |total, value| {
            total.checked_add(value.approx_bytes()).ok_or_else(|| {
                invalid_argument("blocking-operator row memory estimate exceeds usize::MAX")
            })
        })
}

fn charge_after_spill(budget: &MemoryBudget, requested: usize, category: &str) -> DevonResult<()> {
    if budget.charge_or_reclaim(requested) {
        return Ok(());
    }
    Err(DevonError::BudgetExceeded {
        context: format!(
            "{category} requests {requested} bytes with {} charged against a {} byte limit",
            budget.charged(),
            budget.limit()
        ),
    })
}

fn write_spill_rows(
    writer: &mut impl Write,
    rows: impl IntoIterator<Item = Vec<Value>>,
) -> DevonResult<()> {
    for row in rows {
        write_spill_row(writer, &row)?;
    }
    Ok(())
}

fn write_spill_row(writer: &mut impl Write, row: &[Value]) -> DevonResult<()> {
    write_spill_row_bounded(writer, row, MAX_SPILL_ROW_BYTES)
}

/// The write-side twin of `SpillReader::read_row`'s length cap. A row too
/// large to read back is rejected before any bytes are written. Spilling
/// therefore preserves query semantics, and `Corrupt` remains reserved for
/// actual corruption.
fn write_spill_row_bounded(
    writer: &mut impl Write,
    row: &[Value],
    max_row_bytes: usize,
) -> DevonResult<()> {
    let payload = encode_spill_row(row)?;
    let out_of_range = |encoded_bytes: usize| {
        invalid_argument(format!(
            "spill row is {encoded_bytes} encoded bytes but the spillable row range is \
             {MIN_SPILL_ROW_BYTES}..={max_row_bytes} bytes"
        ))
    };
    if !(MIN_SPILL_ROW_BYTES..=max_row_bytes).contains(&payload.len()) {
        return Err(out_of_range(payload.len()));
    }
    let row_len = u32::try_from(payload.len()).map_err(|_| out_of_range(payload.len()))?;
    writer.write_all(&row_len.to_le_bytes())?;
    writer.write_all(&payload)?;
    Ok(())
}

fn encode_spill_row(row: &[Value]) -> DevonResult<Vec<u8>> {
    let mut body = Vec::new();
    encode_spill_len(&mut body, row.len(), "spill row value count")?;
    for value in row {
        encode_spill_value(&mut body, value)?;
    }
    let body_len = u32::try_from(body.len())
        .map_err(|_| invalid_argument("spill row body exceeds u32::MAX bytes"))?;
    let capacity = SPILL_ROW_HEADER_BYTES
        .checked_add(body.len())
        .ok_or_else(|| invalid_argument("spill row encoded length exceeds usize::MAX"))?;
    let mut encoded = Vec::with_capacity(capacity);
    encoded.extend_from_slice(SPILL_ROW_MAGIC);
    encoded.extend_from_slice(&SPILL_ROW_FORMAT_VERSION.to_le_bytes());
    encoded.extend_from_slice(&body_len.to_le_bytes());
    encoded.extend_from_slice(&body);
    Ok(encoded)
}

fn decode_spill_row(payload: &[u8]) -> DevonResult<Vec<Value>> {
    let mut decoder = SpillRowDecoder::new(payload);
    if decoder.take(SPILL_ROW_MAGIC.len())? != SPILL_ROW_MAGIC {
        return Err(corrupt_spill_row("bad spill-row header magic"));
    }
    let version = decoder.read_u16()?;
    if version != SPILL_ROW_FORMAT_VERSION {
        return Err(corrupt_spill_row(format!(
            "unsupported spill-row format version {version}"
        )));
    }
    let body_len = decoder.read_u32()? as usize;
    if decoder.remaining_len() != body_len {
        return Err(corrupt_spill_row(format!(
            "spill-row header declares {body_len} body bytes but {} remain",
            decoder.remaining_len()
        )));
    }
    decode_spill_row_body(decoder.take(body_len)?)
}

fn encode_spill_len(encoded: &mut Vec<u8>, len: usize, kind: &str) -> DevonResult<()> {
    let len =
        u32::try_from(len).map_err(|_| invalid_argument(format!("{kind} exceeds u32::MAX")))?;
    encoded.extend_from_slice(&len.to_le_bytes());
    Ok(())
}

fn encode_spill_bytes(encoded: &mut Vec<u8>, bytes: &[u8], kind: &str) -> DevonResult<()> {
    encode_spill_len(encoded, bytes.len(), kind)?;
    encoded.extend_from_slice(bytes);
    Ok(())
}

fn encode_spill_value(encoded: &mut Vec<u8>, value: &Value) -> DevonResult<()> {
    match value {
        Value::Null => encoded.push(0),
        Value::Bool(value) => encoded.extend([1, u8::from(*value)]),
        Value::Int64(value) => encode_fixed(encoded, 2, &value.to_le_bytes()),
        Value::Float64(value) => encode_fixed(encoded, 3, &value.to_bits().to_le_bytes()),
        Value::String(value) => {
            encoded.push(4);
            encode_spill_bytes(encoded, value.as_bytes(), "spill String length")?;
        }
        Value::Vector(value) => encode_spill_vector(encoded, value)?,
        Value::GeoPoint(value) => {
            encoded.push(6);
            encoded.extend_from_slice(&value.lat_deg().to_bits().to_le_bytes());
            encoded.extend_from_slice(&value.lng_deg().to_bits().to_le_bytes());
        }
        Value::Timestamp(value) => encode_fixed(encoded, 7, &value.to_le_bytes()),
        Value::Bytes(value) => {
            encoded.push(8);
            encode_spill_bytes(encoded, value, "spill Bytes length")?;
        }
        Value::Decimal(value) => {
            encoded.push(9);
            encoded.extend_from_slice(&value.digits().to_le_bytes());
            encoded.push(value.scale());
        }
        Value::Json(value) => {
            encoded.push(10);
            encode_spill_bytes(encoded, value.as_bytes(), "spill Json length")?;
        }
    }
    Ok(())
}

fn encode_fixed(encoded: &mut Vec<u8>, tag: u8, bytes: &[u8]) {
    encoded.push(tag);
    encoded.extend_from_slice(bytes);
}

fn encode_spill_vector(encoded: &mut Vec<u8>, vector: &[f32]) -> DevonResult<()> {
    encoded.push(5);
    encode_spill_len(encoded, vector.len(), "spill Vector element count")?;
    for value in vector {
        encoded.extend_from_slice(&value.to_bits().to_le_bytes());
    }
    Ok(())
}

fn decode_spill_row_body(body: &[u8]) -> DevonResult<Vec<Value>> {
    let mut decoder = SpillRowDecoder::new(body);
    let value_count = decoder.read_u32()? as usize;
    if value_count > decoder.remaining_len() {
        return Err(corrupt_spill_row(format!(
            "spill row declares {value_count} values in only {} bytes",
            decoder.remaining_len()
        )));
    }
    let mut row = Vec::with_capacity(value_count);
    for _ in 0..value_count {
        row.push(decode_spill_value(&mut decoder)?);
    }
    if decoder.remaining_len() != 0 {
        return Err(corrupt_spill_row(format!(
            "spill row has {} trailing bytes",
            decoder.remaining_len()
        )));
    }
    Ok(row)
}

fn decode_spill_value(decoder: &mut SpillRowDecoder<'_>) -> DevonResult<Value> {
    match decoder.read_u8()? {
        0 => Ok(Value::Null),
        1 => decode_spill_bool(decoder),
        2 => decoder.read_i64().map(Value::Int64),
        3 => decoder.read_u64().map(f64::from_bits).map(Value::Float64),
        4 => decoder.read_text("String").map(Value::String),
        5 => decode_spill_vector(decoder).map(Value::Vector),
        6 => decode_spill_geo_point(decoder).map(Value::GeoPoint),
        7 => decoder.read_i64().map(Value::Timestamp),
        8 => decoder.read_blob("Bytes").map(Value::Bytes),
        9 => decode_spill_decimal(decoder).map(Value::Decimal),
        10 => decoder.read_text("Json").map(Value::Json),
        tag => Err(corrupt_spill_row(format!("unknown spill value tag {tag}"))),
    }
}

fn decode_spill_bool(decoder: &mut SpillRowDecoder<'_>) -> DevonResult<Value> {
    match decoder.read_u8()? {
        0 => Ok(Value::Bool(false)),
        1 => Ok(Value::Bool(true)),
        value => Err(corrupt_spill_row(format!(
            "spill Bool byte must be 0 or 1, got {value}"
        ))),
    }
}

fn decode_spill_vector(decoder: &mut SpillRowDecoder<'_>) -> DevonResult<Vec<f32>> {
    let count = decoder.read_u32()? as usize;
    let byte_len = count
        .checked_mul(size_of::<u32>())
        .ok_or_else(|| corrupt_spill_row("spill Vector byte length overflowed usize"))?;
    let bytes = decoder.take(byte_len)?;
    Ok(bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|bytes| f32::from_bits(u32::from_le_bytes(*bytes)))
        .collect())
}

fn decode_spill_geo_point(
    decoder: &mut SpillRowDecoder<'_>,
) -> DevonResult<devondb_types::GeoPoint> {
    let lat_deg = f64::from_bits(decoder.read_u64()?);
    let lng_deg = f64::from_bits(decoder.read_u64()?);
    devondb_types::GeoPoint::from_canonical(lat_deg, lng_deg)
        .map_err(|error| corrupt_spill_row(format!("invalid spill GeoPoint: {error}")))
}

fn decode_spill_decimal(decoder: &mut SpillRowDecoder<'_>) -> DevonResult<Decimal128> {
    let digits = decoder.read_i128()?;
    let scale = decoder.read_u8()?;
    Decimal128::new(digits, scale)
        .map_err(|error| corrupt_spill_row(format!("invalid spill Decimal: {error}")))
}

struct SpillRowDecoder<'a> {
    remaining: &'a [u8],
}

impl<'a> SpillRowDecoder<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { remaining: bytes }
    }

    fn remaining_len(&self) -> usize {
        self.remaining.len()
    }

    fn take(&mut self, len: usize) -> DevonResult<&'a [u8]> {
        if len > self.remaining.len() {
            return Err(corrupt_spill_row(format!(
                "short spill row: need {len} bytes, only {} remain",
                self.remaining.len()
            )));
        }
        let (value, remaining) = self.remaining.split_at(len);
        self.remaining = remaining;
        Ok(value)
    }

    fn read_array<const N: usize>(&mut self) -> DevonResult<[u8; N]> {
        self.take(N)?
            .try_into()
            .map_err(|_| corrupt_spill_row("spill scalar has the wrong width"))
    }

    fn read_u8(&mut self) -> DevonResult<u8> {
        Ok(self.read_array::<1>()?[0])
    }

    fn read_u16(&mut self) -> DevonResult<u16> {
        self.read_array().map(u16::from_le_bytes)
    }

    fn read_u32(&mut self) -> DevonResult<u32> {
        self.read_array().map(u32::from_le_bytes)
    }

    fn read_u64(&mut self) -> DevonResult<u64> {
        self.read_array().map(u64::from_le_bytes)
    }

    fn read_i64(&mut self) -> DevonResult<i64> {
        self.read_array().map(i64::from_le_bytes)
    }

    fn read_i128(&mut self) -> DevonResult<i128> {
        self.read_array().map(i128::from_le_bytes)
    }

    fn read_blob(&mut self, kind: &str) -> DevonResult<Vec<u8>> {
        let len = self.read_u32()? as usize;
        self.take(len)
            .map(<[u8]>::to_vec)
            .map_err(|error| corrupt_spill_row(format!("malformed spill {kind} payload: {error}")))
    }

    fn read_text(&mut self, kind: &str) -> DevonResult<String> {
        String::from_utf8(self.read_blob(kind)?).map_err(|error| {
            corrupt_spill_row(format!("spill {kind} payload is not UTF-8: {error}"))
        })
    }
}

fn corrupt_spill_row(context: impl Into<String>) -> DevonError {
    DevonError::Corrupt {
        context: context.into(),
    }
}

/// Buffered-read size for spill runs: big enough to absorb the 4-byte
/// header + payload syscall pairs even when the row buffer is small
/// without retaining additional rows.
const SPILL_READ_BUFFER_BYTES: usize = 64 * 1024;

struct SpillReader {
    file: BufReader<File>,
    path: PathBuf,
}

impl SpillReader {
    fn open(path: PathBuf) -> DevonResult<Self> {
        let file = BufReader::with_capacity(SPILL_READ_BUFFER_BYTES, File::open(&path)?);
        Ok(Self { file, path })
    }

    fn read_row(&mut self) -> DevonResult<Option<Vec<Value>>> {
        let mut header = [0_u8; 4];
        if self.file.read(&mut header[..1])? == 0 {
            return Ok(None);
        }
        read_exact_spill(&mut self.file, &mut header[1..], &self.path, "row length")?;
        let row_len = u32::from_le_bytes(header) as usize;
        if !(MIN_SPILL_ROW_BYTES..=MAX_SPILL_ROW_BYTES).contains(&row_len) {
            return Err(corrupt_spill(
                &self.path,
                format!("bad spill row length {row_len}"),
            ));
        }
        let position = self.file.stream_position()?;
        let remaining = self
            .file
            .get_ref()
            .metadata()?
            .len()
            .saturating_sub(position);
        if row_len as u64 > remaining {
            return Err(corrupt_spill(
                &self.path,
                format!("short spill row: length is {row_len}, only {remaining} bytes remain"),
            ));
        }
        let mut payload = vec![0; row_len];
        read_exact_spill(&mut self.file, &mut payload, &self.path, "row payload")?;
        decode_spill_row(&payload)
            .map(Some)
            .map_err(|error| match error {
                DevonError::Corrupt { context } => corrupt_spill(&self.path, context),
                other => other,
            })
    }
}

fn read_exact_spill(
    reader: &mut impl Read,
    bytes: &mut [u8],
    path: &Path,
    part: &str,
) -> DevonResult<()> {
    reader.read_exact(bytes).map_err(|error| {
        if error.kind() == ErrorKind::UnexpectedEof {
            corrupt_spill(path, format!("short read while reading {part}"))
        } else {
            DevonError::Io(error)
        }
    })
}

fn corrupt_spill(path: &Path, context: impl Into<String>) -> DevonError {
    DevonError::Corrupt {
        context: format!("spill file `{}`: {}", path.display(), context.into()),
    }
}

struct ChargedRunRow<T> {
    row: T,
    bytes: usize,
}

struct RunCursor<T> {
    reader: SpillReader,
    buffered: VecDeque<ChargedRunRow<T>>,
    budget: Arc<MemoryBudget>,
    buffer_rows: usize,
    exhausted: bool,
}

impl<T> RunCursor<T> {
    fn open(path: PathBuf, budget: Arc<MemoryBudget>, buffer_rows: usize) -> DevonResult<Self> {
        Ok(Self {
            reader: SpillReader::open(path)?,
            buffered: VecDeque::new(),
            budget,
            buffer_rows,
            exhausted: false,
        })
    }

    fn ensure_buffered(
        &mut self,
        mut decode: impl FnMut(Vec<Value>) -> DevonResult<T>,
        mut charge: impl FnMut(&T) -> DevonResult<usize>,
    ) -> DevonResult<()> {
        if !self.buffered.is_empty() || self.exhausted {
            return Ok(());
        }
        for _ in 0..self.buffer_rows {
            let Some(values) = self.reader.read_row()? else {
                self.exhausted = true;
                break;
            };
            let row = decode(values)?;
            let bytes = charge(&row)?;
            if !self.budget.charge_or_reclaim(bytes) {
                return Err(DevonError::BudgetExceeded {
                    context: format!(
                        "spill merge buffer requests {bytes} bytes with {} charged against a {} byte limit",
                        self.budget.charged(),
                        self.budget.limit()
                    ),
                });
            }
            self.buffered.push_back(ChargedRunRow { row, bytes });
        }
        Ok(())
    }

    fn front(&self) -> Option<&T> {
        self.buffered.front().map(|row| &row.row)
    }

    fn pop_front(&mut self) -> Option<T> {
        let row = self.buffered.pop_front()?;
        self.budget.release(row.bytes);
        Some(row.row)
    }
}

impl<T> Drop for RunCursor<T> {
    fn drop(&mut self) {
        let charged = self
            .buffered
            .iter()
            .fold(0_usize, |total, row| total.saturating_add(row.bytes));
        self.budget.release(charged);
    }
}

struct SortMerge {
    types: Vec<LogicalType>,
    keys: Vec<(Expr, SortOrder)>,
    value_count: usize,
    cursors: Vec<RunCursor<SortRow>>,
}

impl SortMerge {
    fn new(
        types: Vec<LogicalType>,
        keys: Vec<(Expr, SortOrder)>,
        paths: Vec<PathBuf>,
        budget: Arc<MemoryBudget>,
    ) -> DevonResult<Self> {
        let buffer_rows = merge_buffer_rows(paths.len());
        let cursors = paths
            .into_iter()
            .map(|path| RunCursor::open(path, Arc::clone(&budget), buffer_rows))
            .collect::<DevonResult<Vec<_>>>()?;
        Ok(Self {
            value_count: types.len(),
            types,
            keys,
            cursors,
        })
    }

    fn next_chunk(&mut self) -> DevonResult<Option<Chunk>> {
        let mut builder = ChunkBuilder::new(self.types.clone());
        let mut row_count = 0;
        while !builder.is_full() {
            let Some(row) = self.take_next_row()? else {
                break;
            };
            builder.push_row(row.values)?;
            row_count += 1;
        }
        Ok((row_count != 0).then(|| builder.finish()))
    }

    fn take_next_row(&mut self) -> DevonResult<Option<SortRow>> {
        let value_count = self.value_count;
        let key_count = self.keys.len();
        for cursor in &mut self.cursors {
            cursor.ensure_buffered(
                |row| decode_sort_spill_row(row, value_count, key_count),
                sort_row_charge,
            )?;
        }
        let mut best: Option<usize> = None;
        for index in 0..self.cursors.len() {
            let Some(candidate) = self.cursors[index].front() else {
                continue;
            };
            if let Some(best_index) = best {
                let best_row = self.cursors[best_index]
                    .front()
                    .ok_or_else(invalid_sort_permutation)?;
                if compare_sort_rows(candidate, best_row, &self.keys)? == Ordering::Less {
                    best = Some(index);
                }
            } else {
                best = Some(index);
            }
        }
        best.map_or(Ok(None), |index| {
            self.cursors[index]
                .pop_front()
                .ok_or_else(invalid_sort_permutation)
                .map(Some)
        })
    }
}

fn decode_sort_spill_row(
    mut row: Vec<Value>,
    value_count: usize,
    key_count: usize,
) -> DevonResult<SortRow> {
    if row.len() != value_count.saturating_add(key_count) {
        return Err(DevonError::Corrupt {
            context: format!(
                "spill sort row has {} values, expected {} row values and {key_count} keys",
                row.len(),
                value_count
            ),
        });
    }
    let keys = row.split_off(value_count);
    Ok(SortRow { values: row, keys })
}

struct AggregateMerge {
    output_types: Vec<LogicalType>,
    group_count: usize,
    aggs: Vec<(AggregateFunction, Expr)>,
    cursors: Vec<RunCursor<AggregateInputRow>>,
}

impl AggregateMerge {
    fn new(
        output_types: Vec<LogicalType>,
        group_count: usize,
        aggs: Vec<(AggregateFunction, Expr)>,
        paths: Vec<PathBuf>,
        budget: Arc<MemoryBudget>,
    ) -> DevonResult<Self> {
        let buffer_rows = merge_buffer_rows(paths.len());
        let cursors = paths
            .into_iter()
            .map(|path| RunCursor::open(path, Arc::clone(&budget), buffer_rows))
            .collect::<DevonResult<Vec<_>>>()?;
        Ok(Self {
            output_types,
            group_count,
            aggs,
            cursors,
        })
    }

    fn next_chunk(&mut self) -> DevonResult<Option<Chunk>> {
        let mut builder = ChunkBuilder::new(self.output_types.clone());
        let mut row_count = 0;
        while !builder.is_full() {
            let Some(row) = self.next_group()? else {
                break;
            };
            builder.push_row(row)?;
            row_count += 1;
        }
        Ok((row_count != 0).then(|| builder.finish()))
    }

    fn next_group(&mut self) -> DevonResult<Option<Vec<Value>>> {
        let Some(first) = self.take_next_input()? else {
            return Ok(None);
        };
        let key = first.group_key;
        let mut states = self
            .aggs
            .iter()
            .map(|(function, _)| initial_aggregate_state(*function))
            .collect::<DevonResult<Vec<_>>>()?;
        update_aggregate_states(&mut states, &first.record)?;
        while let Some(index) = self.best_cursor()? {
            let Some(next) = self.cursors[index].front() else {
                return Err(invalid_sort_permutation());
            };
            if compare_key_tuples(&key, &next.group_key)? != Ordering::Equal {
                break;
            }
            let next = self.cursors[index]
                .pop_front()
                .ok_or_else(invalid_sort_permutation)?;
            update_aggregate_states(&mut states, &next.record)?;
        }
        let mut output = key;
        output.extend(finish_aggregate_states(
            states,
            &self.output_types[self.group_count..],
        )?);
        Ok(Some(output))
    }

    fn take_next_input(&mut self) -> DevonResult<Option<AggregateInputRow>> {
        let Some(index) = self.best_cursor()? else {
            return Ok(None);
        };
        self.cursors[index]
            .pop_front()
            .ok_or_else(invalid_sort_permutation)
            .map(Some)
    }

    fn best_cursor(&mut self) -> DevonResult<Option<usize>> {
        let group_count = self.group_count;
        for cursor in &mut self.cursors {
            cursor.ensure_buffered(
                |row| decode_aggregate_spill_row(row, group_count, &self.aggs),
                aggregate_row_charge,
            )?;
        }
        let mut best: Option<usize> = None;
        for index in 0..self.cursors.len() {
            let Some(candidate) = self.cursors[index].front() else {
                continue;
            };
            if let Some(best_index) = best {
                let best_row = self.cursors[best_index]
                    .front()
                    .ok_or_else(invalid_sort_permutation)?;
                if compare_aggregate_records(candidate, best_row)? == Ordering::Less {
                    best = Some(index);
                }
            } else {
                best = Some(index);
            }
        }
        Ok(best)
    }
}

fn merge_buffer_rows(run_count: usize) -> usize {
    // Keep one charged front row per run. Chunk-deep row buffers were slower
    // than `SpillReader`'s buffered byte reads alone (449 ms versus 362 ms
    // when merging 294 runs and 200k rows), and their unreclaimable charges
    // can starve a peer run's mandatory front row.
    usize::from(run_count != 0)
}

fn compare_aggregate_records(
    left: &AggregateInputRow,
    right: &AggregateInputRow,
) -> DevonResult<Ordering> {
    let group = compare_key_tuples(&left.group_key, &right.group_key)?;
    if group != Ordering::Equal {
        return Ok(group);
    }
    let rank = aggregate_record_rank(&left.record).cmp(&aggregate_record_rank(&right.record));
    if rank != Ordering::Equal {
        return Ok(rank);
    }
    match (&left.record, &right.record) {
        (
            AggregateRecord::PercentileValue { value: left, .. },
            AggregateRecord::PercentileValue { value: right, .. },
        ) => Ok(left.digits().cmp(&right.digits())),
        _ => Ok(Ordering::Equal),
    }
}

fn aggregate_record_rank(record: &AggregateRecord) -> usize {
    match record {
        AggregateRecord::Input(_) => 0,
        AggregateRecord::PercentileCount { index, .. } => index.saturating_mul(2).saturating_add(1),
        AggregateRecord::PercentileValue { index, .. } => index.saturating_mul(2).saturating_add(2),
    }
}

fn decode_aggregate_spill_row(
    mut row: Vec<Value>,
    group_count: usize,
    aggs: &[(AggregateFunction, Expr)],
) -> DevonResult<AggregateInputRow> {
    if row.len() < group_count.saturating_add(2) {
        return Err(DevonError::Corrupt {
            context: format!(
                "spill aggregate row has {} values, expected {group_count} group keys, a tag, and a payload",
                row.len()
            ),
        });
    }
    let payload = row.split_off(group_count);
    let record = decode_aggregate_record(payload, aggs)?;
    Ok(AggregateInputRow {
        group_key: row,
        record,
    })
}

fn decode_aggregate_record(
    mut payload: Vec<Value>,
    aggs: &[(AggregateFunction, Expr)],
) -> DevonResult<AggregateRecord> {
    let tag = match payload.first() {
        Some(Value::Int64(tag)) => *tag,
        _ => return Err(corrupt_aggregate_spill("record tag is not Int64")),
    };
    payload.remove(0);
    if tag == -1 {
        if payload.len() != aggs.len() {
            return Err(corrupt_aggregate_spill(format!(
                "input record has {} values, expected {}",
                payload.len(),
                aggs.len()
            )));
        }
        return Ok(AggregateRecord::Input(payload));
    }
    let tag = usize::try_from(tag)
        .map_err(|_| corrupt_aggregate_spill(format!("invalid negative record tag {tag}")))?;
    let index = tag / 2;
    if aggs.get(index).map(|agg| agg.0) != Some(AggregateFunction::PercentileCont) {
        return Err(corrupt_aggregate_spill(format!(
            "record tag {tag} does not name a percentile_cont aggregate"
        )));
    }
    let [value] = payload.as_slice() else {
        return Err(corrupt_aggregate_spill(format!(
            "percentile record has {} payload values, expected 1",
            payload.len()
        )));
    };
    if tag % 2 == 0 {
        let Value::Int64(count) = value else {
            return Err(corrupt_aggregate_spill(
                "percentile count record is not Int64",
            ));
        };
        if *count < 0 {
            return Err(corrupt_aggregate_spill(format!(
                "percentile count is negative: {count}"
            )));
        }
        Ok(AggregateRecord::PercentileCount {
            index,
            count: *count,
        })
    } else {
        let Value::Decimal(value) = value else {
            return Err(corrupt_aggregate_spill(
                "percentile value record is not Decimal",
            ));
        };
        Ok(AggregateRecord::PercentileValue {
            index,
            value: *value,
        })
    }
}

fn corrupt_aggregate_spill(context: impl Into<String>) -> DevonError {
    DevonError::Corrupt {
        context: format!("spill aggregate row {}", context.into()),
    }
}

fn update_aggregate_states(
    states: &mut [AggregateState],
    record: &AggregateRecord,
) -> DevonResult<()> {
    match record {
        AggregateRecord::Input(values) => {
            for (state, value) in states.iter_mut().zip(values) {
                if !matches!(state, AggregateState::Percentile { .. }) {
                    update_aggregate_state(state, value)?;
                }
            }
            Ok(())
        }
        AggregateRecord::PercentileCount { index, count } => {
            let state = states
                .get_mut(*index)
                .ok_or_else(|| corrupt_aggregate_spill("percentile state index is out of range"))?;
            update_percentile_count(state, *count)
        }
        AggregateRecord::PercentileValue { index, value } => {
            let state = states
                .get_mut(*index)
                .ok_or_else(|| corrupt_aggregate_spill("percentile state index is out of range"))?;
            update_percentile_value(state, *value)
        }
    }
}

fn aggregate_groups(
    rows: &mut [AggregateInputRow],
    group_count: usize,
    aggs: &[(AggregateFunction, Expr)],
    output_types: &[LogicalType],
) -> DevonResult<Vec<Vec<Value>>> {
    if rows.is_empty() {
        return if group_count == 0 {
            Ok(vec![aggregate_group(&mut [], aggs, output_types)?])
        } else {
            Ok(Vec::new())
        };
    }

    let mut output = Vec::new();
    let mut start = 0;
    while start < rows.len() {
        let end = group_end(rows, start)?;
        let mut output_row = rows[start].group_key.clone();
        output_row.extend(aggregate_group(
            &mut rows[start..end],
            aggs,
            &output_types[group_count..],
        )?);
        output.push(output_row);
        start = end;
    }
    Ok(output)
}

fn group_end(rows: &[AggregateInputRow], start: usize) -> DevonResult<usize> {
    let mut end = start + 1;
    while end < rows.len()
        && compare_key_tuples(&rows[start].group_key, &rows[end].group_key)? == Ordering::Equal
    {
        end += 1;
    }
    Ok(end)
}

fn aggregate_group(
    rows: &mut [AggregateInputRow],
    aggs: &[(AggregateFunction, Expr)],
    output_types: &[LogicalType],
) -> DevonResult<Vec<Value>> {
    let mut states = aggs
        .iter()
        .map(|(function, _)| initial_aggregate_state(*function))
        .collect::<DevonResult<Vec<_>>>()?;
    for row in rows.iter() {
        update_aggregate_states(&mut states, &row.record)?;
    }
    for (index, (function, _)) in aggs.iter().enumerate() {
        if *function == AggregateFunction::PercentileCont {
            update_in_memory_percentile(&mut states, rows, index)?;
        }
    }
    finish_aggregate_states(states, output_types)
}

fn initial_aggregate_state(function: AggregateFunction) -> DevonResult<AggregateState> {
    match function {
        AggregateFunction::Count => Ok(AggregateState::Count(0)),
        AggregateFunction::Sum => Ok(AggregateState::Sum(None)),
        AggregateFunction::Min => Ok(AggregateState::Min(None)),
        AggregateFunction::Max => Ok(AggregateState::Max(None)),
        AggregateFunction::Avg => Ok(AggregateState::Avg {
            sum: None,
            count: 0,
        }),
        AggregateFunction::PercentileCont => Ok(AggregateState::Percentile {
            expected: 0,
            seen: 0,
            scale: None,
            lower: None,
            upper: None,
        }),
    }
}

fn update_in_memory_percentile(
    states: &mut [AggregateState],
    rows: &mut [AggregateInputRow],
    index: usize,
) -> DevonResult<()> {
    rows.sort_unstable_by(|left, right| compare_percentile_values(left, right, index));
    let count = rows
        .iter()
        .filter(|row| percentile_decimal(row, index).is_some())
        .count();
    let count = i64::try_from(count)
        .map_err(|_| invalid_argument("aggregate `percentile_cont` input count overflowed"))?;
    let state = states.get_mut(index).ok_or_else(|| DevonError::Corrupt {
        context: "percentile_cont state index is out of range".into(),
    })?;
    validate_percentile_rows(rows, 0, rows.len(), index)?;
    update_percentile_count(state, count)?;
    for value in rows.iter().filter_map(|row| percentile_decimal(row, index)) {
        update_percentile_value(state, value)?;
    }
    Ok(())
}

fn validate_percentile_rows(
    rows: &[AggregateInputRow],
    start: usize,
    end: usize,
    index: usize,
) -> DevonResult<()> {
    let mut scale = None;
    for row in &rows[start..end] {
        let AggregateRecord::Input(values) = &row.record else {
            return Err(DevonError::Corrupt {
                context: "in-memory aggregate contains a spill-only record".into(),
            });
        };
        let value = values.get(index).ok_or_else(|| DevonError::Corrupt {
            context: format!("aggregate input row is missing value {index}"),
        })?;
        match value {
            Value::Null => {}
            Value::Decimal(value) => match scale {
                None => scale = Some(value.scale()),
                Some(expected) if expected == value.scale() => {}
                Some(expected) => {
                    return Err(DevonError::Corrupt {
                        context: format!(
                            "aggregate `percentile_cont` cannot combine Decimal values with scales {expected} and {}",
                            value.scale()
                        ),
                    });
                }
            },
            value => return Err(invalid_aggregate_type("percentile_cont", value)),
        }
    }
    Ok(())
}

fn compare_percentile_rows(
    left: &AggregateInputRow,
    right: &AggregateInputRow,
    index: usize,
) -> DevonResult<Ordering> {
    let group = compare_key_tuples(&left.group_key, &right.group_key)?;
    if group != Ordering::Equal {
        return Ok(group);
    }
    Ok(compare_percentile_values(left, right, index))
}

fn compare_percentile_values(
    left: &AggregateInputRow,
    right: &AggregateInputRow,
    index: usize,
) -> Ordering {
    match (
        percentile_decimal(left, index),
        percentile_decimal(right, index),
    ) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(left), Some(right)) => left.digits().cmp(&right.digits()),
    }
}

fn percentile_decimal(row: &AggregateInputRow, index: usize) -> Option<Decimal128> {
    let AggregateRecord::Input(values) = &row.record else {
        return None;
    };
    match values.get(index) {
        Some(Value::Decimal(value)) => Some(*value),
        _ => None,
    }
}

fn update_percentile_count(state: &mut AggregateState, count: i64) -> DevonResult<()> {
    let AggregateState::Percentile { expected, .. } = state else {
        return Err(corrupt_aggregate_spill(
            "percentile count names a non-percentile state",
        ));
    };
    let count = u64::try_from(count)
        .map_err(|_| corrupt_aggregate_spill(format!("negative percentile count {count}")))?;
    *expected = expected
        .checked_add(count)
        .ok_or_else(|| invalid_argument("aggregate `percentile_cont` input count overflowed"))?;
    Ok(())
}

fn update_percentile_value(state: &mut AggregateState, value: Decimal128) -> DevonResult<()> {
    let AggregateState::Percentile {
        expected,
        seen,
        scale,
        lower,
        upper,
    } = state
    else {
        return Err(corrupt_aggregate_spill(
            "percentile value names a non-percentile state",
        ));
    };
    match *scale {
        None => *scale = Some(value.scale()),
        Some(expected_scale) if expected_scale == value.scale() => {}
        Some(expected_scale) => {
            return Err(DevonError::Corrupt {
                context: format!(
                    "aggregate `percentile_cont` cannot combine Decimal values with scales {expected_scale} and {}",
                    value.scale()
                ),
            });
        }
    }
    if *seen >= *expected {
        return Err(corrupt_aggregate_spill(
            "contains more percentile values than its count records",
        ));
    }
    let lower_index = expected.saturating_sub(1) / 2;
    let upper_index = *expected / 2;
    if *seen == lower_index {
        *lower = Some(value);
    }
    if *seen == upper_index {
        *upper = Some(value);
    }
    *seen += 1;
    Ok(())
}

fn update_aggregate_state(state: &mut AggregateState, value: &Value) -> DevonResult<()> {
    match state {
        AggregateState::Count(count) => update_count(count, value),
        AggregateState::Sum(sum) => update_sum(sum, value),
        AggregateState::Min(minimum) => update_extreme(minimum, value, Ordering::Less),
        AggregateState::Max(maximum) => update_extreme(maximum, value, Ordering::Greater),
        AggregateState::Avg { sum, count } => update_avg(sum, count, value),
        AggregateState::Percentile { .. } => Err(DevonError::Corrupt {
            context: "percentile_cont state received an unordered input record".into(),
        }),
    }
}

fn update_typed_aggregate_column(state: &mut AggregateState, column: &Column) -> DevonResult<()> {
    match column {
        Column::Int64 { values, validity } => {
            update_valid_values(values, validity, |value| update_int64_state(state, value))
        }
        Column::Float64 { values, validity } => {
            update_valid_values(values, validity, |value| update_float64_state(state, value))
        }
        Column::Bool { values, validity } => {
            update_valid_values(values, validity, |value| update_bool_state(state, value))
        }
        Column::Timestamp { values, validity } => update_valid_values(values, validity, |value| {
            update_timestamp_state(state, value)
        }),
        Column::Decimal {
            values,
            scale,
            validity,
        } => update_valid_values(values, validity, |digits| {
            update_decimal_state(state, Decimal128::new(digits, *scale)?)
        }),
        Column::Boxed(values) => {
            for value in values {
                update_aggregate_state(state, value)?;
            }
            Ok(())
        }
    }
}

fn update_valid_values<T: Copy>(
    values: &[T],
    validity: &Option<Bitmap>,
    mut update: impl FnMut(T) -> DevonResult<()>,
) -> DevonResult<()> {
    for (row, value) in values.iter().copied().enumerate() {
        if is_valid(validity, row) {
            update(value)?;
        }
    }
    Ok(())
}

fn is_valid(validity: &Option<Bitmap>, row: usize) -> bool {
    validity.as_ref().is_none_or(|bitmap| bitmap.is_valid(row))
}

fn update_grouped_typed_column(
    groups: &mut [TypedGroup],
    group_indices: &[usize],
    state_index: usize,
    column: &Column,
) -> DevonResult<()> {
    match column {
        Column::Int64 { values, validity } => update_grouped_valid_values(
            groups,
            group_indices,
            state_index,
            values,
            validity,
            update_int64_state,
        ),
        Column::Float64 { values, validity } => update_grouped_valid_values(
            groups,
            group_indices,
            state_index,
            values,
            validity,
            update_float64_state,
        ),
        Column::Bool { values, validity } => update_grouped_valid_values(
            groups,
            group_indices,
            state_index,
            values,
            validity,
            update_bool_state,
        ),
        Column::Timestamp { values, validity } => update_grouped_valid_values(
            groups,
            group_indices,
            state_index,
            values,
            validity,
            update_timestamp_state,
        ),
        Column::Decimal {
            values,
            scale,
            validity,
        } => update_grouped_valid_values(
            groups,
            group_indices,
            state_index,
            values,
            validity,
            |state, digits| update_decimal_state(state, Decimal128::new(digits, *scale)?),
        ),
        Column::Boxed(_) => Err(invalid_argument(
            "typed aggregate accumulation received a Boxed column",
        )),
    }
}

fn update_grouped_valid_values<T: Copy>(
    groups: &mut [TypedGroup],
    group_indices: &[usize],
    state_index: usize,
    values: &[T],
    validity: &Option<Bitmap>,
    mut update: impl FnMut(&mut AggregateState, T) -> DevonResult<()>,
) -> DevonResult<()> {
    for (row, value) in values.iter().copied().enumerate() {
        if !is_valid(validity, row) {
            continue;
        }
        let group_index = group_indices
            .get(row)
            .copied()
            .ok_or_else(|| DevonError::Corrupt {
                context: format!("typed aggregate row {row} has no group index"),
            })?;
        let state = groups
            .get_mut(group_index)
            .and_then(|group| group.states.get_mut(state_index))
            .ok_or_else(|| DevonError::Corrupt {
                context: format!("typed aggregate group {group_index} has no state {state_index}"),
            })?;
        update(state, value)?;
    }
    Ok(())
}

fn update_int64_state(state: &mut AggregateState, value: i64) -> DevonResult<()> {
    match state {
        AggregateState::Count(count) => increment_count(count),
        AggregateState::Sum(sum) => update_int64_sum(sum, value, "sum"),
        AggregateState::Min(minimum) => {
            update_extreme(minimum, &Value::Int64(value), Ordering::Less)
        }
        AggregateState::Max(maximum) => {
            update_extreme(maximum, &Value::Int64(value), Ordering::Greater)
        }
        AggregateState::Avg { sum, count } => {
            update_int64_sum(sum, value, "avg")?;
            increment_avg_count(count)
        }
        AggregateState::Percentile { .. } => invalid_typed_percentile(),
    }
}

fn update_int64_sum(sum: &mut Option<Value>, value: i64, function: &str) -> DevonResult<()> {
    match sum {
        None => *sum = Some(Value::Int64(value)),
        Some(Value::Int64(total)) => {
            *total = total.checked_add(value).ok_or_else(|| {
                invalid_argument(format!(
                    "aggregate `{function}` overflowed for Int64 inputs"
                ))
            })?;
        }
        Some(existing) => {
            return Err(invalid_aggregate_pair(
                function,
                existing,
                &Value::Int64(value),
            ));
        }
    }
    Ok(())
}

fn update_float64_state(state: &mut AggregateState, value: f64) -> DevonResult<()> {
    match state {
        AggregateState::Count(count) => increment_count(count),
        AggregateState::Sum(sum) => update_float64_sum(sum, value, "sum"),
        AggregateState::Min(minimum) => {
            update_extreme(minimum, &Value::Float64(value), Ordering::Less)
        }
        AggregateState::Max(maximum) => {
            update_extreme(maximum, &Value::Float64(value), Ordering::Greater)
        }
        AggregateState::Avg { sum, count } => {
            update_float64_sum(sum, value, "avg")?;
            increment_avg_count(count)
        }
        AggregateState::Percentile { .. } => invalid_typed_percentile(),
    }
}

fn update_float64_sum(sum: &mut Option<Value>, value: f64, function: &str) -> DevonResult<()> {
    match sum {
        None => *sum = Some(Value::Float64(value)),
        Some(Value::Float64(total)) => *total += value,
        Some(existing) => {
            return Err(invalid_aggregate_pair(
                function,
                existing,
                &Value::Float64(value),
            ));
        }
    }
    Ok(())
}

fn update_bool_state(state: &mut AggregateState, value: bool) -> DevonResult<()> {
    match state {
        AggregateState::Count(count) => increment_count(count),
        AggregateState::Min(minimum) => {
            update_extreme(minimum, &Value::Bool(value), Ordering::Less)
        }
        AggregateState::Max(maximum) => {
            update_extreme(maximum, &Value::Bool(value), Ordering::Greater)
        }
        _ => update_aggregate_state(state, &Value::Bool(value)),
    }
}

fn update_timestamp_state(state: &mut AggregateState, value: i64) -> DevonResult<()> {
    match state {
        AggregateState::Count(count) => increment_count(count),
        AggregateState::Min(minimum) => {
            update_extreme(minimum, &Value::Timestamp(value), Ordering::Less)
        }
        AggregateState::Max(maximum) => {
            update_extreme(maximum, &Value::Timestamp(value), Ordering::Greater)
        }
        _ => update_aggregate_state(state, &Value::Timestamp(value)),
    }
}

fn update_decimal_state(state: &mut AggregateState, value: Decimal128) -> DevonResult<()> {
    match state {
        AggregateState::Count(count) => increment_count(count),
        AggregateState::Sum(sum) => update_decimal_state_sum(sum, value),
        AggregateState::Min(minimum) => {
            update_extreme(minimum, &Value::Decimal(value), Ordering::Less)
        }
        AggregateState::Max(maximum) => {
            update_extreme(maximum, &Value::Decimal(value), Ordering::Greater)
        }
        AggregateState::Avg { sum, count } => {
            update_decimal_state_sum(sum, value)?;
            increment_avg_count(count)
        }
        AggregateState::Percentile { .. } => invalid_typed_percentile(),
    }
}

fn update_decimal_state_sum(sum: &mut Option<Value>, value: Decimal128) -> DevonResult<()> {
    match sum {
        None => {
            validate_decimal_sum_precision(value)?;
            *sum = Some(Value::Decimal(value));
            Ok(())
        }
        Some(Value::Decimal(total)) => update_decimal_sum(total, &value),
        Some(existing) => Err(invalid_aggregate_pair(
            "sum",
            existing,
            &Value::Decimal(value),
        )),
    }
}

fn increment_count(count: &mut i64) -> DevonResult<()> {
    *count = count
        .checked_add(1)
        .ok_or_else(|| invalid_argument("aggregate `count` overflowed Int64"))?;
    Ok(())
}

fn increment_avg_count(count: &mut u64) -> DevonResult<()> {
    *count = count
        .checked_add(1)
        .ok_or_else(|| invalid_argument("aggregate `avg` input count overflowed"))?;
    Ok(())
}

fn invalid_typed_percentile() -> DevonResult<()> {
    Err(DevonError::Corrupt {
        context: "percentile_cont state received an unordered input record".into(),
    })
}

fn update_count(count: &mut i64, value: &Value) -> DevonResult<()> {
    if !matches!(value, Value::Null) {
        *count = count
            .checked_add(1)
            .ok_or_else(|| invalid_argument("aggregate `count` overflowed Int64"))?;
    }
    Ok(())
}

fn update_sum(sum: &mut Option<Value>, value: &Value) -> DevonResult<()> {
    if matches!(value, Value::Null) {
        return Ok(());
    }
    match sum {
        None => match value {
            Value::Int64(_) | Value::Float64(_) => {
                *sum = Some(value.clone());
                Ok(())
            }
            Value::Decimal(value) => {
                validate_decimal_sum_precision(*value)?;
                *sum = Some(Value::Decimal(*value));
                Ok(())
            }
            _ => Err(invalid_aggregate_type("sum", value)),
        },
        Some(Value::Int64(total)) => match value {
            Value::Int64(value) => {
                *total = total.checked_add(*value).ok_or_else(|| {
                    invalid_argument("aggregate `sum` overflowed for Int64 inputs")
                })?;
                Ok(())
            }
            _ => Err(invalid_aggregate_pair("sum", &Value::Int64(*total), value)),
        },
        Some(Value::Float64(total)) => match value {
            Value::Float64(value) => {
                *total += value;
                Ok(())
            }
            _ => Err(invalid_aggregate_pair(
                "sum",
                &Value::Float64(*total),
                value,
            )),
        },
        Some(Value::Decimal(total)) => match value {
            Value::Decimal(value) => update_decimal_sum(total, value),
            _ => Err(invalid_aggregate_pair(
                "sum",
                &Value::Decimal(*total),
                value,
            )),
        },
        Some(existing) => Err(invalid_aggregate_pair("sum", existing, value)),
    }
}

fn update_decimal_sum(total: &mut Decimal128, value: &Decimal128) -> DevonResult<()> {
    if total.scale() != value.scale() {
        return Err(DevonError::Corrupt {
            context: format!(
                "aggregate `sum` cannot combine Decimal values with scales {} and {}",
                total.scale(),
                value.scale()
            ),
        });
    }
    let digits = total
        .digits()
        .checked_add(value.digits())
        .ok_or_else(decimal_sum_overflow)?;
    let next = Decimal128::new(digits, total.scale())?;
    validate_decimal_sum_precision(next)?;
    *total = next;
    Ok(())
}

fn validate_decimal_sum_precision(value: Decimal128) -> DevonResult<()> {
    if value.precision() > MAX_PRECISION {
        return Err(decimal_sum_overflow());
    }
    Ok(())
}

fn decimal_sum_overflow() -> DevonError {
    invalid_argument(format!(
        "aggregate `sum` overflowed Decimal precision {MAX_PRECISION}"
    ))
}

fn decimal_to_f64_for_avg(value: Decimal128) -> f64 {
    value.digits() as f64 / f64::powi(10.0, i32::from(value.scale()))
}

fn update_extreme(
    extreme: &mut Option<Value>,
    value: &Value,
    replacement_order: Ordering,
) -> DevonResult<()> {
    if matches!(value, Value::Null) {
        return Ok(());
    }
    compare_order_values(value, value)?;
    let Some(current) = extreme else {
        *extreme = Some(value.clone());
        return Ok(());
    };
    if compare_order_values(value, current)? == replacement_order {
        *current = value.clone();
    }
    Ok(())
}

fn update_avg(sum: &mut Option<Value>, count: &mut u64, value: &Value) -> DevonResult<()> {
    if matches!(value, Value::Null) {
        return Ok(());
    }
    match sum {
        None => match value {
            Value::Int64(_) | Value::Float64(_) | Value::Decimal(_) => {
                *sum = Some(value.clone());
            }
            _ => return Err(invalid_aggregate_type("avg", value)),
        },
        Some(Value::Int64(total)) => match value {
            Value::Int64(value) => {
                *total = total.checked_add(*value).ok_or_else(|| {
                    invalid_argument("aggregate `avg` overflowed for Int64 inputs")
                })?;
            }
            _ => return Err(invalid_aggregate_pair("avg", &Value::Int64(*total), value)),
        },
        Some(Value::Float64(total)) => match value {
            Value::Float64(value) => *total += value,
            _ => {
                return Err(invalid_aggregate_pair(
                    "avg",
                    &Value::Float64(*total),
                    value,
                ));
            }
        },
        Some(Value::Decimal(total)) => match value {
            Value::Decimal(value) => update_decimal_sum(total, value)?,
            _ => {
                return Err(invalid_aggregate_pair(
                    "avg",
                    &Value::Decimal(*total),
                    value,
                ));
            }
        },
        Some(existing) => return Err(invalid_aggregate_pair("avg", existing, value)),
    }
    *count = count
        .checked_add(1)
        .ok_or_else(|| invalid_argument("aggregate `avg` input count overflowed"))?;
    Ok(())
}

fn finish_aggregate_states(
    states: Vec<AggregateState>,
    output_types: &[LogicalType],
) -> DevonResult<Vec<Value>> {
    if states.len() != output_types.len() {
        return Err(DevonError::Corrupt {
            context: format!(
                "aggregate has {} states but {} result types",
                states.len(),
                output_types.len()
            ),
        });
    }
    states
        .into_iter()
        .zip(output_types)
        .map(|(state, output_type)| finish_aggregate_state(state, *output_type))
        .collect()
}

fn finish_aggregate_state(state: AggregateState, output_type: LogicalType) -> DevonResult<Value> {
    match state {
        AggregateState::Count(count) => Ok(Value::Int64(count)),
        AggregateState::Sum(value) | AggregateState::Min(value) | AggregateState::Max(value) => {
            Ok(value.unwrap_or(Value::Null))
        }
        AggregateState::Avg { sum: _, count: 0 } => Ok(Value::Null),
        AggregateState::Avg { sum, count } => {
            // Decimal accumulation remains exact through the input; this is
            // the single specified rounding point.
            let numeric_sum = match sum {
                Some(Value::Int64(total)) => total as f64,
                Some(Value::Float64(total)) => total,
                Some(Value::Decimal(total)) => decimal_to_f64_for_avg(total),
                _ => {
                    return Err(DevonError::Corrupt {
                        context: "aggregate `avg` retained a non-numeric sum".into(),
                    });
                }
            };
            Ok(Value::Float64(numeric_sum / count as f64))
        }
        AggregateState::Percentile {
            expected: 0,
            seen: 0,
            ..
        } => Ok(Value::Null),
        AggregateState::Percentile {
            expected,
            seen,
            lower,
            upper,
            ..
        } => finish_percentile(expected, seen, lower, upper, output_type),
    }
}

fn finish_percentile(
    expected: u64,
    seen: u64,
    lower: Option<Decimal128>,
    upper: Option<Decimal128>,
    output_type: LogicalType,
) -> DevonResult<Value> {
    if seen != expected {
        return Err(DevonError::Corrupt {
            context: format!(
                "aggregate `percentile_cont` counted {expected} values but received {seen}"
            ),
        });
    }
    let (precision, scale) = match output_type {
        LogicalType::Decimal { precision, scale } => (precision, scale),
        _ => {
            return Err(DevonError::Corrupt {
                context: format!(
                    "aggregate `percentile_cont` has non-Decimal output type {output_type}"
                ),
            });
        }
    };
    let lower = lower.ok_or_else(|| DevonError::Corrupt {
        context: "aggregate `percentile_cont` did not retain its lower rank".into(),
    })?;
    let upper = upper.ok_or_else(|| DevonError::Corrupt {
        context: "aggregate `percentile_cont` did not retain its upper rank".into(),
    })?;
    if lower.scale() != scale || upper.scale() != scale {
        return Err(DevonError::Corrupt {
            context: format!(
                "aggregate `percentile_cont` output scale {scale} disagrees with input scales {} and {}",
                lower.scale(),
                upper.scale()
            ),
        });
    }
    let result = if lower == upper {
        lower
    } else {
        exact_decimal_mean(lower, upper)?
    };
    if !result.fits(precision, scale) {
        return Err(invalid_argument(format!(
            "aggregate `percentile_cont` result {result} does not fit Decimal({precision}, {scale})"
        )));
    }
    Ok(Value::Decimal(result))
}

fn exact_decimal_mean(left: Decimal128, right: Decimal128) -> DevonResult<Decimal128> {
    let left_digits = left.digits();
    let right_digits = right.digits();
    if left_digits.rem_euclid(2) != right_digits.rem_euclid(2) {
        return Err(invalid_argument(format!(
            "aggregate `percentile_cont` interpolation between {left} and {right} is not representable at Decimal scale {}",
            left.scale()
        )));
    }
    let halves = left_digits
        .checked_div(2)
        .and_then(|left_half| left_half.checked_add(right_digits / 2))
        .and_then(|halves| halves.checked_add((left_digits % 2 + right_digits % 2) / 2))
        .ok_or_else(|| {
            invalid_argument("aggregate `percentile_cont` interpolation overflowed i128")
        })?;
    Decimal128::new(halves, left.scale())
}

fn stable_sort_by<T>(
    rows: &mut Vec<T>,
    mut compare: impl FnMut(&T, &T) -> DevonResult<Ordering>,
) -> DevonResult<()> {
    let mut order = (0..rows.len()).collect::<Vec<_>>();
    let mut scratch = vec![0; rows.len()];
    merge_sort_indices(&mut order, &mut scratch, rows, &mut compare)?;

    let mut original = std::mem::take(rows)
        .into_iter()
        .map(Some)
        .collect::<Vec<_>>();
    for index in order {
        let row = original
            .get_mut(index)
            .and_then(Option::take)
            .ok_or_else(invalid_sort_permutation)?;
        rows.push(row);
    }
    Ok(())
}

fn merge_sort_indices<T>(
    indices: &mut [usize],
    scratch: &mut [usize],
    rows: &[T],
    compare: &mut impl FnMut(&T, &T) -> DevonResult<Ordering>,
) -> DevonResult<()> {
    if indices.len() < 2 {
        return Ok(());
    }
    let middle = indices.len() / 2;
    let (left_indices, right_indices) = indices.split_at_mut(middle);
    let (left_scratch, right_scratch) = scratch.split_at_mut(middle);
    merge_sort_indices(left_indices, left_scratch, rows, compare)?;
    merge_sort_indices(right_indices, right_scratch, rows, compare)?;

    scratch.copy_from_slice(indices);
    let (left, right) = scratch.split_at(middle);
    merge_indices(indices, left, right, rows, compare)
}

fn merge_indices<T>(
    output: &mut [usize],
    left: &[usize],
    right: &[usize],
    rows: &[T],
    compare: &mut impl FnMut(&T, &T) -> DevonResult<Ordering>,
) -> DevonResult<()> {
    let (mut left_index, mut right_index) = (0, 0);
    for output_index in output {
        let next = match (
            left.get(left_index).copied(),
            right.get(right_index).copied(),
        ) {
            (Some(left_row), Some(right_row)) => {
                if compare(&rows[left_row], &rows[right_row])? != Ordering::Greater {
                    left_index += 1;
                    left_row
                } else {
                    right_index += 1;
                    right_row
                }
            }
            (Some(left_row), None) => {
                left_index += 1;
                left_row
            }
            (None, Some(right_row)) => {
                right_index += 1;
                right_row
            }
            (None, None) => return Err(invalid_sort_permutation()),
        };
        *output_index = next;
    }
    Ok(())
}

fn invalid_sort_permutation() -> DevonError {
    DevonError::Corrupt {
        context: "blocking operator produced an invalid row permutation".into(),
    }
}

/// Rebuilds a latched terminal-initialization error for replay on later
/// `next_chunk` calls. `DevonError` is not `Clone` (its `Io` payload is not
/// cloneable), so the latch stores this replayable copy while the caller
/// receives the original; every later call observes the same variant and
/// message without re-running the failed initialization.
fn replay_terminal_error(error: &DevonError) -> DevonError {
    match error {
        DevonError::Io(source) => {
            DevonError::Io(std::io::Error::new(source.kind(), source.to_string()))
        }
        DevonError::Corrupt { context } => DevonError::Corrupt {
            context: context.clone(),
        },
        DevonError::VersionMismatch {
            file_version,
            min_reader_version,
            supported,
        } => DevonError::VersionMismatch {
            file_version: *file_version,
            min_reader_version: *min_reader_version,
            supported: *supported,
        },
        DevonError::InvalidArgument { context } => DevonError::InvalidArgument {
            context: context.clone(),
        },
        DevonError::NotFound { what } => DevonError::NotFound { what: what.clone() },
        DevonError::TransactionConflict { context } => DevonError::TransactionConflict {
            context: context.clone(),
        },
        DevonError::BudgetExceeded { context } => DevonError::BudgetExceeded {
            context: context.clone(),
        },
        DevonError::ReadOnly { context } => DevonError::ReadOnly {
            context: context.clone(),
        },
        DevonError::Busy { context } => DevonError::Busy {
            context: context.clone(),
        },
    }
}

fn compare_key_tuples(left: &[Value], right: &[Value]) -> DevonResult<Ordering> {
    for (left, right) in left.iter().zip(right) {
        let ordering = compare_order_values(left, right)?;
        if ordering != Ordering::Equal {
            return Ok(ordering);
        }
    }
    Ok(left.len().cmp(&right.len()))
}

fn compare_order_values(left: &Value, right: &Value) -> DevonResult<Ordering> {
    match (left, right) {
        (Value::Null, Value::Null) => Ok(Ordering::Equal),
        (Value::Null, _) => Ok(Ordering::Less),
        (_, Value::Null) => Ok(Ordering::Greater),
        (Value::Bool(left), Value::Bool(right)) => Ok(left.cmp(right)),
        (Value::Int64(left), Value::Int64(right)) => Ok(left.cmp(right)),
        (Value::Float64(left), Value::Float64(right)) => Ok(left.total_cmp(right)),
        (Value::String(left), Value::String(right)) => Ok(left.cmp(right)),
        (Value::Timestamp(left), Value::Timestamp(right)) => Ok(left.cmp(right)),
        (Value::Decimal(left), Value::Decimal(right)) => compare_decimal_order(left, right),
        // Mixed types, and same-typed values with no defined order
        // (Vector, GeoPoint, Bytes, and Json are not orderable scalars).
        _ => Err(DevonError::Corrupt {
            context: format!(
                "cannot order values of types {} and {}",
                value_variant(left),
                value_variant(right)
            ),
        }),
    }
}

fn compare_decimal_order(
    left: &devondb_types::decimal::Decimal128,
    right: &devondb_types::decimal::Decimal128,
) -> DevonResult<Ordering> {
    if left.scale() != right.scale() {
        return Err(DevonError::Corrupt {
            context: format!(
                "cannot order Decimal values with scales {} and {}",
                left.scale(),
                right.scale()
            ),
        });
    }
    Ok(left.digits().cmp(&right.digits()))
}

fn build_output_chunks(
    types: &[LogicalType],
    rows: impl IntoIterator<Item = Vec<Value>>,
) -> DevonResult<VecDeque<Chunk>> {
    let mut output = VecDeque::new();
    let mut builder = ChunkBuilder::new(types.to_vec());
    let mut pending_rows = 0;
    for row in rows {
        builder.push_row(row)?;
        pending_rows += 1;
        if builder.is_full() {
            output.push_back(builder.finish());
            builder = ChunkBuilder::new(types.to_vec());
            pending_rows = 0;
        }
    }
    if pending_rows != 0 {
        output.push_back(builder.finish());
    }
    Ok(output)
}

fn values_at(columns: &[Vec<Value>], row: usize, context: &str) -> DevonResult<Vec<Value>> {
    columns
        .iter()
        .map(|column| {
            column.get(row).cloned().ok_or_else(|| {
                invalid_argument(format!("{context} returned fewer than {} rows", row + 1))
            })
        })
        .collect()
}

fn invalid_aggregate_type(function: &str, value: &Value) -> DevonError {
    invalid_argument(format!(
        "aggregate `{function}` does not accept input type {}",
        value_variant(value)
    ))
}

fn invalid_aggregate_pair(function: &str, left: &Value, right: &Value) -> DevonError {
    invalid_argument(format!(
        "aggregate `{function}` cannot combine input types {} and {}",
        value_variant(left),
        value_variant(right)
    ))
}

fn value_variant(value: &Value) -> &'static str {
    match value {
        Value::Null => "Null",
        Value::Bool(_) => "Bool",
        Value::Int64(_) => "Int64",
        Value::Float64(_) => "Float64",
        Value::String(_) => "String",
        Value::Vector(_) => "Vector",
        Value::GeoPoint(_) => "GeoPoint",
        Value::Timestamp(_) => "Timestamp",
        Value::Bytes(_) => "Bytes",
        Value::Decimal(_) => "Decimal",
        Value::Json(_) => "Json",
    }
}

fn selected_rows(predicate: &[Value]) -> DevonResult<Vec<usize>> {
    let mut selected = Vec::with_capacity(predicate.len().min(CHUNK_CAPACITY));
    for (row, value) in predicate.iter().enumerate() {
        match value {
            Value::Bool(true) => selected.push(row),
            Value::Bool(false) | Value::Null => {}
            _ => {
                return Err(invalid_argument(format!(
                    "filter predicate evaluated to non-Bool value {value} at row {row}"
                )));
            }
        }
    }
    Ok(selected)
}

fn projected_chunk(
    columns: &[Vec<Value>],
    types: &[LogicalType],
    row_count: usize,
) -> DevonResult<Chunk> {
    let mut builder = ChunkBuilder::new(types.to_vec());
    for row in 0..row_count {
        let values = columns
            .iter()
            .map(|column| {
                column.get(row).cloned().ok_or_else(|| {
                    invalid_argument(format!(
                        "projection expression returned fewer than {row_count} rows"
                    ))
                })
            })
            .collect::<DevonResult<Vec<_>>>()?;
        builder.push_row(values)?;
    }
    Ok(builder.finish())
}

fn copy_rows(chunk: &Chunk, rows: impl IntoIterator<Item = usize>) -> DevonResult<Chunk> {
    let mut builder = ChunkBuilder::new(chunk.types().to_vec());
    for row in rows {
        builder.push_row(clone_row(chunk, row)?)?;
    }
    Ok(builder.finish())
}

fn clone_row(chunk: &Chunk, row: usize) -> DevonResult<Vec<Value>> {
    (0..chunk.column_count())
        .map(|column| {
            chunk.value(row, column).ok_or_else(|| {
                invalid_argument(format!(
                    "cannot copy missing row {row}, column {column} from chunk"
                ))
            })
        })
        .collect()
}

fn invalid_argument(context: impl Into<String>) -> DevonError {
    DevonError::InvalidArgument {
        context: context.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        collections::{HashMap, VecDeque},
        fs,
        path::PathBuf,
        rc::Rc,
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering as AtomicOrdering},
        },
        time::{SystemTime, UNIX_EPOCH},
    };

    use devondb_plan::{
        expr::{BinaryOp, Expr},
        ops::{AggregateFunction, Operator, SortOrder},
    };
    use devondb_storage::budget::MemoryBudget;
    use devondb_types::{
        DevonError, DevonResult, decimal::Decimal128, logical_type::LogicalType, value::Value,
    };

    use super::{
        Aggregate, Filter, InterfaceScanInput, Limit, Project, ScanInterface, Sort, SpillConfig,
        has_constant_aggregate_state,
    };
    use crate::{
        chunk::{CHUNK_CAPACITY, Chunk, ChunkBuilder},
        source::{ChunkSource, OuterBindings, ScalarSubqueryExecutor, ScalarSubqueryResult},
    };

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

    fn interface_input_chunk(id: i64, name: &str) -> Chunk {
        let mut builder = ChunkBuilder::new(vec![
            LogicalType::Int64,
            LogicalType::String,
            LogicalType::Int64,
        ]);
        builder
            .push_row(vec![
                Value::Int64(id),
                Value::String(name.to_owned()),
                Value::Int64(id - 1),
            ])
            .unwrap();
        builder.finish()
    }

    #[test]
    fn scan_interface_streams_implementers_in_order_and_projects_columns() {
        let first_pulls = Rc::new(Cell::new(0));
        let second_pulls = Rc::new(Cell::new(0));
        let first_opens = Rc::new(Cell::new(0));
        let second_opens = Rc::new(Cell::new(0));
        let first_factory_pulls = Rc::clone(&first_pulls);
        let first_factory_opens = Rc::clone(&first_opens);
        let second_factory_pulls = Rc::clone(&second_pulls);
        let second_factory_opens = Rc::clone(&second_opens);
        let mut scan = ScanInterface::new(
            vec![
                InterfaceScanInput::deferred(
                    move || {
                        first_factory_opens.set(first_factory_opens.get() + 1);
                        Ok(Box::new(CountingSource {
                            chunks: VecDeque::from([interface_input_chunk(1, "Ada")]),
                            pulls: first_factory_pulls,
                        }))
                    },
                    vec![1],
                    "Person".into(),
                ),
                InterfaceScanInput::deferred(
                    move || {
                        second_factory_opens.set(second_factory_opens.get() + 1);
                        Ok(Box::new(CountingSource {
                            chunks: VecDeque::from([interface_input_chunk(2, "Devon")]),
                            pulls: second_factory_pulls,
                        }))
                    },
                    vec![1],
                    "Project".into(),
                ),
            ],
            vec![LogicalType::String],
        );

        let first = scan.next_chunk().unwrap().unwrap();
        assert_eq!(
            first.rows().next(),
            Some(vec![
                Value::String("Ada".into()),
                Value::String("Person".into())
            ])
        );
        assert_eq!(first_opens.get(), 1);
        assert_eq!(first_pulls.get(), 1);
        assert_eq!(second_opens.get(), 0, "the union eagerly opened table two");
        assert_eq!(second_pulls.get(), 0, "the union eagerly pulled table two");

        let second = scan.next_chunk().unwrap().unwrap();
        assert_eq!(
            second.rows().next(),
            Some(vec![
                Value::String("Devon".into()),
                Value::String("Project".into())
            ])
        );
        assert_eq!(second_opens.get(), 1);
        assert_eq!(scan.next_chunk().unwrap(), None);
    }

    #[test]
    fn scan_interface_with_zero_implementers_is_empty() {
        let mut scan = ScanInterface::new(Vec::new(), vec![LogicalType::String]);
        assert_eq!(scan.next_chunk().unwrap(), None);
    }

    struct CountingSource {
        chunks: VecDeque<Chunk>,
        pulls: Rc<Cell<usize>>,
    }

    impl ChunkSource for CountingSource {
        fn next_chunk(&mut self) -> DevonResult<Option<Chunk>> {
            self.pulls.set(self.pulls.get() + 1);
            Ok(self.chunks.pop_front())
        }
    }

    struct ErrorSource {
        first: Option<Chunk>,
    }

    struct TestScalarExecutor {
        executions: Arc<AtomicU64>,
        budget: Arc<MemoryBudget>,
    }

    impl ScalarSubqueryExecutor for TestScalarExecutor {
        fn memory_budget(&self) -> Arc<MemoryBudget> {
            Arc::clone(&self.budget)
        }

        fn execute(
            &self,
            plan: &Operator,
            outer: &OuterBindings,
        ) -> DevonResult<ScalarSubqueryResult> {
            self.executions.fetch_add(1, AtomicOrdering::Relaxed);
            let table = scalar_test_table(plan).ok_or_else(|| DevonError::InvalidArgument {
                context: "test scalar plan has no scan".into(),
            })?;
            let values = match table {
                "zero" => Vec::new(),
                "one" => vec![Value::Int64(7)],
                "many" => vec![Value::Int64(1), Value::Int64(2)],
                "correlated" => vec![Value::Int64(
                    outer
                        .get("o.value")
                        .and_then(|value| match value {
                            Value::Int64(value) => Some(*value),
                            _ => None,
                        })
                        .ok_or_else(|| DevonError::InvalidArgument {
                            context: "correlated scalar did not receive `o.value`".into(),
                        })?
                        * 10,
                )],
                other => {
                    return Err(DevonError::InvalidArgument {
                        context: format!("unknown test scalar table `{other}`"),
                    });
                }
            };
            Ok(ScalarSubqueryResult {
                output_type: LogicalType::Int64,
                values,
            })
        }
    }

    fn scalar_test_table(operator: &Operator) -> Option<&str> {
        match operator {
            Operator::ScanNodes { table, .. } => Some(table),
            Operator::ExpandRel { input, .. }
            | Operator::Expand { input, .. }
            | Operator::Filter { input, .. }
            | Operator::Project { input, .. }
            | Operator::Sort { input, .. }
            | Operator::Limit { input, .. }
            | Operator::Aggregate { input, .. } => scalar_test_table(input),
            Operator::HashJoin { left, .. } => scalar_test_table(left),
            Operator::KnnScan { .. }
            | Operator::TextScan { .. }
            | Operator::WithinScan { .. }
            | Operator::ScanInterface { .. } => None,
        }
    }

    fn scalar_scan(table: &str) -> Expr {
        Expr::Scalar {
            plan: Box::new(Operator::ScanNodes {
                table: table.into(),
                binding: "i".into(),
            }),
        }
    }

    fn correlated_scalar() -> Expr {
        Expr::Scalar {
            plan: Box::new(Operator::Filter {
                predicate: binary(
                    BinaryOp::Eq,
                    Expr::Col("i.value".into()),
                    Expr::Col("o.value".into()),
                ),
                input: Box::new(Operator::ScanNodes {
                    table: "correlated".into(),
                    binding: "i".into(),
                }),
            }),
        }
    }

    static NEXT_SPILL_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct SpillDirectory {
        path: PathBuf,
    }

    impl SpillDirectory {
        fn new() -> Self {
            let sequence = NEXT_SPILL_DIRECTORY.fetch_add(1, AtomicOrdering::Relaxed);
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            Self {
                path: std::env::temp_dir().join(format!(
                    "devondb-exec-spill-test-{}-{timestamp}-{sequence}",
                    std::process::id()
                )),
            }
        }

        fn config(&self, limit: usize) -> SpillConfig {
            SpillConfig {
                budget: Arc::new(MemoryBudget::new(limit)),
                tmp_dir: self.path.clone(),
            }
        }
    }

    impl Drop for SpillDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    impl ChunkSource for ErrorSource {
        fn next_chunk(&mut self) -> DevonResult<Option<Chunk>> {
            if let Some(chunk) = self.first.take() {
                Ok(Some(chunk))
            } else {
                Err(DevonError::InvalidArgument {
                    context: "upstream boom".into(),
                })
            }
        }
    }

    fn chunk(types: Vec<LogicalType>, rows: Vec<Vec<Value>>) -> Chunk {
        let mut builder = ChunkBuilder::new(types);
        for row in rows {
            builder.push_row(row).unwrap();
        }
        builder.finish()
    }

    fn int_chunk(values: impl IntoIterator<Item = i64>) -> Chunk {
        chunk(
            vec![LogicalType::Int64],
            values
                .into_iter()
                .map(|value| vec![Value::Int64(value)])
                .collect(),
        )
    }

    fn decimal(digits: i128, scale: u8) -> Value {
        Value::Decimal(Decimal128::new(digits, scale).unwrap())
    }

    fn column_map(entries: &[(&str, usize)]) -> HashMap<String, usize> {
        entries
            .iter()
            .map(|(name, index)| ((*name).to_owned(), *index))
            .collect()
    }

    fn binary(op: BinaryOp, left: Expr, right: Expr) -> Expr {
        Expr::Binary {
            op,
            left: Box::new(left),
            right: Box::new(right),
        }
    }

    fn collect_column(source: &mut dyn ChunkSource, column: usize) -> Vec<Value> {
        let mut values = Vec::new();
        while let Some(chunk) = source.next_chunk().unwrap() {
            values.extend((0..chunk.row_count()).map(|row| chunk.value(row, column).unwrap()));
        }
        values
    }

    fn collect_chunks(source: &mut dyn ChunkSource) -> Vec<Chunk> {
        let mut chunks = Vec::new();
        while let Some(chunk) = source.next_chunk().unwrap() {
            chunks.push(chunk);
        }
        chunks
    }

    fn aggregate_extremes(input: Chunk, output_type: LogicalType) -> Chunk {
        let value = Expr::Col("r.value".into());
        let mut aggregate = Aggregate::new(
            Box::new(VecSource::new(vec![input])),
            vec![],
            vec![
                (AggregateFunction::Min, value.clone()),
                (AggregateFunction::Max, value),
            ],
            column_map(&[("r.value", 0)]),
            vec![output_type, output_type],
            SpillConfig::unbounded(),
        );
        aggregate.next_chunk().unwrap().unwrap()
    }

    #[test]
    fn filter_keeps_true_and_drops_false_and_null_rows() {
        let input = chunk(
            vec![LogicalType::Int64, LogicalType::Bool],
            vec![
                vec![Value::Int64(1), Value::Bool(true)],
                vec![Value::Int64(2), Value::Bool(false)],
                vec![Value::Int64(3), Value::Null],
                vec![Value::Int64(4), Value::Bool(true)],
            ],
        );
        let mut filter = Filter::new(
            Box::new(VecSource::new(vec![input])),
            Expr::Col("p.keep".into()),
            column_map(&[("p.keep", 1)]),
        );

        let output = filter.next_chunk().unwrap().unwrap();
        assert_eq!(output.column_count(), 2);
        assert_eq!(
            output.column(0).unwrap(),
            &[Value::Int64(1), Value::Int64(4)]
        );
        assert_eq!(
            output.column(1).unwrap(),
            &[Value::Bool(true), Value::Bool(true)]
        );
        assert!(filter.next_chunk().unwrap().is_none());
    }

    #[test]
    fn filter_skips_empty_results_across_multiple_chunks() {
        let types = vec![LogicalType::Int64, LogicalType::Bool];
        let chunks = vec![
            chunk(
                types.clone(),
                vec![vec![Value::Int64(1), Value::Bool(false)]],
            ),
            chunk(
                types.clone(),
                vec![
                    vec![Value::Int64(2), Value::Bool(true)],
                    vec![Value::Int64(3), Value::Bool(false)],
                ],
            ),
            chunk(types, vec![vec![Value::Int64(4), Value::Bool(true)]]),
        ];
        let mut filter = Filter::new(
            Box::new(VecSource::new(chunks)),
            Expr::Col("p.keep".into()),
            column_map(&[("p.keep", 1)]),
        );

        assert_eq!(
            filter.next_chunk().unwrap().unwrap().column(0).unwrap(),
            &[Value::Int64(2)]
        );
        assert_eq!(
            filter.next_chunk().unwrap().unwrap().column(0).unwrap(),
            &[Value::Int64(4)]
        );
        assert!(filter.next_chunk().unwrap().is_none());
    }

    #[test]
    fn filter_rejects_and_names_non_bool_predicate_value() {
        let input = int_chunk([17]);
        let mut filter = Filter::new(
            Box::new(VecSource::new(vec![input])),
            Expr::Col("p.value".into()),
            column_map(&[("p.value", 0)]),
        );

        let DevonError::InvalidArgument { context } = filter.next_chunk().unwrap_err() else {
            panic!("expected InvalidArgument");
        };
        assert!(context.contains("non-Bool"));
        assert!(context.contains("17"));
    }

    #[test]
    fn project_computes_ordered_columns_and_preserves_row_counts() {
        let source = VecSource::new(vec![int_chunk([1, 2]), int_chunk([5])]);
        let expressions = vec![
            (Expr::Col("p.value".into()), "original".into()),
            (
                binary(
                    BinaryOp::Add,
                    Expr::Col("p.value".into()),
                    Expr::Lit(Value::Int64(10)),
                ),
                "plus_ten".into(),
            ),
            (Expr::Lit(Value::String("fixed".into())), "label".into()),
        ];
        let mut project = Project::new(
            Box::new(source),
            expressions,
            column_map(&[("p.value", 0)]),
            vec![LogicalType::Int64, LogicalType::Int64, LogicalType::String],
        )
        .unwrap();

        let first = project.next_chunk().unwrap().unwrap();
        assert_eq!(first.row_count(), 2);
        assert_eq!(first.column_count(), 3);
        assert_eq!(
            first.column(0).unwrap(),
            &[Value::Int64(1), Value::Int64(2)]
        );
        assert_eq!(
            first.column(1).unwrap(),
            &[Value::Int64(11), Value::Int64(12)]
        );
        assert_eq!(
            first.column(2).unwrap(),
            &[Value::String("fixed".into()), Value::String("fixed".into())]
        );

        let second = project.next_chunk().unwrap().unwrap();
        assert_eq!(second.row_count(), 1);
        assert_eq!(second.column(0).unwrap(), &[Value::Int64(5)]);
        assert_eq!(second.column(1).unwrap(), &[Value::Int64(15)]);
        assert!(project.next_chunk().unwrap().is_none());
    }

    #[test]
    fn limit_offset_spans_chunk_boundary() {
        let source = VecSource::new(vec![
            int_chunk([0, 1]),
            int_chunk([2, 3, 4]),
            int_chunk([5]),
        ]);
        let mut limit = Limit::new(Box::new(source), 3, 3);

        assert_eq!(
            limit.next_chunk().unwrap().unwrap().column(0).unwrap(),
            &[Value::Int64(3), Value::Int64(4)]
        );
        assert_eq!(
            limit.next_chunk().unwrap().unwrap().column(0).unwrap(),
            &[Value::Int64(5)]
        );
        assert!(limit.next_chunk().unwrap().is_none());
    }

    #[test]
    fn limit_stops_pulling_upstream_after_satisfaction() {
        let pulls = Rc::new(Cell::new(0));
        let source = CountingSource {
            chunks: vec![int_chunk([1, 2]), int_chunk([3, 4])].into(),
            pulls: Rc::clone(&pulls),
        };
        let mut limit = Limit::new(Box::new(source), 1, 0);

        assert_eq!(
            limit.next_chunk().unwrap().unwrap().column(0).unwrap(),
            &[Value::Int64(1)]
        );
        assert_eq!(pulls.get(), 1);
        assert!(limit.next_chunk().unwrap().is_none());
        assert_eq!(pulls.get(), 1);
    }

    #[test]
    fn filter_project_limit_chain_has_known_answers_over_four_chunks() {
        let scan = VecSource::new(vec![
            int_chunk([1, 2]),
            int_chunk([3, 4]),
            int_chunk([5, 6]),
            int_chunk([7, 8]),
        ]);
        let filter = Filter::new(
            Box::new(scan),
            binary(
                BinaryOp::Gt,
                Expr::Col("p.value".into()),
                Expr::Lit(Value::Int64(2)),
            ),
            column_map(&[("p.value", 0)]),
        );
        let project = Project::new(
            Box::new(filter),
            vec![(
                binary(
                    BinaryOp::Mul,
                    Expr::Col("p.value".into()),
                    Expr::Lit(Value::Int64(10)),
                ),
                "scaled".into(),
            )],
            column_map(&[("p.value", 0)]),
            vec![LogicalType::Int64],
        )
        .unwrap();
        let mut limit = Limit::new(Box::new(project), 3, 1);

        assert_eq!(
            collect_column(&mut limit, 0),
            vec![Value::Int64(40), Value::Int64(50), Value::Int64(60)]
        );
    }

    #[test]
    fn filter_timestamp_range_includes_pre_epoch_and_drops_null() {
        let input = chunk(
            vec![LogicalType::Timestamp],
            vec![
                vec![Value::Timestamp(-2_000_000)],
                vec![Value::Timestamp(-1_000_000)],
                vec![Value::Timestamp(0)],
                vec![Value::Timestamp(1_000_000)],
                vec![Value::Null],
            ],
        );
        let lower = binary(
            BinaryOp::Ge,
            Expr::Col("r.ts".into()),
            Expr::Lit(Value::Timestamp(-1_000_000)),
        );
        let upper = binary(
            BinaryOp::Lt,
            Expr::Col("r.ts".into()),
            Expr::Lit(Value::Timestamp(1_000_000)),
        );
        let mut filter = Filter::new(
            Box::new(VecSource::new(vec![input])),
            binary(BinaryOp::And, lower, upper),
            column_map(&[("r.ts", 0)]),
        );

        assert_eq!(
            collect_column(&mut filter, 0),
            vec![Value::Timestamp(-1_000_000), Value::Timestamp(0)]
        );
    }

    #[test]
    fn every_operator_returns_none_for_empty_upstream() {
        let mut filter = Filter::new(
            Box::new(VecSource::new(vec![])),
            Expr::Lit(Value::Bool(true)),
            HashMap::new(),
        );
        assert!(filter.next_chunk().unwrap().is_none());

        let mut project = Project::new(
            Box::new(VecSource::new(vec![])),
            vec![(Expr::Lit(Value::Int64(1)), "one".into())],
            HashMap::new(),
            vec![LogicalType::Int64],
        )
        .unwrap();
        assert!(project.next_chunk().unwrap().is_none());

        let mut limit = Limit::new(Box::new(VecSource::new(vec![])), 5, 2);
        assert!(limit.next_chunk().unwrap().is_none());
    }

    #[test]
    fn zero_limit_does_not_pull_upstream() {
        let pulls = Rc::new(Cell::new(0));
        let source = CountingSource {
            chunks: vec![int_chunk([1])].into(),
            pulls: Rc::clone(&pulls),
        };
        let mut limit = Limit::new(Box::new(source), 0, u64::MAX);

        assert!(limit.next_chunk().unwrap().is_none());
        assert_eq!(pulls.get(), 0);
    }

    #[test]
    fn sort_multi_key_ascending_descending_is_stable_on_ties() {
        let input = chunk(
            vec![LogicalType::Int64, LogicalType::String, LogicalType::Int64],
            vec![
                vec![Value::Int64(1), Value::String("b".into()), Value::Int64(10)],
                vec![Value::Int64(1), Value::String("a".into()), Value::Int64(11)],
                vec![Value::Int64(2), Value::String("c".into()), Value::Int64(12)],
                vec![Value::Int64(1), Value::String("b".into()), Value::Int64(13)],
            ],
        );
        let mut sort = Sort::new(
            Box::new(VecSource::new(vec![input])),
            vec![
                (Expr::Col("r.first".into()), SortOrder::Asc),
                (Expr::Col("r.second".into()), SortOrder::Desc),
            ],
            column_map(&[("r.first", 0), ("r.second", 1)]),
            SpillConfig::unbounded(),
        );

        let output = sort.next_chunk().unwrap().unwrap();
        assert_eq!(
            output.column(2).unwrap(),
            &[
                Value::Int64(10),
                Value::Int64(13),
                Value::Int64(11),
                Value::Int64(12),
            ]
        );
        assert!(sort.next_chunk().unwrap().is_none());
    }

    #[test]
    fn sort_nulls_first_ascending_last_descending_and_preserves_types() {
        let input = chunk(
            vec![LogicalType::String, LogicalType::Int64],
            vec![
                vec![Value::Null, Value::Int64(0)],
                vec![Value::String("a".into()), Value::Int64(1)],
                vec![Value::Null, Value::Int64(2)],
                vec![Value::String("b".into()), Value::Int64(3)],
            ],
        );
        let columns = column_map(&[("r.key", 0)]);
        let mut ascending = Sort::new(
            Box::new(VecSource::new(vec![input.clone()])),
            vec![(Expr::Col("r.key".into()), SortOrder::Asc)],
            columns.clone(),
            SpillConfig::unbounded(),
        );
        let mut descending = Sort::new(
            Box::new(VecSource::new(vec![input])),
            vec![(Expr::Col("r.key".into()), SortOrder::Desc)],
            columns,
            SpillConfig::unbounded(),
        );

        let ascending = ascending.next_chunk().unwrap().unwrap();
        assert_eq!(
            ascending.column(1).unwrap(),
            &[
                Value::Int64(0),
                Value::Int64(2),
                Value::Int64(1),
                Value::Int64(3),
            ]
        );
        assert_eq!(
            ascending.types(),
            &[LogicalType::String, LogicalType::Int64]
        );
        assert_eq!(
            descending.next_chunk().unwrap().unwrap().column(1).unwrap(),
            &[
                Value::Int64(3),
                Value::Int64(1),
                Value::Int64(0),
                Value::Int64(2),
            ]
        );
    }

    #[test]
    fn sort_float_total_cmp_places_negative_zero_before_zero_and_nan() {
        let nan = f64::from_bits(0x7ff8_0000_0000_0001);
        let input = chunk(
            vec![LogicalType::Float64, LogicalType::Int64],
            vec![
                vec![Value::Float64(nan), Value::Int64(1)],
                vec![Value::Float64(0.0), Value::Int64(2)],
                vec![Value::Float64(1.0), Value::Int64(3)],
                vec![Value::Float64(-0.0), Value::Int64(4)],
            ],
        );
        let mut sort = Sort::new(
            Box::new(VecSource::new(vec![input])),
            vec![(Expr::Col("r.key".into()), SortOrder::Asc)],
            column_map(&[("r.key", 0)]),
            SpillConfig::unbounded(),
        );

        assert_eq!(
            sort.next_chunk().unwrap().unwrap().column(1).unwrap(),
            &[
                Value::Int64(4),
                Value::Int64(2),
                Value::Int64(3),
                Value::Int64(1),
            ]
        );
    }

    #[test]
    fn sort_string_uses_byte_order() {
        let input = chunk(
            vec![LogicalType::String],
            vec![
                vec![Value::String("ä".into())],
                vec![Value::String("a".into())],
                vec![Value::String("Z".into())],
            ],
        );
        let mut sort = Sort::new(
            Box::new(VecSource::new(vec![input])),
            vec![(Expr::Col("r.key".into()), SortOrder::Asc)],
            column_map(&[("r.key", 0)]),
            SpillConfig::unbounded(),
        );

        assert_eq!(
            sort.next_chunk().unwrap().unwrap().column(0).unwrap(),
            &[
                Value::String("Z".into()),
                Value::String("a".into()),
                Value::String("ä".into()),
            ]
        );
    }

    #[test]
    fn sort_timestamp_uses_epoch_microsecond_total_order() {
        let input = chunk(
            vec![LogicalType::Timestamp],
            [0, -1, 1_000_000, -2_000_000]
                .into_iter()
                .map(|value| vec![Value::Timestamp(value)])
                .collect(),
        );
        let mut sort = Sort::new(
            Box::new(VecSource::new(vec![input])),
            vec![(Expr::Col("r.key".into()), SortOrder::Asc)],
            column_map(&[("r.key", 0)]),
            SpillConfig::unbounded(),
        );

        assert_eq!(
            collect_column(&mut sort, 0),
            vec![
                Value::Timestamp(-2_000_000),
                Value::Timestamp(-1),
                Value::Timestamp(0),
                Value::Timestamp(1_000_000),
            ]
        );
    }

    #[test]
    fn sort_equal_scale_decimal_compares_signed_digits() {
        let input = chunk(
            vec![LogicalType::Decimal {
                precision: 4,
                scale: 2,
            }],
            [150, -125, 0, -250]
                .into_iter()
                .map(|digits| vec![decimal(digits, 2)])
                .collect(),
        );
        let mut sort = Sort::new(
            Box::new(VecSource::new(vec![input])),
            vec![(Expr::Col("r.key".into()), SortOrder::Asc)],
            column_map(&[("r.key", 0)]),
            SpillConfig::unbounded(),
        );

        assert_eq!(
            collect_column(&mut sort, 0),
            vec![
                decimal(-250, 2),
                decimal(-125, 2),
                decimal(0, 2),
                decimal(150, 2),
            ]
        );
    }

    #[test]
    fn scalar_v2_sort_rejections_use_scale_and_not_comparable_messages() {
        let decimal_chunks = vec![
            chunk(
                vec![LogicalType::Decimal {
                    precision: 3,
                    scale: 2,
                }],
                vec![vec![decimal(100, 2)]],
            ),
            chunk(
                vec![LogicalType::Decimal {
                    precision: 3,
                    scale: 3,
                }],
                vec![vec![decimal(100, 3)]],
            ),
        ];
        let mut decimal_sort = Sort::new(
            Box::new(VecSource::new(decimal_chunks)),
            vec![(Expr::Col("r.key".into()), SortOrder::Asc)],
            column_map(&[("r.key", 0)]),
            SpillConfig::unbounded(),
        );
        let DevonError::Corrupt { context } = decimal_sort.next_chunk().unwrap_err() else {
            panic!("expected Corrupt");
        };
        assert_eq!(context, "cannot order Decimal values with scales 2 and 3");

        for (logical_type, value, variant) in [
            (LogicalType::Bytes, Value::Bytes(vec![0]), "Bytes"),
            (LogicalType::Json, Value::Json(r#"{"a":1}"#.into()), "Json"),
        ] {
            let input = chunk(vec![logical_type], vec![vec![value]]);
            let mut sort = Sort::new(
                Box::new(VecSource::new(vec![input])),
                vec![(Expr::Col("r.key".into()), SortOrder::Asc)],
                column_map(&[("r.key", 0)]),
                SpillConfig::unbounded(),
            );
            let DevonError::Corrupt { context } = sort.next_chunk().unwrap_err() else {
                panic!("expected Corrupt");
            };
            assert_eq!(
                context,
                format!("cannot order values of types {variant} and {variant}")
            );
        }
    }

    #[test]
    fn sort_output_spans_capacity_with_order_intact() {
        let row_count = CHUNK_CAPACITY + 3;
        let values = (0..row_count as i64).rev().collect::<Vec<_>>();
        let chunks = values
            .chunks(CHUNK_CAPACITY)
            .map(|values| int_chunk(values.iter().copied()))
            .collect();
        let mut sort = Sort::new(
            Box::new(VecSource::new(chunks)),
            vec![(Expr::Col("r.key".into()), SortOrder::Asc)],
            column_map(&[("r.key", 0)]),
            SpillConfig::unbounded(),
        );

        let first = sort.next_chunk().unwrap().unwrap();
        let second = sort.next_chunk().unwrap().unwrap();
        assert_eq!(first.row_count(), CHUNK_CAPACITY);
        assert_eq!(first.value(0, 0), Some(Value::Int64(0)));
        assert_eq!(
            first.value(CHUNK_CAPACITY - 1, 0),
            Some(Value::Int64(CHUNK_CAPACITY as i64 - 1))
        );
        assert_eq!(second.row_count(), 3);
        assert_eq!(
            second.column(0).unwrap(),
            &[
                Value::Int64(CHUNK_CAPACITY as i64),
                Value::Int64(CHUNK_CAPACITY as i64 + 1),
                Value::Int64(CHUNK_CAPACITY as i64 + 2),
            ]
        );
        assert!(sort.next_chunk().unwrap().is_none());
    }

    #[test]
    fn sort_mixed_variant_keys_are_corrupt_and_name_both_types() {
        let float_chunk = chunk(vec![LogicalType::Float64], vec![vec![Value::Float64(1.0)]]);
        let mut sort = Sort::new(
            Box::new(VecSource::new(vec![int_chunk([1]), float_chunk])),
            vec![(Expr::Col("r.key".into()), SortOrder::Asc)],
            column_map(&[("r.key", 0)]),
            SpillConfig::unbounded(),
        );

        let DevonError::Corrupt { context } = sort.next_chunk().unwrap_err() else {
            panic!("expected Corrupt");
        };
        assert!(context.contains("Int64"));
        assert!(context.contains("Float64"));
    }

    #[test]
    fn sort_is_blocking_and_propagates_upstream_errors() {
        let pulls = Rc::new(Cell::new(0));
        let source = CountingSource {
            chunks: vec![int_chunk([2]), int_chunk([1])].into(),
            pulls: Rc::clone(&pulls),
        };
        let mut sort = Sort::new(
            Box::new(source),
            vec![(Expr::Col("r.key".into()), SortOrder::Asc)],
            column_map(&[("r.key", 0)]),
            SpillConfig::unbounded(),
        );
        assert!(sort.next_chunk().unwrap().is_some());
        assert_eq!(pulls.get(), 3);

        let mut failing = Sort::new(
            Box::new(ErrorSource {
                first: Some(int_chunk([1])),
            }),
            vec![(Expr::Col("r.key".into()), SortOrder::Asc)],
            column_map(&[("r.key", 0)]),
            SpillConfig::unbounded(),
        );
        let DevonError::InvalidArgument { context } = failing.next_chunk().unwrap_err() else {
            panic!("expected InvalidArgument");
        };
        assert_eq!(context, "upstream boom");
    }

    #[test]
    fn aggregate_every_function_groups_nulls_and_orders_keys() {
        let input = chunk(
            vec![LogicalType::String, LogicalType::Int64],
            vec![
                vec![Value::String("b".into()), Value::Null],
                vec![Value::String("a".into()), Value::Int64(2)],
                vec![Value::String("a".into()), Value::Null],
                vec![Value::String("b".into()), Value::Null],
                vec![Value::String("a".into()), Value::Int64(1)],
            ],
        );
        let value = Expr::Col("r.value".into());
        let mut aggregate = Aggregate::new(
            Box::new(VecSource::new(vec![input])),
            vec![Expr::Col("r.group".into())],
            vec![
                (AggregateFunction::Count, value.clone()),
                (AggregateFunction::Sum, value.clone()),
                (AggregateFunction::Min, value.clone()),
                (AggregateFunction::Max, value.clone()),
                (AggregateFunction::Avg, value),
            ],
            column_map(&[("r.group", 0), ("r.value", 1)]),
            vec![
                LogicalType::String,
                LogicalType::Int64,
                LogicalType::Int64,
                LogicalType::Int64,
                LogicalType::Int64,
                LogicalType::Float64,
            ],
            SpillConfig::unbounded(),
        );

        let output = aggregate.next_chunk().unwrap().unwrap();
        assert_eq!(output.row_count(), 2);
        assert_eq!(
            output.column(0).unwrap(),
            &[Value::String("a".into()), Value::String("b".into())]
        );
        assert_eq!(
            output.column(1).unwrap(),
            &[Value::Int64(2), Value::Int64(0)]
        );
        assert_eq!(output.column(2).unwrap(), &[Value::Int64(3), Value::Null]);
        assert_eq!(output.column(3).unwrap(), &[Value::Int64(1), Value::Null]);
        assert_eq!(output.column(4).unwrap(), &[Value::Int64(2), Value::Null]);
        assert_eq!(
            output.column(5).unwrap(),
            &[Value::Float64(1.5), Value::Null]
        );
        assert_eq!(output.types()[1], LogicalType::Int64);
        assert_eq!(output.types()[5], LogicalType::Float64);
    }

    #[test]
    fn aggregate_min_max_timestamp_skip_nulls() {
        let input = chunk(
            vec![LogicalType::Timestamp],
            vec![
                vec![Value::Timestamp(30)],
                vec![Value::Null],
                vec![Value::Timestamp(-20)],
                vec![Value::Timestamp(10)],
            ],
        );

        let output = aggregate_extremes(input, LogicalType::Timestamp);
        assert_eq!(output.column(0).unwrap(), &[Value::Timestamp(-20)]);
        assert_eq!(output.column(1).unwrap(), &[Value::Timestamp(30)]);
    }

    #[test]
    fn aggregate_min_max_equal_scale_decimal_skip_nulls() {
        let decimal_type = LogicalType::Decimal {
            precision: 4,
            scale: 2,
        };
        let input = chunk(
            vec![decimal_type],
            vec![
                vec![decimal(300, 2)],
                vec![Value::Null],
                vec![decimal(-250, 2)],
                vec![decimal(-125, 2)],
            ],
        );

        let output = aggregate_extremes(input, decimal_type);
        assert_eq!(output.column(0).unwrap(), &[decimal(-250, 2)]);
        assert_eq!(output.column(1).unwrap(), &[decimal(300, 2)]);
    }

    #[test]
    fn aggregate_decimal_sum_is_exact_across_multiple_chunks() {
        let decimal_type = LogicalType::Decimal {
            precision: 38,
            scale: 4,
        };
        let chunks = vec![
            chunk(
                vec![decimal_type],
                vec![vec![decimal(12_345, 4)], vec![Value::Null]],
            ),
            chunk(
                vec![decimal_type],
                vec![vec![decimal(-2_345, 4)], vec![decimal(50_005, 4)]],
            ),
        ];
        let mut aggregate = Aggregate::new(
            Box::new(VecSource::new(chunks)),
            vec![],
            vec![(AggregateFunction::Sum, Expr::Col("r.value".into()))],
            column_map(&[("r.value", 0)]),
            vec![decimal_type],
            SpillConfig::unbounded(),
        );

        let output = aggregate.next_chunk().unwrap().unwrap();
        assert_eq!(output.column(0).unwrap(), &[decimal(60_005, 4)]);
    }

    #[test]
    fn aggregate_decimal_sum_reports_intermediate_precision_38_overflow() {
        let decimal_type = LogicalType::Decimal {
            precision: 38,
            scale: 0,
        };
        let max_decimal = 10_i128.pow(38) - 1;
        let chunks = vec![
            chunk(vec![decimal_type], vec![vec![decimal(max_decimal, 0)]]),
            chunk(
                vec![decimal_type],
                vec![vec![decimal(1, 0)], vec![decimal(-1, 0)]],
            ),
        ];
        let mut aggregate = Aggregate::new(
            Box::new(VecSource::new(chunks)),
            vec![],
            vec![(AggregateFunction::Sum, Expr::Col("r.value".into()))],
            column_map(&[("r.value", 0)]),
            vec![decimal_type],
            SpillConfig::unbounded(),
        );

        let DevonError::InvalidArgument { context } = aggregate.next_chunk().unwrap_err() else {
            panic!("expected InvalidArgument");
        };
        assert_eq!(context, "aggregate `sum` overflowed Decimal precision 38");
    }

    #[test]
    fn aggregate_decimal_sum_mirrors_int_sum_for_nulls_and_empty_group() {
        let decimal_type = LogicalType::Decimal {
            precision: 38,
            scale: 2,
        };
        let aggregates = vec![
            (AggregateFunction::Sum, Expr::Col("r.decimal".into())),
            (AggregateFunction::Sum, Expr::Col("r.integer".into())),
        ];
        let columns = column_map(&[("r.decimal", 0), ("r.integer", 1)]);
        let output_types = vec![decimal_type, LogicalType::Int64];
        let mixed_nulls = chunk(
            vec![decimal_type, LogicalType::Int64],
            vec![
                vec![decimal(125, 2), Value::Int64(125)],
                vec![Value::Null, Value::Null],
                vec![decimal(-25, 2), Value::Int64(-25)],
            ],
        );
        let all_nulls = chunk(
            vec![decimal_type, LogicalType::Int64],
            vec![vec![Value::Null, Value::Null]],
        );
        let mut mixed = Aggregate::new(
            Box::new(VecSource::new(vec![mixed_nulls])),
            vec![],
            aggregates.clone(),
            columns.clone(),
            output_types.clone(),
            SpillConfig::unbounded(),
        );
        let mut only_nulls = Aggregate::new(
            Box::new(VecSource::new(vec![all_nulls])),
            vec![],
            aggregates.clone(),
            columns.clone(),
            output_types.clone(),
            SpillConfig::unbounded(),
        );
        let mut empty = Aggregate::new(
            Box::new(VecSource::new(vec![])),
            vec![],
            aggregates,
            columns,
            output_types,
            SpillConfig::unbounded(),
        );

        let mixed = mixed.next_chunk().unwrap().unwrap();
        assert_eq!(mixed.column(0).unwrap(), &[decimal(100, 2)]);
        assert_eq!(mixed.column(1).unwrap(), &[Value::Int64(100)]);
        for output in [&mut only_nulls, &mut empty] {
            let output = output.next_chunk().unwrap().unwrap();
            assert_eq!(output.column(0).unwrap(), &[Value::Null]);
            assert_eq!(output.column(1).unwrap(), &[Value::Null]);
        }
    }

    #[test]
    fn aggregate_grouped_decimal_sums_have_exact_known_answers() {
        let decimal_type = LogicalType::Decimal {
            precision: 38,
            scale: 2,
        };
        let chunks = vec![
            chunk(
                vec![LogicalType::String, decimal_type],
                vec![
                    vec![Value::String("b".into()), decimal(200, 2)],
                    vec![Value::String("a".into()), decimal(125, 2)],
                ],
            ),
            chunk(
                vec![LogicalType::String, decimal_type],
                vec![
                    vec![Value::String("a".into()), Value::Null],
                    vec![Value::String("b".into()), decimal(-50, 2)],
                    vec![Value::String("a".into()), decimal(275, 2)],
                ],
            ),
        ];
        let mut aggregate = Aggregate::new(
            Box::new(VecSource::new(chunks)),
            vec![Expr::Col("r.group".into())],
            vec![(AggregateFunction::Sum, Expr::Col("r.value".into()))],
            column_map(&[("r.group", 0), ("r.value", 1)]),
            vec![LogicalType::String, decimal_type],
            SpillConfig::unbounded(),
        );

        let output = aggregate.next_chunk().unwrap().unwrap();
        assert_eq!(
            output.column(0).unwrap(),
            &[Value::String("a".into()), Value::String("b".into())]
        );
        assert_eq!(
            output.column(1).unwrap(),
            &[decimal(400, 2), decimal(150, 2)]
        );
    }

    #[test]
    fn scalar_v2_aggregate_sum_and_avg_reject_non_numeric_variants() {
        let inputs = [
            (LogicalType::Timestamp, Value::Timestamp(0), "Timestamp"),
            (LogicalType::Bytes, Value::Bytes(vec![0]), "Bytes"),
            (LogicalType::Json, Value::Json(r#"{"a":1}"#.into()), "Json"),
        ];

        for (function, function_name) in [
            (AggregateFunction::Sum, "sum"),
            (AggregateFunction::Avg, "avg"),
        ] {
            for (logical_type, value, variant) in &inputs {
                let input = chunk(vec![*logical_type], vec![vec![value.clone()]]);
                let mut aggregate = Aggregate::new(
                    Box::new(VecSource::new(vec![input])),
                    vec![],
                    vec![(function, Expr::Col("r.value".into()))],
                    column_map(&[("r.value", 0)]),
                    vec![LogicalType::Float64],
                    SpillConfig::unbounded(),
                );
                let DevonError::InvalidArgument { context } = aggregate.next_chunk().unwrap_err()
                else {
                    panic!("expected InvalidArgument");
                };
                assert_eq!(
                    context,
                    format!("aggregate `{function_name}` does not accept input type {variant}")
                );
            }
        }
    }

    #[test]
    fn bytes_and_json_min_max_reject_even_single_non_null_input() {
        for (logical_type, value, variant) in [
            (LogicalType::Bytes, Value::Bytes(vec![0]), "Bytes"),
            (LogicalType::Json, Value::Json(r#"{"a":1}"#.into()), "Json"),
        ] {
            for function in [AggregateFunction::Min, AggregateFunction::Max] {
                let input = chunk(vec![logical_type], vec![vec![value.clone()]]);
                let mut aggregate = Aggregate::new(
                    Box::new(VecSource::new(vec![input])),
                    vec![],
                    vec![(function, Expr::Col("r.value".into()))],
                    column_map(&[("r.value", 0)]),
                    vec![logical_type],
                    SpillConfig::unbounded(),
                );
                let DevonError::Corrupt { context } = aggregate.next_chunk().unwrap_err() else {
                    panic!("expected Corrupt");
                };
                assert_eq!(
                    context,
                    format!("cannot order values of types {variant} and {variant}")
                );
            }
        }
    }

    #[test]
    fn aggregate_null_and_bit_identical_nan_keys_group_deterministically() {
        let nan_one = f64::from_bits(0x7ff8_0000_0000_0001);
        let nan_two = f64::from_bits(0x7ff8_0000_0000_0002);
        let input = chunk(
            vec![LogicalType::Float64, LogicalType::Int64],
            vec![
                vec![Value::Float64(nan_one), Value::Int64(1)],
                vec![Value::Null, Value::Int64(1)],
                vec![Value::Float64(0.0), Value::Int64(1)],
                vec![Value::Float64(-0.0), Value::Int64(1)],
                vec![Value::Float64(nan_one), Value::Int64(1)],
                vec![Value::Null, Value::Int64(1)],
                vec![Value::Float64(0.0), Value::Int64(1)],
                vec![Value::Float64(nan_two), Value::Int64(1)],
            ],
        );
        let mut aggregate = Aggregate::new(
            Box::new(VecSource::new(vec![input])),
            vec![Expr::Col("r.key".into())],
            vec![(AggregateFunction::Count, Expr::Col("r.value".into()))],
            column_map(&[("r.key", 0), ("r.value", 1)]),
            vec![LogicalType::Float64, LogicalType::Int64],
            SpillConfig::unbounded(),
        );

        let output = aggregate.next_chunk().unwrap().unwrap();
        assert_eq!(output.row_count(), 5);
        assert_eq!(output.value(0, 0), Some(Value::Null));
        let key_bits = (1..5)
            .map(|row| match output.value(row, 0).unwrap() {
                Value::Float64(value) => value.to_bits(),
                value => panic!("expected Float64, got {value}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            key_bits,
            vec![
                (-0.0_f64).to_bits(),
                0.0_f64.to_bits(),
                nan_one.to_bits(),
                nan_two.to_bits(),
            ]
        );
        assert_eq!(
            output.column(1).unwrap(),
            &[
                Value::Int64(2),
                Value::Int64(1),
                Value::Int64(2),
                Value::Int64(2),
                Value::Int64(1),
            ]
        );
    }

    #[test]
    fn aggregate_empty_group_by_emits_one_row_for_empty_and_nonempty_input() {
        let aggregates = vec![
            (AggregateFunction::Count, Expr::Col("r.value".into())),
            (AggregateFunction::Sum, Expr::Col("r.value".into())),
            (AggregateFunction::Avg, Expr::Col("r.value".into())),
        ];
        let output_types = vec![LogicalType::Int64, LogicalType::Int64, LogicalType::Float64];
        let mut empty = Aggregate::new(
            Box::new(VecSource::new(vec![])),
            vec![],
            aggregates.clone(),
            column_map(&[("r.value", 0)]),
            output_types.clone(),
            SpillConfig::unbounded(),
        );
        let mut nonempty = Aggregate::new(
            Box::new(VecSource::new(vec![int_chunk([2, 4])])),
            vec![],
            aggregates,
            column_map(&[("r.value", 0)]),
            output_types,
            SpillConfig::unbounded(),
        );

        let empty = empty.next_chunk().unwrap().unwrap();
        assert_eq!(empty.row_count(), 1);
        assert_eq!(empty.column(0).unwrap(), &[Value::Int64(0)]);
        assert_eq!(empty.column(1).unwrap(), &[Value::Null]);
        assert_eq!(empty.column(2).unwrap(), &[Value::Null]);
        let nonempty = nonempty.next_chunk().unwrap().unwrap();
        assert_eq!(nonempty.column(0).unwrap(), &[Value::Int64(2)]);
        assert_eq!(nonempty.column(1).unwrap(), &[Value::Int64(6)]);
        assert_eq!(nonempty.column(2).unwrap(), &[Value::Float64(3.0)]);
    }

    #[test]
    fn ungrouped_streaming_matches_buffered_for_every_function_with_nulls_and_empty_input() {
        let inputs = [
            Vec::new(),
            vec![chunk(
                vec![LogicalType::Int64],
                vec![vec![Value::Null], vec![Value::Null]],
            )],
            vec![
                chunk(
                    vec![LogicalType::Int64],
                    vec![vec![Value::Null], vec![Value::Int64(3)]],
                ),
                int_chunk([-1, 8]),
            ],
        ];
        let value = Expr::Col("r.value".into());
        let aggregates = vec![
            (AggregateFunction::Count, value.clone()),
            (AggregateFunction::Sum, value.clone()),
            (AggregateFunction::Min, value.clone()),
            (AggregateFunction::Max, value.clone()),
            (AggregateFunction::Avg, value),
        ];
        let output_types = vec![
            LogicalType::Int64,
            LogicalType::Int64,
            LogicalType::Int64,
            LogicalType::Int64,
            LogicalType::Float64,
        ];

        for chunks in inputs {
            let mut streaming = Aggregate::new(
                Box::new(VecSource::new(chunks.clone())),
                vec![],
                aggregates.clone(),
                column_map(&[("r.value", 0)]),
                output_types.clone(),
                SpillConfig::unbounded(),
            );
            let mut buffered = Aggregate::new(
                Box::new(VecSource::new(chunks)),
                vec![],
                aggregates.clone(),
                column_map(&[("r.value", 0)]),
                output_types.clone(),
                SpillConfig::unbounded(),
            );

            let actual = collect_chunks(&mut streaming);
            buffered.initialize_buffered().unwrap();
            let expected = collect_chunks(&mut buffered);
            assert_eq!(actual, expected);
            assert!(streaming.spill_files.paths.is_empty());
        }
    }

    #[test]
    fn ungrouped_streaming_succeeds_below_one_row_charge_while_grouped_fails() {
        let directory = SpillDirectory::new();
        let input = int_chunk([1, 2, 3]);
        let value = Expr::Col("r.value".into());
        let streaming_config = directory.config(1);
        let streaming_budget = Arc::clone(&streaming_config.budget);
        let mut streaming = Aggregate::new(
            Box::new(VecSource::new(vec![input.clone()])),
            vec![],
            vec![(AggregateFunction::Count, value.clone())],
            column_map(&[("r.value", 0)]),
            vec![LogicalType::Int64],
            streaming_config,
        );

        let output = streaming.next_chunk().unwrap().unwrap();
        assert_eq!(output.column(0).unwrap(), &[Value::Int64(3)]);
        assert_eq!(streaming_budget.charged(), 0);
        assert!(streaming.spill_files.paths.is_empty());
        assert!(!directory.path.exists());

        let mut grouped = Aggregate::new(
            Box::new(VecSource::new(vec![input])),
            vec![value.clone()],
            vec![(AggregateFunction::Count, value)],
            column_map(&[("r.value", 0)]),
            vec![LogicalType::Int64, LogicalType::Int64],
            directory.config(1),
        );
        let DevonError::BudgetExceeded { context } = grouped.next_chunk().unwrap_err() else {
            panic!("expected BudgetExceeded");
        };
        assert!(context.contains("Aggregate input row"));
    }

    #[test]
    fn aggregate_nonempty_group_by_over_empty_input_emits_no_rows() {
        let mut aggregate = Aggregate::new(
            Box::new(VecSource::new(vec![])),
            vec![Expr::Col("r.group".into())],
            vec![(AggregateFunction::Count, Expr::Col("r.value".into()))],
            column_map(&[("r.group", 0), ("r.value", 1)]),
            vec![LogicalType::String, LogicalType::Int64],
            SpillConfig::unbounded(),
        );

        assert!(aggregate.next_chunk().unwrap().is_none());
    }

    #[test]
    fn aggregate_int_sum_overflow_is_invalid_and_names_sum() {
        let mut aggregate = Aggregate::new(
            Box::new(VecSource::new(vec![int_chunk([i64::MAX, 1])])),
            vec![],
            vec![(AggregateFunction::Sum, Expr::Col("r.value".into()))],
            column_map(&[("r.value", 0)]),
            vec![LogicalType::Int64],
            SpillConfig::unbounded(),
        );

        let DevonError::InvalidArgument { context } = aggregate.next_chunk().unwrap_err() else {
            panic!("expected InvalidArgument");
        };
        assert!(context.contains("sum"));
        assert!(context.contains("overflow"));
    }

    #[test]
    fn aggregate_multiple_functions_and_group_keys_have_known_answers() {
        let input = chunk(
            vec![LogicalType::String, LogicalType::Bool, LogicalType::Float64],
            vec![
                vec![
                    Value::String("b".into()),
                    Value::Bool(false),
                    Value::Float64(2.0),
                ],
                vec![
                    Value::String("a".into()),
                    Value::Bool(true),
                    Value::Float64(3.0),
                ],
                vec![
                    Value::String("a".into()),
                    Value::Bool(false),
                    Value::Float64(1.0),
                ],
                vec![
                    Value::String("a".into()),
                    Value::Bool(false),
                    Value::Float64(4.0),
                ],
            ],
        );
        let value = Expr::Col("r.value".into());
        let mut aggregate = Aggregate::new(
            Box::new(VecSource::new(vec![input])),
            vec![Expr::Col("r.name".into()), Expr::Col("r.flag".into())],
            vec![
                (AggregateFunction::Count, value.clone()),
                (AggregateFunction::Sum, value.clone()),
                (AggregateFunction::Avg, value),
            ],
            column_map(&[("r.name", 0), ("r.flag", 1), ("r.value", 2)]),
            vec![
                LogicalType::String,
                LogicalType::Bool,
                LogicalType::Int64,
                LogicalType::Float64,
                LogicalType::Float64,
            ],
            SpillConfig::unbounded(),
        );

        let output = aggregate.next_chunk().unwrap().unwrap();
        assert_eq!(
            output.column(0).unwrap(),
            &[
                Value::String("a".into()),
                Value::String("a".into()),
                Value::String("b".into()),
            ]
        );
        assert_eq!(
            output.column(1).unwrap(),
            &[Value::Bool(false), Value::Bool(true), Value::Bool(false),]
        );
        assert_eq!(
            output.column(2).unwrap(),
            &[Value::Int64(2), Value::Int64(1), Value::Int64(1)]
        );
        assert_eq!(
            output.column(3).unwrap(),
            &[
                Value::Float64(5.0),
                Value::Float64(3.0),
                Value::Float64(2.0),
            ]
        );
        assert_eq!(
            output.column(4).unwrap(),
            &[
                Value::Float64(2.5),
                Value::Float64(3.0),
                Value::Float64(2.0),
            ]
        );
    }

    #[test]
    fn aggregate_output_spans_capacity_and_uses_supplied_types() {
        let row_count = CHUNK_CAPACITY + 2;
        let values = (0..row_count as i64).rev().collect::<Vec<_>>();
        let chunks = values
            .chunks(CHUNK_CAPACITY)
            .map(|values| {
                chunk(
                    vec![LogicalType::Int64, LogicalType::Int64],
                    values
                        .iter()
                        .map(|value| vec![Value::Int64(*value), Value::Int64(1)])
                        .collect(),
                )
            })
            .collect();
        let mut aggregate = Aggregate::new(
            Box::new(VecSource::new(chunks)),
            vec![Expr::Col("r.group".into())],
            vec![(AggregateFunction::Count, Expr::Col("r.value".into()))],
            column_map(&[("r.group", 0), ("r.value", 1)]),
            vec![LogicalType::Int64, LogicalType::Int64],
            SpillConfig::unbounded(),
        );

        let chunks = collect_chunks(&mut aggregate);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].row_count(), CHUNK_CAPACITY);
        assert_eq!(chunks[1].row_count(), 2);
        assert_eq!(chunks[0].types(), &[LogicalType::Int64, LogicalType::Int64]);
        assert_eq!(chunks[0].value(0, 0), Some(Value::Int64(0)));
        assert_eq!(
            chunks[1].column(0).unwrap(),
            &[
                Value::Int64(CHUNK_CAPACITY as i64),
                Value::Int64(CHUNK_CAPACITY as i64 + 1),
            ]
        );
    }

    #[test]
    fn aggregate_supplied_type_mismatch_and_arity_are_validated() {
        let mut wrong_type = Aggregate::new(
            Box::new(VecSource::new(vec![])),
            vec![],
            vec![(AggregateFunction::Count, Expr::Col("r.value".into()))],
            column_map(&[("r.value", 0)]),
            vec![LogicalType::Float64],
            SpillConfig::unbounded(),
        );
        let DevonError::InvalidArgument { context } = wrong_type.next_chunk().unwrap_err() else {
            panic!("expected InvalidArgument");
        };
        assert!(context.contains("does not match expected type Float64"));

        let mut wrong_type_again = Aggregate::new(
            Box::new(VecSource::new(vec![])),
            vec![],
            vec![(AggregateFunction::Count, Expr::Col("r.value".into()))],
            column_map(&[("r.value", 0)]),
            vec![LogicalType::Float64],
            SpillConfig::unbounded(),
        );
        let DevonError::InvalidArgument { context } = wrong_type_again.next_chunk().unwrap_err()
        else {
            panic!("expected InvalidArgument");
        };
        assert!(context.contains("does not match expected type Float64"));

        let mut wrong_arity = Aggregate::new(
            Box::new(VecSource::new(vec![])),
            vec![],
            vec![(AggregateFunction::Count, Expr::Col("r.value".into()))],
            column_map(&[("r.value", 0)]),
            vec![],
            SpillConfig::unbounded(),
        );
        let first_arity_error = wrong_arity.next_chunk().unwrap_err();
        assert_eq!(
            format!("{first_arity_error}"),
            format!("{}", wrong_arity.next_chunk().unwrap_err())
        );

        let mut wrong_arity_again = Aggregate::new(
            Box::new(VecSource::new(vec![])),
            vec![],
            vec![(AggregateFunction::Count, Expr::Col("r.value".into()))],
            column_map(&[("r.value", 0)]),
            vec![],
            SpillConfig::unbounded(),
        );
        let error = wrong_arity_again
            .next_chunk()
            .expect_err("expected an aggregate arity validation error");
        let DevonError::InvalidArgument { context } = error else {
            panic!("expected InvalidArgument");
        };
        assert!(context.contains("1 outputs"));
        assert!(context.contains("0 output types"));
    }

    #[test]
    fn aggregate_min_mixed_variants_are_corrupt_and_name_both_types() {
        let float_chunk = chunk(vec![LogicalType::Float64], vec![vec![Value::Float64(1.0)]]);
        let mut aggregate = Aggregate::new(
            Box::new(VecSource::new(vec![int_chunk([1]), float_chunk])),
            vec![],
            vec![(AggregateFunction::Min, Expr::Col("r.value".into()))],
            column_map(&[("r.value", 0)]),
            vec![LogicalType::Int64],
            SpillConfig::unbounded(),
        );

        let DevonError::Corrupt { context } = aggregate.next_chunk().unwrap_err() else {
            panic!("expected Corrupt");
        };
        assert!(context.contains("Int64"));
        assert!(context.contains("Float64"));
    }

    #[test]
    fn aggregate_is_blocking_and_propagates_upstream_errors() {
        let pulls = Rc::new(Cell::new(0));
        let source = CountingSource {
            chunks: vec![int_chunk([1]), int_chunk([2])].into(),
            pulls: Rc::clone(&pulls),
        };
        let mut aggregate = Aggregate::new(
            Box::new(source),
            vec![],
            vec![(AggregateFunction::Count, Expr::Col("r.value".into()))],
            column_map(&[("r.value", 0)]),
            vec![LogicalType::Int64],
            SpillConfig::unbounded(),
        );
        assert!(aggregate.next_chunk().unwrap().is_some());
        assert_eq!(pulls.get(), 3);

        let mut failing = Aggregate::new(
            Box::new(ErrorSource {
                first: Some(int_chunk([1])),
            }),
            vec![],
            vec![(AggregateFunction::Count, Expr::Col("r.value".into()))],
            column_map(&[("r.value", 0)]),
            vec![LogicalType::Int64],
            SpillConfig::unbounded(),
        );
        let DevonError::InvalidArgument { context } = failing.next_chunk().unwrap_err() else {
            panic!("expected InvalidArgument");
        };
        assert_eq!(context, "upstream boom");
    }

    #[test]
    fn spill_sort_matches_in_memory_output_and_preserves_global_stability() {
        let directory = SpillDirectory::new();
        let row_count = 5_000_i64;
        let input_rows = (0..row_count)
            .map(|sequence| vec![Value::Int64(sequence % 3), Value::Int64(sequence)])
            .collect::<Vec<_>>();
        let chunks = input_rows
            .chunks(CHUNK_CAPACITY)
            .map(|rows| chunk(vec![LogicalType::Int64, LogicalType::Int64], rows.to_vec()))
            .collect::<Vec<_>>();
        let keys = vec![(Expr::Col("r.key".into()), SortOrder::Asc)];
        let columns = column_map(&[("r.key", 0)]);
        let mut in_memory = Sort::new(
            Box::new(VecSource::new(chunks.clone())),
            keys.clone(),
            columns.clone(),
            SpillConfig::unbounded(),
        );
        let config = directory.config(500_000);
        let budget = Arc::clone(&config.budget);
        let mut spilled = Sort::new(Box::new(VecSource::new(chunks)), keys, columns, config);

        let expected = collect_chunks(&mut in_memory);
        let actual = collect_chunks(&mut spilled);

        assert_eq!(actual, expected);
        assert!(spilled.spill_files.paths.len() >= 2);
        let tied_sequences = actual
            .iter()
            .flat_map(|chunk| chunk.rows())
            .filter_map(|row| match (&row[0], &row[1]) {
                (Value::Int64(1), Value::Int64(sequence)) => Some(*sequence),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(tied_sequences.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(budget.charged(), 0);
    }

    #[test]
    fn spill_external_aggregate_matches_every_in_memory_function() {
        let directory = SpillDirectory::new();
        let input_rows = (0..5_000_i64)
            .map(|index| {
                vec![
                    Value::String(format!("g{}", index % 7)),
                    if index % 5 == 0 {
                        Value::Null
                    } else {
                        Value::Int64(index % 13)
                    },
                ]
            })
            .collect::<Vec<_>>();
        let chunks = input_rows
            .chunks(CHUNK_CAPACITY)
            .map(|rows| chunk(vec![LogicalType::String, LogicalType::Int64], rows.to_vec()))
            .collect::<Vec<_>>();
        let value = Expr::Col("r.value".into());
        let aggregates = vec![
            (AggregateFunction::Count, value.clone()),
            (AggregateFunction::Sum, value.clone()),
            (AggregateFunction::Min, value.clone()),
            (AggregateFunction::Max, value.clone()),
            (AggregateFunction::Avg, value),
        ];
        let output_types = vec![
            LogicalType::String,
            LogicalType::Int64,
            LogicalType::Int64,
            LogicalType::Int64,
            LogicalType::Int64,
            LogicalType::Float64,
        ];
        let columns = column_map(&[("r.group", 0), ("r.value", 1)]);
        let mut in_memory = Aggregate::new(
            Box::new(VecSource::new(chunks.clone())),
            vec![Expr::Col("r.group".into())],
            aggregates.clone(),
            columns.clone(),
            output_types.clone(),
            SpillConfig::unbounded(),
        );
        let config = directory.config(800_000);
        let budget = Arc::clone(&config.budget);
        let mut spilled = Aggregate::new(
            Box::new(VecSource::new(chunks)),
            vec![Expr::Col("r.group".into())],
            aggregates,
            columns,
            output_types,
            config,
        );

        assert_eq!(collect_chunks(&mut spilled), collect_chunks(&mut in_memory));
        assert!(spilled.spill_files.paths.len() >= 2);
        assert_eq!(budget.charged(), 0);
    }

    #[test]
    fn streaming_aggregate_emits_one_row_without_spilling_and_checks_overflow() {
        let directory = SpillDirectory::new();
        let chunks = (0..5_000_i64)
            .collect::<Vec<_>>()
            .chunks(CHUNK_CAPACITY)
            .map(|values| int_chunk(values.iter().copied()))
            .collect::<Vec<_>>();
        let value = Expr::Col("r.value".into());
        let mut aggregate = Aggregate::new(
            Box::new(VecSource::new(chunks)),
            vec![],
            vec![
                (AggregateFunction::Count, value.clone()),
                (AggregateFunction::Sum, value.clone()),
                (AggregateFunction::Avg, value),
            ],
            column_map(&[("r.value", 0)]),
            vec![LogicalType::Int64, LogicalType::Int64, LogicalType::Float64],
            directory.config(1),
        );

        let output = aggregate.next_chunk().unwrap().unwrap();
        assert_eq!(output.row_count(), 1);
        assert_eq!(output.value(0, 0), Some(Value::Int64(5_000)));
        assert_eq!(output.value(0, 1), Some(Value::Int64(12_497_500)));
        assert_eq!(output.value(0, 2), Some(Value::Float64(2_499.5)));
        assert!(aggregate.spill_files.paths.is_empty());
        assert!(!directory.path.exists());

        let mut overflow_values = vec![i64::MAX, 1];
        overflow_values.extend(std::iter::repeat_n(0, 4_998));
        let overflow_chunks = overflow_values
            .chunks(CHUNK_CAPACITY)
            .map(|values| int_chunk(values.iter().copied()))
            .collect();
        let mut overflow = Aggregate::new(
            Box::new(VecSource::new(overflow_chunks)),
            vec![],
            vec![(AggregateFunction::Sum, Expr::Col("r.value".into()))],
            column_map(&[("r.value", 0)]),
            vec![LogicalType::Int64],
            directory.config(1),
        );
        let DevonError::InvalidArgument { context } = overflow.next_chunk().unwrap_err() else {
            panic!("expected InvalidArgument");
        };
        assert!(context.contains("sum"));
        assert!(context.contains("overflow"));
        assert!(overflow.spill_files.paths.is_empty());
        assert!(!directory.path.exists());
    }

    #[test]
    fn percentile_cont_skips_nulls_orders_digits_and_handles_empty_singleton_and_even_inputs() {
        let ty = LogicalType::Decimal {
            precision: 10,
            scale: 2,
        };
        let cases = [
            (Vec::new(), Value::Null),
            (vec![Value::Null], Value::Null),
            (vec![decimal(725, 2)], decimal(725, 2)),
            (
                vec![
                    decimal(500, 2),
                    Value::Null,
                    decimal(100, 2),
                    decimal(300, 2),
                ],
                decimal(300, 2),
            ),
            (vec![decimal(100, 2), decimal(300, 2)], decimal(200, 2)),
        ];
        for (values, expected) in cases {
            let chunks = if values.is_empty() {
                Vec::new()
            } else {
                vec![chunk(
                    vec![ty],
                    values.into_iter().map(|value| vec![value]).collect(),
                )]
            };
            let mut aggregate = Aggregate::new(
                Box::new(VecSource::new(chunks)),
                Vec::new(),
                vec![(
                    AggregateFunction::PercentileCont,
                    Expr::Col("r.value".into()),
                )],
                column_map(&[("r.value", 0)]),
                vec![ty],
                SpillConfig::unbounded(),
            );
            let output = aggregate.next_chunk().unwrap().unwrap();
            assert_eq!(output.value(0, 0), Some(expected));
        }
    }

    #[test]
    fn percentile_cont_rejects_unrepresentable_interpolation() {
        let ty = LogicalType::Decimal {
            precision: 10,
            scale: 2,
        };
        let input = chunk(vec![ty], vec![vec![decimal(100, 2)], vec![decimal(101, 2)]]);
        let mut aggregate = Aggregate::new(
            Box::new(VecSource::new(vec![input])),
            Vec::new(),
            vec![(
                AggregateFunction::PercentileCont,
                Expr::Col("r.value".into()),
            )],
            column_map(&[("r.value", 0)]),
            vec![ty],
            SpillConfig::unbounded(),
        );

        let DevonError::InvalidArgument { context } = aggregate.next_chunk().unwrap_err() else {
            panic!("expected InvalidArgument");
        };
        assert!(context.contains("not representable"));
        assert!(context.contains("scale 2"));
    }

    #[test]
    fn percentile_cont_is_buffered_and_spills_under_budget_pressure() {
        assert!(!has_constant_aggregate_state(
            AggregateFunction::PercentileCont
        ));
        let directory = SpillDirectory::new();
        let ty = LogicalType::Decimal {
            precision: 10,
            scale: 2,
        };
        let rows = (0..400_i128)
            .map(|value| vec![decimal(value * 2, 2), decimal(1_000 - value * 2, 2)])
            .collect::<Vec<_>>();
        let config = directory.config(4_000);
        let budget = Arc::clone(&config.budget);
        let mut aggregate = Aggregate::new(
            Box::new(VecSource::new(vec![chunk(vec![ty, ty], rows)])),
            Vec::new(),
            vec![
                (
                    AggregateFunction::PercentileCont,
                    Expr::Col("r.first".into()),
                ),
                (
                    AggregateFunction::PercentileCont,
                    Expr::Col("r.second".into()),
                ),
            ],
            column_map(&[("r.first", 0), ("r.second", 1)]),
            vec![ty, ty],
            config,
        );

        let output = aggregate.next_chunk().unwrap().unwrap();
        assert_eq!(output.value(0, 0), Some(decimal(399, 2)));
        assert_eq!(output.value(0, 1), Some(decimal(601, 2)));
        assert!(aggregate.spill_files.paths.len() >= 4);
        assert_eq!(budget.charged(), 0);
    }

    #[test]
    fn scalar_subquery_enforces_zero_one_and_many_row_cardinality() {
        for (table, expected) in [("zero", Value::Null), ("one", Value::Int64(7))] {
            let executor = Arc::new(TestScalarExecutor {
                executions: Arc::new(AtomicU64::new(0)),
                budget: Arc::new(MemoryBudget::unlimited()),
            });
            let mut project = Project::new(
                Box::new(VecSource::new(vec![int_chunk([1])])),
                vec![(scalar_scan(table), "value".into())],
                column_map(&[("o.value", 0)]),
                vec![LogicalType::Int64],
            )
            .unwrap()
            .with_scalar_executor(executor);
            let output = project.next_chunk().unwrap().unwrap();
            assert_eq!(output.value(0, 0), Some(expected));
        }

        let executor = Arc::new(TestScalarExecutor {
            executions: Arc::new(AtomicU64::new(0)),
            budget: Arc::new(MemoryBudget::unlimited()),
        });
        let mut project = Project::new(
            Box::new(VecSource::new(vec![int_chunk([1])])),
            vec![(scalar_scan("many"), "value".into())],
            column_map(&[("o.value", 0)]),
            vec![LogicalType::Int64],
        )
        .unwrap()
        .with_scalar_executor(executor);
        let DevonError::InvalidArgument { context } = project.next_chunk().unwrap_err() else {
            panic!("expected InvalidArgument");
        };
        assert!(context.contains("more than one row"));
    }

    #[test]
    fn scalar_subquery_correlates_per_row_and_caches_uncorrelated_execution() {
        let executions = Arc::new(AtomicU64::new(0));
        let budget = Arc::new(MemoryBudget::unlimited());
        let executor = Arc::new(TestScalarExecutor {
            executions: Arc::clone(&executions),
            budget: Arc::clone(&budget),
        });
        let mut project = Project::new(
            Box::new(VecSource::new(vec![int_chunk([2, 5]), int_chunk([9])])),
            vec![
                (scalar_scan("one"), "constant".into()),
                (correlated_scalar(), "correlated".into()),
            ],
            column_map(&[("o.value", 0)]),
            vec![LogicalType::Int64, LogicalType::Int64],
        )
        .unwrap()
        .with_scalar_executor(executor);

        let first = project.next_chunk().unwrap().unwrap();
        assert_eq!(
            first.column(0).unwrap(),
            &[Value::Int64(7), Value::Int64(7)]
        );
        assert_eq!(
            first.column(1).unwrap(),
            &[Value::Int64(20), Value::Int64(50)]
        );
        let second = project.next_chunk().unwrap().unwrap();
        assert_eq!(second.column(0).unwrap(), &[Value::Int64(7)]);
        assert_eq!(second.column(1).unwrap(), &[Value::Int64(90)]);
        assert!(project.next_chunk().unwrap().is_none());
        assert_eq!(executions.load(AtomicOrdering::Relaxed), 4);
        assert!(budget.charged() > 0);
        drop(project);
        assert_eq!(budget.charged(), 0);
    }

    #[test]
    fn scalar_subquery_obeys_if_and_coalesce_lazy_branch_suppression() {
        let executions = Arc::new(AtomicU64::new(0));
        let executor = Arc::new(TestScalarExecutor {
            executions: Arc::clone(&executions),
            budget: Arc::new(MemoryBudget::unlimited()),
        });
        let mut project = Project::new(
            Box::new(VecSource::new(vec![int_chunk([1, 2])])),
            vec![
                (
                    Expr::If {
                        cond: Box::new(Expr::Lit(Value::Bool(false))),
                        then_expr: Box::new(scalar_scan("many")),
                        else_expr: Box::new(Expr::Lit(Value::Int64(11))),
                    },
                    "if_value".into(),
                ),
                (
                    Expr::Coalesce(vec![Expr::Lit(Value::Int64(12)), scalar_scan("many")]),
                    "coalesce_value".into(),
                ),
            ],
            column_map(&[("o.value", 0)]),
            vec![LogicalType::Int64, LogicalType::Int64],
        )
        .unwrap()
        .with_scalar_executor(executor);

        let output = project.next_chunk().unwrap().unwrap();
        assert_eq!(
            output.column(0).unwrap(),
            &[Value::Int64(11), Value::Int64(11)]
        );
        assert_eq!(
            output.column(1).unwrap(),
            &[Value::Int64(12), Value::Int64(12)]
        );
        assert_eq!(executions.load(AtomicOrdering::Relaxed), 0);
    }

    #[test]
    fn spill_corrupt_run_short_length_bad_length_and_envelope_fail_cleanly() {
        let directory = SpillDirectory::new();
        let corruptions = [
            vec![1],
            0_u32.to_le_bytes().to_vec(),
            [3_u32.to_le_bytes().as_slice(), b"xxx"].concat(),
        ];
        for corruption in corruptions {
            let mut sort = Sort::new(
                Box::new(VecSource::new(vec![int_chunk([3, 2, 1])])),
                vec![(Expr::Col("r.key".into()), SortOrder::Asc)],
                column_map(&[("r.key", 0)]),
                directory.config(100),
            );
            sort.initialize().unwrap();
            let path = sort.spill_files.paths[0].clone();
            fs::write(&path, corruption).unwrap();

            let DevonError::Corrupt { context } = sort.next_chunk().unwrap_err() else {
                panic!("expected Corrupt");
            };
            assert!(context.contains("spill file"));
        }
    }

    #[test]
    fn spill_files_follow_the_binding_name_and_are_deleted_on_operator_drop() {
        let directory = SpillDirectory::new();
        let mut sort = Sort::new(
            Box::new(VecSource::new(vec![int_chunk([3, 2, 1])])),
            vec![(Expr::Col("r.key".into()), SortOrder::Asc)],
            column_map(&[("r.key", 0)]),
            directory.config(100),
        );
        sort.initialize().unwrap();
        let paths = sort.spill_files.paths.clone();
        assert!(!paths.is_empty());
        for (index, path) in paths.iter().enumerate() {
            let name = path.file_name().unwrap().to_string_lossy();
            assert!(name.starts_with(&format!("sort-{}-", std::process::id())));
            assert!(name.ends_with(&format!("-{index}.run")));
            assert!(path.exists());
        }

        drop(sort);

        assert!(paths.iter().all(|path| !path.exists()));
    }

    #[test]
    fn spill_row_over_the_write_bound_refuses_honestly_before_any_bytes_land() {
        let mut sink = Vec::new();
        let row = vec![Value::String("x".repeat(64))];

        let error = super::write_spill_row_bounded(&mut sink, &row, 64).unwrap_err();

        let DevonError::InvalidArgument { context } = error else {
            panic!("expected InvalidArgument, got: {error:?}");
        };
        assert!(
            context.contains("spillable row range is 2..=64 bytes"),
            "{context}"
        );
        assert!(sink.is_empty());
    }

    #[test]
    fn spill_row_over_the_real_reader_cap_is_refused_at_write_time() {
        // A row whose encoded payload exceeds MAX_SPILL_ROW_BYTES must be
        // rejected before it reaches the merge reader.
        let mut sink = Vec::new();
        let row = vec![Value::String("x".repeat(super::MAX_SPILL_ROW_BYTES))];

        let error = super::write_spill_row(&mut sink, &row).unwrap_err();

        assert!(
            matches!(error, DevonError::InvalidArgument { .. }),
            "expected InvalidArgument, got: {error:?}"
        );
        assert!(sink.is_empty());
    }

    #[test]
    fn spill_row_at_the_write_bound_round_trips() {
        let directory = SpillDirectory::new();
        fs::create_dir_all(&directory.path).unwrap();
        let path = directory.path.join("bounded.run");
        let row = vec![Value::Int64(42), Value::String("abc".to_owned())];
        let encoded_len = super::encode_spill_row(&row).unwrap().len();
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        super::write_spill_row_bounded(&mut file, &row, encoded_len).unwrap();
        drop(file);

        let mut reader = super::SpillReader::open(path).unwrap();
        assert_eq!(reader.read_row().unwrap(), Some(row));
        assert!(reader.read_row().unwrap().is_none());
    }

    #[test]
    fn spill_run_creation_retries_past_a_leftover_name_collision() {
        let directory = SpillDirectory::new();
        let mut spill_files = super::SpillFiles::new(&directory.path);
        spill_files.write_run(vec![vec![Value::Int64(1)]]).unwrap();
        let first = spill_files.paths()[0].clone();
        // Fabricate a crash leftover at the exact name the next run would use.
        let first_name = first.file_name().unwrap().to_string_lossy();
        let bait_name = first_name.replace("-0.run", "-1.run");
        assert_ne!(first_name, bait_name);
        let bait = directory.path.join(&bait_name);
        fs::write(&bait, b"leftover").unwrap();

        spill_files.write_run(vec![vec![Value::Int64(2)]]).unwrap();

        let second = spill_files.paths()[1].clone();
        assert_ne!(second, bait);
        let mut reader = super::SpillReader::open(second).unwrap();
        assert_eq!(reader.read_row().unwrap(), Some(vec![Value::Int64(2)]));
        // The leftover was never touched, let alone clobbered.
        assert_eq!(fs::read(&bait).unwrap(), b"leftover");
    }

    #[test]
    #[ignore = "manual timing evidence: run with --release -- --ignored --nocapture"]
    fn timed_multi_run_merge_evidence() {
        use std::time::Instant;

        const TOTAL_ROWS: i64 = 200_000;
        const LIMIT: usize = 64 * 1024;

        let directory = SpillDirectory::new();
        let chunks = (0..TOTAL_ROWS)
            .rev()
            .collect::<Vec<_>>()
            .chunks(CHUNK_CAPACITY)
            .map(|values| int_chunk(values.iter().copied()))
            .collect::<Vec<_>>();
        let mut sort = Sort::new(
            Box::new(VecSource::new(chunks)),
            vec![(Expr::Col("r.key".into()), SortOrder::Asc)],
            column_map(&[("r.key", 0)]),
            directory.config(LIMIT),
        );
        let spill_start = Instant::now();
        sort.initialize().unwrap();
        let spill_elapsed = spill_start.elapsed();
        let run_count = sort.spill_files.paths.len();

        let merge_start = Instant::now();
        let mut rows = 0_i64;
        let mut previous = i64::MIN;
        while let Some(chunk) = sort.next_chunk().unwrap() {
            let column = chunk.column(0).unwrap();
            for row in 0..chunk.row_count() {
                let value = column.value_at(row);
                let Value::Int64(int) = &value else {
                    panic!("unexpected value {value:?}");
                };
                assert!(*int >= previous, "merge order broke");
                previous = *int;
                rows += 1;
            }
        }
        let merge_elapsed = merge_start.elapsed();

        assert_eq!(rows, TOTAL_ROWS);
        assert!(run_count > 1, "fixture must force a multi-run merge");
        println!(
            "task-180 evidence: {TOTAL_ROWS} rows, {run_count} runs, limit {LIMIT}B — \
             spill {spill_elapsed:?}, merge {merge_elapsed:?}"
        );
    }
}
