//! `COPY` bulk-load execution under the bulk fence (`docs/SCALE.md` §5.3).
//!
//! Intercepted by `Database::execute` before transactional dispatch: COPY
//! bypasses the WAL, so it never runs inside a write transaction. The
//! fence: refuse while write transactions are open, hold the commit pipe
//! for the whole load, checkpoint first, build node groups or relationship
//! CSR on fresh pages, and publish atomically through `catalog.save`.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{BufRead, BufReader, Read},
    mem::size_of,
    sync::Arc,
};

#[cfg(feature = "parquet")]
#[path = "copy_parquet.rs"]
mod copy_parquet;

use devondb_plan::csv::CsvReader;
use devondb_storage::{
    budget::{ChargedBytes, MemoryBudget},
    bulk::{BulkNodeWriter, BulkRelWriter, geo_atom_sort_key},
    catalog::Catalog,
    node_group::NodeGroup,
    overlay::PublishedState,
    wal::WalWriter,
};
use devondb_types::{
    DevonError, DevonResult,
    logical_type::LogicalType,
    schema::{Column, NodeTableSchema, RelTableSchema, fold, suggestion_suffix},
    value::Value,
};

use super::{
    Database, Shared,
    checkpoint::checkpoint_locked,
    corrupt, invalid_argument, next_lsn,
    options::{CommitPipe, lock},
    truncate_wal,
};

const TREE_KEY_OVERHEAD_BYTES: usize = 64;

/// Executes `copy <table> from "<path>" [sort by <column>]`.
pub(super) fn execute_copy(
    database: &mut Database,
    table: &str,
    path: &str,
    sort_by: Option<&str>,
) -> DevonResult<()> {
    let shared = &database.shared;
    shared.require_writable("copy")?;
    let (mut pipe, _publication_guard) = shared.lock_commit_and_gate()?;
    require_quiescence(shared)?;
    {
        let current = shared.current_state();
        let target = require_copy_table(&current.catalog, table)?;
        validate_copy_target(&target, sort_by)?;
    }
    checkpoint_locked(shared, &mut pipe)?;

    let current = shared.current_state();
    let target = require_copy_table(&current.catalog, table)?;
    match target {
        CopyTable::Node(schema) => {
            execute_node_copy(shared, &mut pipe, &current, schema, table, path, sort_by)
        }
        CopyTable::Rel(schema) => execute_rel_copy(shared, &mut pipe, &current, schema, path),
    }
}

fn execute_node_copy(
    shared: &Arc<Shared>,
    pipe: &mut CommitPipe,
    current: &PublishedState,
    schema: NodeTableSchema,
    requested_table: &str,
    path: &str,
    sort_by: Option<&str>,
) -> DevonResult<()> {
    let source = CopySource::open(path)?;
    let sort = sort_by
        .map(|column| resolve_sort(&schema, column))
        .transpose()?;
    let mut keys = PrimaryKeys::new(&shared.budget)?;
    load_existing_keys(shared, &current.catalog, &schema, &mut keys)?;
    // The reader is scoped to its arm so loader-only charges (PK sets,
    // source buffers) release BEFORE index construction
    // (docs/INDEX_BULK_LOAD.md step 5); the groups are durable pages the
    // accessor re-reads.
    let storage = match source {
        CopySource::Csv => {
            let mut csv = CsvReader::open(path, &schema)?;
            load_rows(
                shared,
                &current.catalog,
                &schema,
                requested_table,
                &mut csv,
                sort,
                &mut keys,
            )?
        }
        #[cfg(feature = "parquet")]
        CopySource::Parquet => {
            let mut rows = copy_parquet::ParquetRows::open(path, &schema, &shared.budget)?;
            load_rows(
                shared,
                &current.catalog,
                &schema,
                requested_table,
                &mut rows,
                sort,
                &mut keys,
            )?
        }
    };

    let mut catalog = (*current.catalog).clone();
    catalog.set_table_storage(schema.name(), storage)?;
    drop(keys);
    super::hnsw::copy_rebuild_indexes(shared, current, &schema, &mut catalog)?;
    publish_copy(shared, pipe, current, catalog)
}

