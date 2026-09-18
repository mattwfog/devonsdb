//! Snapshot-visible BM25 plumbing: fixed base statistics and strict scan memory.
use super::*;
// Per-thread diagnostic mutation observes index consultation without affecting scores.
thread_local! { static CACHED_ROWS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) }; }
pub(super) fn cached_rows() -> u64 {
    CACHED_ROWS.get()
}

use devondb_exec::fulltext::{Reservation, RowScorer, TextScan};
use devondb_storage::{
    fulltext::{FullTextError, FullTextQuery},
    overlay::{ChargedFullTextIndex, FullTextResult},
};

#[allow(clippy::too_many_arguments)]
pub(super) fn text_pipeline(
    view: &ReadView,
    root: &Operator,
    table: &str,
    column: &str,
    text: &str,
    k: u64,
    binding: &str,
) -> DevonResult<Pipeline> {
    let schema = view
        .catalog
        .node_table(table)
        .cloned()
        .ok_or_else(|| invalid_argument("TextScan table is missing"))?;
    let column = schema
        .columns()
        .iter()
        .position(|entry| fold(&entry.name) == fold(column))
        .ok_or_else(|| invalid_argument("TextScan String column is missing"))?;
    let (key, _) = primary_key(&schema)?;
    let projection = with_required_columns(
        referenced_columns(root, binding, &schema),
        &schema,
        [key, column],
    );
    let position = |index| {
        projection
            .as_ref()
            .map_or(Some(index), |set| position_in(set, index))
            .ok_or_else(|| corrupt("TextScan projection lost a required column"))
    };
    let key_position = position(key)?;
    let text_position = position(column)?;
    let PreparedQuery {
        query,
        index,
        bootstrap,
        query_charge,
    } = prepare_query(view, &schema, column, key, text)?;
    let source: Box<dyn ChunkSource> = if query.has_terms() {
        Box::new(StrictTextSource::new(
            view,
            schema.clone(),
            projection.clone(),
        )?)
    } else {
        Box::new(EmptySource)
    };
    let mut pipeline = Pipeline::scan(source, &schema, binding, projection.as_ref())?;
    if query.has_terms() {
        let types = pipeline.ordered_types()?;
        let offset_position = pipeline.width - 1;
        let scorer = scorer(
            view.clone(),
            schema.name().into(),
            key,
            key_position,
            text_position,
            offset_position,
            query,
            index,
            bootstrap,
            query_charge,
        );
        pipeline.source = Box::new(TextScan::new(
            pipeline.source,
            scorer,
            usize::try_from(k).map_err(|_| invalid_argument("TextScan k exceeds usize"))?,
            key_position,
            types,
            Arc::clone(&view.shared.budget),
        )?);
    }
    let name = scoreof_column_key(binding);
    pipeline.columns.insert(name.clone(), pipeline.width);
    pipeline.types.insert(name.clone(), LogicalType::Float64);
    pipeline
        .scores
        .insert(fold(binding).into_owned(), (name, pipeline.width));
    pipeline.width += 1;
    Ok(pipeline)
}

struct PreparedQuery {
    query: FullTextQuery,
    index: Option<Arc<ChargedFullTextIndex>>,
    bootstrap: bool,
    query_charge: Reservation,
}
fn prepare_query(
    view: &ReadView,
    schema: &NodeTableSchema,
    column: usize,
    key: usize,
    text: &str,
) -> DevonResult<PreparedQuery> {
    let mut query_charge = Reservation::new(
        Arc::clone(&view.shared.budget),
        FullTextQuery::memory_bound(text).map_err(fulltext_error)?,
    )?;
    let mut query = FullTextQuery::empty_corpus(text);
    query_charge.resize(query.heap_bytes())?;
    let mut index = None;
    if query.has_terms()
        && view.state.catalog.node_table(schema.name()).is_some()
        && let FullTextResult::Indexed(built) = PublishedState::fulltext_index(
            &view.state,
            &view.shared.pager,
            &view.shared.budget,
            schema.name(),
            &schema.columns()[column].name,
        )?
    {
        index = Some(built);
    }
    if let Some(index) = &index {
        query.use_index(index);
    } else if query.has_terms() {
        checkpoint_statistics(view, schema, column, &mut query)?;
    }
    let bootstrap = !query.has_corpus();
    if bootstrap && query.has_terms() {
        visible_statistics(view, schema, column, key, &mut query)?;
    }
    Ok(PreparedQuery {
        query,
        index,
        bootstrap,
        query_charge,
    })
}