fn execute_rel_copy(
    shared: &Arc<Shared>,
    pipe: &mut CommitPipe,
    current: &PublishedState,
    schema: RelTableSchema,
    path: &str,
) -> DevonResult<()> {
    let row_schema = rel_csv_schema(&current.catalog, &schema)?;
    let source = CopySource::open(path)?;
    let endpoints = RelEndpoints::load(shared, &current.catalog, &schema)?;
    let mut writer = BulkRelWriter::new(schema, &shared.budget);
    match source {
        CopySource::Csv => {
            let mut csv = CsvReader::open(path, &row_schema)?;
            load_rel_csv_rows(path, &mut csv, &endpoints, &mut writer)?;
        }
        #[cfg(feature = "parquet")]
        CopySource::Parquet => {
            let mut rows = copy_parquet::ParquetRows::open(path, &row_schema, &shared.budget)?;
            load_rel_parquet_rows(&mut rows, &endpoints, &mut writer)?;
        }
    }

    let mut catalog = (*current.catalog).clone();
    writer.finish(&shared.pager, &mut catalog)?;
    publish_copy(shared, pipe, current, catalog)
}

/// The COPY source format, sniffed from the first four bytes of the file:
/// the magic, not the extension, decides — a `.csv` whose bytes start
/// with `PAR1` IS a Parquet file (`docs/SCALE.md` §5).
enum CopySource {
    Csv,
    #[cfg(feature = "parquet")]
    Parquet,
}

impl CopySource {
    fn open(path: &str) -> DevonResult<Self> {
        if !copy_source_is_parquet(path)? {
            return Ok(Self::Csv);
        }
        #[cfg(feature = "parquet")]
        return Ok(Self::Parquet);
        #[cfg(not(feature = "parquet"))]
        Err(invalid_argument(
            "Parquet COPY requires the `parquet` feature",
        ))
    }
}

const PARQUET_MAGIC: [u8; 4] = *b"PAR1";

fn copy_source_is_parquet(path: &str) -> DevonResult<bool> {
    let mut file = File::open(path)?;
    let mut magic = [0_u8; 4];
    let mut read = 0_usize;
    while read < magic.len() {
        let count = file.read(&mut magic[read..])?;
        if count == 0 {
            break;
        }
        read += count;
    }
    Ok(magic[..read] == PARQUET_MAGIC[..])
}

fn load_rel_csv_rows(
    path: &str,
    csv: &mut CsvReader,
    endpoints: &RelEndpoints<'_>,
    writer: &mut BulkRelWriter,
) -> DevonResult<()> {
    let mut record_lines = CsvRecordLines::open(path)?;
    for row in csv {
        let row = row?;
        let line = record_lines.next_data_line()?;
        let (from_key, to_key, values) = rel_csv_row(row)?;
        push_rel_row(endpoints, writer, from_key, to_key, values, |error| {
            annotate_csv_line(error, line)
        })?;
    }
    Ok(())
}

#[cfg(feature = "parquet")]
fn load_rel_parquet_rows(
    rows: &mut copy_parquet::ParquetRows<'_>,
    endpoints: &RelEndpoints<'_>,
    writer: &mut BulkRelWriter,
) -> DevonResult<()> {
    for (index, row) in rows.enumerate() {
        let row = row?;
        let row_number = index + 1;
        let (from_key, to_key, values) = rel_csv_row(row)?;
        push_rel_row(endpoints, writer, from_key, to_key, values, |error| {
            annotate_parquet_row(error, row_number)
        })?;
    }
    Ok(())
}

fn push_rel_row(
    endpoints: &RelEndpoints<'_>,
    writer: &mut BulkRelWriter,
    from_key: Value,
    to_key: Value,
    values: Vec<Value>,
    annotate: impl Fn(DevonError) -> DevonError,
) -> DevonResult<()> {
    let from = endpoints.resolve_from(&from_key).map_err(&annotate)?;
    let to = endpoints.resolve_to(&to_key).map_err(&annotate)?;
    writer.push_edge(from, to, values).map_err(annotate)
}

fn require_quiescence(shared: &Shared) -> DevonResult<()> {
    if !lock(&shared.write_txns).is_empty() {
        return Err(invalid_argument("copy requires no open write transactions"));
    }
    Ok(())
}

enum CopyTable {
    Node(NodeTableSchema),
    Rel(RelTableSchema),
}

fn require_copy_table(catalog: &Catalog, table: &str) -> DevonResult<CopyTable> {
    if let Some(schema) = catalog.node_table(table) {
        return Ok(CopyTable::Node(schema.clone()));
    }
    if let Some(schema) = catalog.rel_table(table) {
        return Ok(CopyTable::Rel(schema.clone()));
    }
    Err(DevonError::NotFound {
        what: format!(
            "node table `{table}`{}",
            suggestion_suffix(
                table,
                catalog
                    .node_tables()
                    .iter()
                    .map(|schema| schema.name())
                    .chain(catalog.rel_tables().iter().map(|schema| schema.name()))
            )
        ),
    })
}

fn validate_copy_target(target: &CopyTable, sort_by: Option<&str>) -> DevonResult<()> {
    match target {
        // Indexed node tables are legal COPY targets: replacement HNSW roots
        // build inside the fence and publish
        // atomically with the rows (docs/INDEX_BULK_LOAD.md).
        CopyTable::Node(_) => Ok(()),
        CopyTable::Rel(schema) if sort_by.is_some() => Err(invalid_argument(format!(
            "copy sort by is not supported for relationship table `{}`; edges load in CSR order",
            schema.name()
        ))),
        // HNSW catalog entries are node-table-typed and relationship schemas
        // cannot carry vector indexes, so there is no relationship analogue
        // of `refuse_indexed_table` to apply here.
        CopyTable::Rel(_) => Ok(()),
    }
}

fn rel_csv_schema(catalog: &Catalog, schema: &RelTableSchema) -> DevonResult<NodeTableSchema> {
    let from_schema = require_endpoint_schema(catalog, schema.from())?;
    let to_schema = require_endpoint_schema(catalog, schema.to())?;
    let (from_index, _) = primary_key(from_schema)?;
    let (to_index, _) = primary_key(to_schema)?;
    refuse_endpoint_header_collision(schema)?;
    let mut columns = Vec::with_capacity(schema.columns().len() + 2);
    columns.push(Column {
        name: "from".to_owned(),
        ty: from_schema.columns()[from_index].ty,
        primary_key: true,
    });
    columns.push(Column {
        name: "to".to_owned(),
        ty: to_schema.columns()[to_index].ty,
        primary_key: false,
    });
    columns.extend(schema.columns().iter().cloned());
    NodeTableSchema::new(format!("{} COPY row", schema.name()), columns)
}

fn require_endpoint_schema<'a>(
    catalog: &'a Catalog,
    table: &str,
) -> DevonResult<&'a NodeTableSchema> {
    catalog.node_table(table).ok_or_else(|| {
        corrupt(format!(
            "relationship endpoint node table `{table}` is missing"
        ))
    })
}

fn refuse_endpoint_header_collision(schema: &RelTableSchema) -> DevonResult<()> {
    if let Some(column) = schema
        .columns()
        .iter()
        .find(|column| matches!(fold(&column.name).as_ref(), "from" | "to"))
    {
        return Err(invalid_argument(format!(
            "relationship table `{}` property column `{}` conflicts with reserved COPY endpoint headers `from` and `to`",
            schema.name(),
            column.name
        )));
    }
    Ok(())
}

fn rel_csv_row(mut row: Vec<Value>) -> DevonResult<(Value, Value, Vec<Value>)> {
    if row.len() < 2 {
        return Err(corrupt(
            "validated relationship COPY row lost its endpoints",
        ));
    }
    let values = row.split_off(2);
    let to = row
        .pop()
        .ok_or_else(|| corrupt("validated relationship COPY row lost its to endpoint"))?;
    let from = row
        .pop()
        .ok_or_else(|| corrupt("validated relationship COPY row lost its from endpoint"))?;
    Ok((from, to, values))
}

fn annotate_csv_line(error: DevonError, line: usize) -> DevonError {
    match error {
        DevonError::InvalidArgument { context } => {
            invalid_argument(format!("{context} (CSV line {line})"))
        }
        DevonError::NotFound { what } => DevonError::NotFound {
            what: format!("{what} (CSV line {line})"),
        },
        other => other,
    }
}