#[allow(clippy::too_many_arguments)]
fn scorer(
    view: ReadView,
    table: String,
    key_index: usize,
    key_position: usize,
    text_position: usize,
    offset_position: usize,
    query: FullTextQuery,
    index: Option<Arc<ChargedFullTextIndex>>,
    bootstrap: bool,
    query_charge: Reservation,
) -> Box<RowScorer> {
    Box::new(move |chunk, row| {
        let _hold_query_charge = &query_charge;
        let text = match chunk
            .column(text_position)
            .and_then(|column| column.value_ref_boxed(row))
        {
            Some(Value::String(text)) => text.as_str(),
            Some(Value::Null) => return Ok(None),
            _ => return Err(corrupt("TextScan input String column is invalid")),
        };
        let key = chunk
            .value(row, key_position)
            .ok_or_else(|| corrupt("TextScan input key is missing"))?;
        let ordinal = match chunk.value(row, offset_position) {
            Some(Value::Int64(offset)) if offset >= 0 => offset as u64,
            _ => return Err(corrupt("TextScan input offset is invalid")),
        };
        if !bootstrap
            && let Some(index) = &index
            && index.document_length(ordinal).is_some()
            && unchanged_base_key(&view, &table, key_index, &key)
        {
            CACHED_ROWS.set(CACHED_ROWS.get().saturating_add(1));
            return Ok(query
                .matches_row(index, ordinal)
                .then(|| query.score_row(index, ordinal)));
        }
        if !query.matches_text(text).map_err(fulltext_error)? {
            return Ok(None);
        }
        query.score_text(text).map(Some).map_err(fulltext_error)
    })
}

fn unchanged_base_key(view: &ReadView, table: &str, key_index: usize, key: &Value) -> bool {
    let changed = |delta: &CommitDelta| {
        [delta.nodes.get(table), delta.node_updates.get(table)]
            .into_iter()
            .flatten()
            .any(|rows| rows.iter().any(|row| row.get(key_index) == Some(key)))
            || delta
                .node_deletes
                .get(table)
                .is_some_and(|keys| keys.contains(key))
    };
    if view.own.as_deref().is_some_and(changed) {
        return false;
    }
    let mut link = view.state.chain.as_deref();
    while let Some(current) = link {
        if changed(&current.delta) {
            return false;
        }
        link = current.prev.as_deref();
    }
    true
}

fn checkpoint_statistics(
    view: &ReadView,
    schema: &NodeTableSchema,
    column: usize,
    query: &mut FullTextQuery,
) -> DevonResult<()> {
    let types = schema
        .columns()
        .iter()
        .map(|entry| entry.ty)
        .collect::<Vec<_>>();
    let groups = view
        .state
        .catalog
        .table_storage(schema.name())
        .map_or(&[][..], |storage| storage.groups.as_slice());
    for page in groups {
        let directory = NodeGroup::read_directory(&view.shared.pager, *page, &types)?;
        let _decode = Reservation::new(
            Arc::clone(&view.shared.budget),
            directory.column_decode_peak_bytes(column, &types)?,
        )?;
        let (values, _) = NodeGroup::read_column_typed(&view.shared.pager, *page, &types, column)?;
        for row in 0..values.len() {
            if let Some(Value::String(text)) = values.value_ref_boxed(row) {
                query.observe_document(text).map_err(fulltext_error)?;
            }
        }
    }
    Ok(())
}

fn visible_statistics(
    view: &ReadView,
    schema: &NodeTableSchema,
    column: usize,
    key: usize,
    query: &mut FullTextQuery,
) -> DevonResult<()> {
    query.clear_corpus();
    let projection = with_required_columns(Some(BTreeSet::new()), schema, [key, column]);
    let position = projection
        .as_ref()
        .map_or(Some(column), |set| position_in(set, column))
        .ok_or_else(|| corrupt("TextScan statistics lost String column"))?;
    let mut source = StrictTextSource::new(view, schema.clone(), projection)?;
    while let Some(chunk) = source.next_chunk()? {
        for row in 0..chunk.row_count() {
            if let Some(Value::String(text)) = chunk
                .column(position)
                .and_then(|column| column.value_ref_boxed(row))
            {
                query.observe_document(text).map_err(fulltext_error)?;
            }
        }
    }
    Ok(())
}

struct StrictTextSource {
    source: ScanSource,
    _decode: Reservation,
}
impl StrictTextSource {
    fn new(
        view: &ReadView,
        schema: NodeTableSchema,
        projection: Option<BTreeSet<usize>>,
    ) -> DevonResult<Self> {
        let bytes = relationship_query::source_decode_peak(view, &schema, projection.as_ref())?;
        let decode = Reservation::new(Arc::clone(&view.shared.budget), bytes)?;
        let source = ScanSource::new(view, schema, None, projection)?;
        Ok(Self {
            source,
            _decode: decode,
        })
    }
}
impl ChunkSource for StrictTextSource {
    fn next_chunk(&mut self) -> DevonResult<Option<Chunk>> {
        let row = match self.source.next_persisted_row()? {
            Some(row) => row,
            None => match self.source.overlay_rows.next() {
                Some(row) => self.source.overlay_row(row)?,
                None => return Ok(None),
            },
        };
        // Tight vectors avoid legacy ChunkBuilder's 2048 reserved slots per
        // output column when a snapshot contains only a handful of rows.
        let columns = self
            .source
            .types
            .iter()
            .zip(row)
            .map(|(ty, value)| Column::from_values(ty, vec![value]))
            .collect();
        Chunk::from_columns(self.source.types.clone(), columns).map(Some)
    }
}
struct EmptySource;
impl ChunkSource for EmptySource {
    fn next_chunk(&mut self) -> DevonResult<Option<Chunk>> {
        Ok(None)
    }
}
fn fulltext_error(error: FullTextError) -> DevonError {
    invalid_argument(format!("TextScan: {error}"))
}