#[cfg(feature = "parquet")]
fn annotate_parquet_row(error: DevonError, row: usize) -> DevonError {
    match error {
        DevonError::InvalidArgument { context } => {
            invalid_argument(format!("{context} (Parquet row {row})"))
        }
        DevonError::NotFound { what } => DevonError::NotFound {
            what: format!("{what} (Parquet row {row})"),
        },
        other => other,
    }
}

struct CsvRecordLines {
    reader: BufReader<File>,
    line: usize,
}

impl CsvRecordLines {
    fn open(path: &str) -> DevonResult<Self> {
        let mut lines = Self {
            reader: BufReader::new(File::open(path)?),
            line: 1,
        };
        lines
            .consume_record()?
            .ok_or_else(|| corrupt("validated relationship COPY file lost its header"))?;
        Ok(lines)
    }

    fn next_data_line(&mut self) -> DevonResult<usize> {
        self.consume_record()?
            .ok_or_else(|| corrupt("validated relationship COPY file lost a data row"))
    }

    fn consume_record(&mut self) -> DevonResult<Option<usize>> {
        let start = self.line;
        let mut state = CsvLineState::Start;
        let mut quoted_previous_cr = false;
        let mut saw_byte = false;
        loop {
            let Some(byte) = self.read_byte()? else {
                return Ok(saw_byte.then_some(start));
            };
            saw_byte = true;
            if self.consume_byte(byte, &mut state, &mut quoted_previous_cr) {
                return Ok(Some(start));
            }
        }
    }

    fn read_byte(&mut self) -> std::io::Result<Option<u8>> {
        let byte = self.reader.fill_buf()?.first().copied();
        if byte.is_some() {
            self.reader.consume(1);
        }
        Ok(byte)
    }

    fn consume_byte(
        &mut self,
        byte: u8,
        state: &mut CsvLineState,
        quoted_previous_cr: &mut bool,
    ) -> bool {
        match (*state, byte) {
            (CsvLineState::Start, b'"') => *state = CsvLineState::Quoted,
            (CsvLineState::Start | CsvLineState::Unquoted, b'\n')
            | (CsvLineState::AfterQuote | CsvLineState::RecordCr, b'\n') => {
                self.line += 1;
                return true;
            }
            (CsvLineState::Start | CsvLineState::Unquoted, b'\r')
            | (CsvLineState::AfterQuote, b'\r') => *state = CsvLineState::RecordCr,
            (CsvLineState::Start | CsvLineState::Unquoted | CsvLineState::AfterQuote, b',') => {
                *state = CsvLineState::Start;
            }
            (CsvLineState::Start, _) => *state = CsvLineState::Unquoted,
            (CsvLineState::Quoted, b'"') => {
                *quoted_previous_cr = false;
                *state = CsvLineState::AfterQuote;
            }
            (CsvLineState::Quoted, b'\r') => {
                self.line += 1;
                *quoted_previous_cr = true;
            }
            (CsvLineState::Quoted, b'\n') => {
                if !*quoted_previous_cr {
                    self.line += 1;
                }
                *quoted_previous_cr = false;
            }
            (CsvLineState::Quoted, _) => *quoted_previous_cr = false,
            (CsvLineState::AfterQuote, b'"') => *state = CsvLineState::Quoted,
            _ => {}
        }
        false
    }
}

#[derive(Clone, Copy)]
enum CsvLineState {
    Start,
    Unquoted,
    Quoted,
    AfterQuote,
    RecordCr,
}

struct RelEndpoints<'budget> {
    from_schema: NodeTableSchema,
    to_schema: NodeTableSchema,
    from: EndpointKeys<'budget>,
    to: Option<EndpointKeys<'budget>>,
}

impl<'budget> RelEndpoints<'budget> {
    fn load(
        shared: &'budget Shared,
        catalog: &Catalog,
        schema: &RelTableSchema,
    ) -> DevonResult<Self> {
        let from_schema = require_endpoint_schema(catalog, schema.from())?.clone();
        let to_schema = require_endpoint_schema(catalog, schema.to())?.clone();
        let from = EndpointKeys::load(shared, catalog, &from_schema)?;
        let to = if fold(from_schema.name()) == fold(to_schema.name()) {
            None
        } else {
            Some(EndpointKeys::load(shared, catalog, &to_schema)?)
        };
        Ok(Self {
            from_schema,
            to_schema,
            from,
            to,
        })
    }

    fn resolve_from(&self, key: &Value) -> DevonResult<u64> {
        self.from.resolve(&self.from_schema, key)
    }

    fn resolve_to(&self, key: &Value) -> DevonResult<u64> {
        self.to
            .as_ref()
            .unwrap_or(&self.from)
            .resolve(&self.to_schema, key)
    }
}

struct EndpointKeys<'budget> {
    values: BTreeMap<PrimaryKey, u64>,
    charge: CopyCharge<'budget>,
}

impl<'budget> EndpointKeys<'budget> {
    fn load(
        shared: &'budget Shared,
        catalog: &Catalog,
        schema: &NodeTableSchema,
    ) -> DevonResult<Self> {
        let mut keys = Self {
            values: BTreeMap::new(),
            charge: CopyCharge::new(&shared.budget),
        };
        load_endpoint_groups(shared, catalog, schema, &mut keys)?;
        Ok(keys)
    }

    fn insert(&mut self, table: &str, value: &Value, offset: u64) -> DevonResult<()> {
        let key = PrimaryKey::from_value(value)?;
        if self.values.contains_key(&key) {
            return Err(corrupt(format!(
                "node table `{table}` contains duplicate persisted primary key `{value}`"
            )));
        }
        let bytes = key.charged_bytes()?;
        self.charge.grow(bytes, || {
            format!("COPY endpoint-key map requested {bytes} additional bytes")
        })?;
        self.values.insert(key, offset);
        Ok(())
    }

    fn resolve(&self, schema: &NodeTableSchema, value: &Value) -> DevonResult<u64> {
        let (key_index, key_column) = primary_key(schema)?;
        let key = PrimaryKey::from_query(schema, key_index, key_column, value)?;
        self.values
            .get(&key)
            .copied()
            .ok_or_else(|| DevonError::NotFound {
                what: format!("node table `{}` primary key {value}", schema.name()),
            })
    }
}

fn load_endpoint_groups(
    shared: &Shared,
    catalog: &Catalog,
    schema: &NodeTableSchema,
    keys: &mut EndpointKeys<'_>,
) -> DevonResult<()> {
    let (key_index, _) = primary_key(schema)?;
    let types = schema_types(schema);
    let groups = catalog
        .table_storage(schema.name())
        .map_or(&[][..], |storage| storage.groups.as_slice());
    let mut offset = 0_u64;
    for group_id in groups {
        let group = NodeGroup::read(&shared.pager, *group_id, &types)?;
        offset = load_endpoint_group(schema.name(), &group, key_index, offset, keys)?;
    }
    Ok(())
}

fn load_endpoint_group(
    table: &str,
    group: &NodeGroup,
    key_index: usize,
    mut offset: u64,
    keys: &mut EndpointKeys<'_>,
) -> DevonResult<u64> {
    for row in 0..group.row_count() {
        let value = group.value(row, key_index).ok_or_else(|| {
            corrupt(format!(
                "node table `{table}` group lost primary key row {row}"
            ))
        })?;
        if value == &Value::Null {
            return Err(corrupt(format!(
                "node table `{table}` contains a null persisted primary key"
            )));
        }
        keys.insert(table, value, offset)?;
        offset = offset
            .checked_add(1)
            .ok_or_else(|| corrupt(format!("node table `{table}` offset exceeds u64::MAX")))?;
    }
    Ok(offset)
}

fn schema_types(schema: &NodeTableSchema) -> Vec<LogicalType> {
    schema.columns().iter().map(|column| column.ty).collect()
}

#[derive(Clone, Copy)]
struct SortColumn {
    index: usize,
    ty: LogicalType,
}

fn resolve_sort(schema: &NodeTableSchema, column: &str) -> DevonResult<SortColumn> {
    let index = schema
        .column_index(column)
        .ok_or_else(|| DevonError::NotFound {
            what: format!(
                "column `{}.{column}`{}",
                schema.name(),
                suggestion_suffix(
                    column,
                    schema.columns().iter().map(|item| item.name.as_str())
                )
            ),
        })?;
    let ty = schema.columns()[index].ty;
    if !matches!(
        ty,
        LogicalType::Int64 | LogicalType::Float64 | LogicalType::GeoPoint
    ) {
        return Err(invalid_argument(format!(
            "copy sort column `{}.{column}` has unsortable type {ty}; expected Int64, Float64, or GeoPoint",
            schema.name()
        )));
    }
    Ok(SortColumn { index, ty })
}

fn load_rows<R: Iterator<Item = DevonResult<Vec<Value>>>>(
    shared: &Shared,
    catalog: &Catalog,
    schema: &NodeTableSchema,
    requested_table: &str,
    rows: &mut R,
    sort: Option<SortColumn>,
    keys: &mut PrimaryKeys<'_>,
) -> DevonResult<devondb_storage::catalog::TableStorage> {
    let writer = BulkNodeWriter::new(
        schema.clone(),
        catalog.table_storage(schema.name()),
        &shared.pager,
    )?;
    match sort {
        Some(sort) => load_sorted(shared, writer, rows, schema, requested_table, sort, keys),
        None => load_streaming(shared, writer, rows, schema, requested_table, keys),
    }
}

fn load_streaming<R: Iterator<Item = DevonResult<Vec<Value>>>>(
    shared: &Shared,
    mut writer: BulkNodeWriter,
    rows: &mut R,
    schema: &NodeTableSchema,
    table: &str,
    keys: &mut PrimaryKeys<'_>,
) -> DevonResult<devondb_storage::catalog::TableStorage> {
    let (key_index, key_column) = primary_key(schema)?;
    for row in rows {
        let row = row?;
        validate_file_key(&row, key_index, key_column, table, keys)?;
        writer.push_row(&shared.pager, row)?;
    }
    writer.finish(&shared.pager)
}

fn load_sorted<R: Iterator<Item = DevonResult<Vec<Value>>>>(
    shared: &Shared,
    mut writer: BulkNodeWriter,
    rows: &mut R,
    schema: &NodeTableSchema,
    table: &str,
    sort: SortColumn,
    keys: &mut PrimaryKeys<'_>,
) -> DevonResult<devondb_storage::catalog::TableStorage> {
    let (key_index, key_column) = primary_key(schema)?;
    let mut buffered = BufferedRows::new(&shared.budget)?;
    for row in rows {
        let row = row?;
        validate_file_key(&row, key_index, key_column, table, keys)?;
        buffered.push(row, sort)?;
    }
    buffered.sort();
    for row in buffered.drain_rows() {
        writer.push_row(&shared.pager, row)?;
    }
    writer.finish(&shared.pager)
}

fn primary_key(schema: &NodeTableSchema) -> DevonResult<(usize, &str)> {
    schema
        .columns()
        .iter()
        .enumerate()
        .find(|(_, column)| column.primary_key)
        .map(|(index, column)| (index, column.name.as_str()))
        .ok_or_else(|| corrupt(format!("node table `{}` has no primary key", schema.name())))
}

fn validate_file_key(
    row: &[Value],
    key_index: usize,
    key_column: &str,
    table: &str,
    keys: &mut PrimaryKeys<'_>,
) -> DevonResult<()> {
    let value = row
        .get(key_index)
        .ok_or_else(|| corrupt("validated COPY row lost its primary-key value"))?;
    if value == &Value::Null {
        return Err(invalid_argument(format!(
            "primary key column `{key_column}` in node table `{table}` cannot be null"
        )));
    }
    if !keys.insert(value)? {
        return Err(invalid_argument(format!(
            "duplicate primary key `{value}` in node table `{table}`"
        )));
    }
    Ok(())
}

fn load_existing_keys(
    shared: &Shared,
    catalog: &Catalog,
    schema: &NodeTableSchema,
    keys: &mut PrimaryKeys<'_>,
) -> DevonResult<()> {
    let (key_index, _) = primary_key(schema)?;
    let types = schema
        .columns()
        .iter()
        .map(|column| column.ty)
        .collect::<Vec<_>>();
    let groups = catalog
        .table_storage(schema.name())
        .map_or(&[][..], |storage| storage.groups.as_slice());
    for group_id in groups {
        let group = NodeGroup::read(&shared.pager, *group_id, &types)?;
        load_group_keys(schema.name(), &group, key_index, keys)?;
    }
    Ok(())
}

fn load_group_keys(
    table: &str,
    group: &NodeGroup,
    key_index: usize,
    keys: &mut PrimaryKeys<'_>,
) -> DevonResult<()> {
    for row in 0..group.row_count() {
        let value = group.value(row, key_index).ok_or_else(|| {
            corrupt(format!(
                "node table `{table}` group lost primary key row {row}"
            ))
        })?;
        if value == &Value::Null {
            return Err(corrupt(format!(
                "node table `{table}` contains a null persisted primary key"
            )));
        }
        if !keys.insert(value)? {
            return Err(corrupt(format!(
                "node table `{table}` contains duplicate persisted primary key `{value}`"
            )));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Eq, Ord, PartialEq, PartialOrd)]
enum PrimaryKey {
    Int64(i64),
    String(String),
}

impl PrimaryKey {
    fn from_value(value: &Value) -> DevonResult<Self> {
        match value {
            Value::Int64(value) => Ok(Self::Int64(*value)),
            Value::String(value) => Ok(Self::String(value.clone())),
            other => Err(corrupt(format!(
                "node primary key has unexpected value `{other}`"
            ))),
        }
    }

    fn charged_bytes(&self) -> DevonResult<usize> {
        let value_bytes = match self {
            Self::Int64(_) => Some(16),
            Self::String(value) => 32_usize.checked_add(value.len()),
        };
        value_bytes
            .and_then(|bytes| bytes.checked_add(TREE_KEY_OVERHEAD_BYTES))
            .ok_or_else(|| invalid_argument("COPY primary-key charge overflows usize"))
    }

    fn from_query(
        schema: &NodeTableSchema,
        key_index: usize,
        key_column: &str,
        value: &Value,
    ) -> DevonResult<Self> {
        match (schema.columns()[key_index].ty, value) {
            (LogicalType::Int64, Value::Int64(value)) => Ok(Self::Int64(*value)),
            (LogicalType::String, Value::String(value)) => Ok(Self::String(value.clone())),
            (expected, _) => Err(invalid_argument(format!(
                "primary key `{key_column}` in node table `{}` expects {expected} but received {value}",
                schema.name()
            ))),
        }
    }
}

struct PrimaryKeys<'a> {
    values: BTreeSet<PrimaryKey>,
    charge: ChargedBytes<'a>,
}

impl<'a> PrimaryKeys<'a> {
    fn new(budget: &'a MemoryBudget) -> DevonResult<Self> {
        Ok(Self {
            values: BTreeSet::new(),
            charge: ChargedBytes::try_new(budget, 0, || "COPY primary-key set".to_owned())?,
        })
    }

    fn insert(&mut self, value: &Value) -> DevonResult<bool> {
        let key = PrimaryKey::from_value(value)?;
        if self.values.contains(&key) {
            return Ok(false);
        }
        let bytes = key.charged_bytes()?;
        self.charge.grow(bytes, || {
            format!("COPY primary-key set requested {bytes} additional bytes")
        })?;
        if self.values.insert(key) {
            Ok(true)
        } else {
            self.charge.shrink(bytes);
            Ok(false)
        }
    }
}

enum SortValue {
    Null,
    Int64(i64),
    Float64(f64),
    GeoPoint(u64),
}

impl SortValue {
    fn from_row(row: &[Value], sort: SortColumn) -> DevonResult<Self> {
        let value = row
            .get(sort.index)
            .ok_or_else(|| corrupt("validated COPY row lost its sort value"))?;
        match (value, sort.ty) {
            (Value::Null, _) => Ok(Self::Null),
            (Value::Int64(value), LogicalType::Int64) => Ok(Self::Int64(*value)),
            (Value::Float64(value), LogicalType::Float64) => Ok(Self::Float64(*value)),
            (Value::GeoPoint(value), LogicalType::GeoPoint) => {
                geo_atom_sort_key(*value).map(Self::GeoPoint)
            }
            _ => Err(corrupt(format!(
                "COPY sort value `{value}` disagrees with column type {}",
                sort.ty
            ))),
        }
    }

    fn compare(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Null, Self::Null) => Ordering::Equal,
            (Self::Null, _) => Ordering::Greater,
            (_, Self::Null) => Ordering::Less,
            (Self::Int64(left), Self::Int64(right)) => left.cmp(right),
            (Self::Float64(left), Self::Float64(right)) => left.total_cmp(right),
            (Self::GeoPoint(left), Self::GeoPoint(right)) => left.cmp(right),
            _ => Ordering::Equal,
        }
    }
}

struct BufferedRow {
    row: Vec<Value>,
    sort: SortValue,
}

struct BufferedRows<'a> {
    rows: Vec<BufferedRow>,
    charge: ChargedBytes<'a>,
}

impl<'a> BufferedRows<'a> {
    fn new(budget: &'a MemoryBudget) -> DevonResult<Self> {
        Ok(Self {
            rows: Vec::new(),
            charge: ChargedBytes::try_new(budget, 0, || "COPY sort buffer".to_owned())?,
        })
    }

    fn push(&mut self, row: Vec<Value>, sort: SortColumn) -> DevonResult<()> {
        let bytes = buffered_row_bytes(&row)?;
        let sort = SortValue::from_row(&row, sort)?;
        self.charge.grow(bytes, || {
            format!("COPY sort buffer requested {bytes} additional bytes")
        })?;
        self.rows.push(BufferedRow { row, sort });
        Ok(())
    }

    fn sort(&mut self) {
        // Stable: equal sort keys keep original CSV record ordinal
        // (`docs/SCALE.md` §5.4 tiebreak law) — HNSW insertion
        // order must never depend on a sort implementation detail.
        self.rows
            .sort_by(|left, right| left.sort.compare(&right.sort));
    }

    fn drain_rows(&mut self) -> impl Iterator<Item = Vec<Value>> + '_ {
        self.rows.drain(..).map(|buffered| buffered.row)
    }
}

struct CopyCharge<'budget> {
    budget: &'budget MemoryBudget,
    bytes: usize,
}

impl<'budget> CopyCharge<'budget> {
    const fn new(budget: &'budget MemoryBudget) -> Self {
        Self { budget, bytes: 0 }
    }

    fn grow(&mut self, additional: usize, context: impl FnOnce() -> String) -> DevonResult<()> {
        let Some(next) = self.bytes.checked_add(additional) else {
            return Err(DevonError::BudgetExceeded { context: context() });
        };
        if !self.budget.charge_or_reclaim(additional) {
            return Err(DevonError::BudgetExceeded { context: context() });
        }
        self.bytes = next;
        Ok(())
    }
}

impl Drop for CopyCharge<'_> {
    fn drop(&mut self) {
        self.budget.release(self.bytes);
    }
}

fn buffered_row_bytes(row: &[Value]) -> DevonResult<usize> {
    row.iter()
        .try_fold(size_of::<BufferedRow>(), |total, value| {
            total
                .checked_add(value.approx_bytes())
                .ok_or_else(|| invalid_argument("COPY sort-buffer charge overflows usize"))
        })
}

fn publish_copy(
    shared: &Arc<Shared>,
    pipe: &mut CommitPipe,
    current: &PublishedState,
    catalog: Catalog,
) -> DevonResult<()> {
    let publish_lsn = next_lsn(&shared.pager)?;
    catalog.save(&shared.pager, publish_lsn)?;
    pipe.wal = None;
    truncate_wal(&pipe.wal_path)?;
    pipe.wal = Some(WalWriter::open(&pipe.wal_path, next_lsn(&shared.pager)?)?);
    shared.publish(Arc::new(PublishedState {
        catalog: Arc::new(catalog),
        chain: None,
        last_commit_lsn: publish_lsn,
        catalog_generation: publish_lsn,
        recent_summaries: current.recent_summaries.clone(),
    }));
    Ok(())
}
