//! Approximate KNN execution over an HNSW snapshot view.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, VecDeque},
    mem::size_of,
};

use devondb_plan::expr::Metric;
use devondb_storage::{
    budget::{ChargedBytes, MemoryBudget},
    hnsw::{
        search::{SearchOptions, search},
        types::{GraphAccess, HnswConfig, HnswMetric, NavigationEncoding},
    },
};
use devondb_types::{DevonError, DevonResult, logical_type::LogicalType, value::Value};

use crate::{
    chunk::{Chunk, ChunkBuilder},
    knn::KnnScan as ExactKnnScan,
    simd,
    source::ChunkSource,
};

const DEFAULT_EF_SEARCH: usize = 128;
/// Query-policy oversample factor for b1 navigation. `docs/HNSW.md` §6.2
/// sets 3k as the FLOOR and permits tuning `ef_search` upward; 1-bit
/// estimates over the corpus's dim-64 vectors rank too coarsely at 3k
/// (measured recall@100 = 0.87 against the §9.2 gate of 0.90), and the
/// oversample-then-rescore remedy is the RaBitQ-family standard. 6k
/// passes every §9.2 cell; the arena stays k-proportional and charged.
const B1_EF_SEARCH_FACTOR: usize = 6;
const ALLOCATION_OVERHEAD: usize = 64;

/// Navigation-distance callback used by the storage HNSW search.
pub type DistanceAccessor<'a> = dyn FnMut(u64) -> DevonResult<f32> + 'a;

/// Candidate-vector callback used to rescore b1 search results.
pub type RescoreAccessor<'a> = dyn FnMut(u64) -> DevonResult<Vec<f32>> + 'a;

/// Factory for a fresh full-table source on the pinned snapshot.
pub type FullScanFactory<'a> = dyn FnMut() -> Box<dyn ChunkSource> + 'a;

/// Blocking approximate KNN source with exact-tail union and exact fallback.
pub struct KnnScan<'a, G: GraphAccess> {
    graph: Option<G>,
    config: HnswConfig,
    distance: Option<Box<DistanceAccessor<'a>>>,
    rescore: Option<Box<RescoreAccessor<'a>>>,
    tail: Option<Box<dyn ChunkSource>>,
    full_scan: Box<FullScanFactory<'a>>,
    vector_column: usize,
    query: Vec<f32>,
    k: u64,
    metric: Metric,
    budget: &'a MemoryBudget,
    initialized: bool,
    output: VecDeque<Chunk>,
    output_charge: Option<ChargedBytes<'a>>,
}

impl<'a, G: GraphAccess> KnnScan<'a, G> {
    /// Creates an approximate KNN scan over one immutable HNSW snapshot view.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        graph: G,
        config: HnswConfig,
        distance: Box<DistanceAccessor<'a>>,
        rescore: Option<Box<RescoreAccessor<'a>>>,
        tail: Box<dyn ChunkSource>,
        full_scan: Box<FullScanFactory<'a>>,
        vector_column: usize,
        query: Vec<f32>,
        k: u64,
        metric: Metric,
        budget: &'a MemoryBudget,
    ) -> Self {
        Self {
            graph: Some(graph),
            config,
            distance: Some(distance),
            rescore,
            tail: Some(tail),
            full_scan,
            vector_column,
            query,
            k,
            metric,
            budget,
            initialized: false,
            output: VecDeque::new(),
            output_charge: None,
        }
    }

    fn initialize(&mut self) -> DevonResult<()> {
        self.config.validate()?;
        if self.k == 0 {
            self.drop_ann_state();
            return Ok(());
        }
        if !self.ann_supported() {
            self.drop_ann_state();
            let output = self.run_full_brute()?;
            self.install_output(output);
            return Ok(());
        }
        let graph = self
            .graph
            .take()
            .ok_or_else(|| corrupt("HNSW graph view was already consumed"))?;
        let mut distance = self
            .distance
            .take()
            .ok_or_else(|| corrupt("HNSW distance accessor was already consumed"))?;
        let mut rescore = self.rescore.take();
        let result = self.try_ann(&graph, distance.as_mut(), &mut rescore);
        drop((graph, distance, rescore));
        self.tail = None;
        match result {
            Ok(output) => self.install_output(output),
            Err(DevonError::BudgetExceeded { .. }) => {
                let output = self.run_full_brute()?;
                self.install_output(output);
            }
            Err(error) => return Err(error),
        }
        Ok(())
    }

    fn try_ann(
        &mut self,
        graph: &G,
        distance: &mut DistanceAccessor<'_>,
        rescore: &mut Option<Box<RescoreAccessor<'a>>>,
    ) -> DevonResult<ChargedOutput<'a>> {
        let k = usize::try_from(self.k)
            .map_err(|_| invalid_argument("HNSW KNN k does not fit usize"))?;
        let mut charge =
            reserve_ann_working_set(self.budget, k, self.query.len(), self.config.navigation)?;
        let results = search(
            graph,
            &self.config,
            SearchOptions {
                target_layer: 0,
                k,
                ef_search: effective_ef_search(k, self.config.navigation)?,
            },
            self.budget,
            distance,
        )?;
        let mut candidates = self.indexed_candidates(results.nodes(), rescore)?;
        drop(results);
        candidates.sort_unstable_by(compare_candidates);
        candidates.truncate(k);
        let mut schema = None;
        self.materialize_indexed_rows(&mut candidates, &mut schema)?;
        rescore_materialized(
            &mut candidates,
            self.vector_column,
            &self.query,
            self.metric,
        )?;
        let tail = self
            .tail
            .take()
            .ok_or_else(|| corrupt("HNSW exact-tail source was already consumed"))?;
        let (tail_candidates, tail_schema) = collect_tail_candidates(
            tail,
            graph.covered_rows(),
            self.vector_column,
            &self.query,
            self.k,
            self.metric,
        )?;
        merge_schema(&mut schema, tail_schema)?;
        candidates.extend(tail_candidates);
        let selected = union_and_select(candidates, k);
        let output = build_output(selected, schema)?;
        let output_bytes = output_charge_bytes(&output)?;
        charge.grow(output_bytes, || "HNSW final result rows".to_owned())?;
        Ok(ChargedOutput {
            chunks: output,
            charge,
        })
    }

    fn indexed_candidates(
        &mut self,
        nodes: &[devondb_storage::hnsw::search::ScoredNode],
        rescore: &mut Option<Box<RescoreAccessor<'a>>>,
    ) -> DevonResult<Vec<Candidate>> {
        nodes
            .iter()
            .map(|node| {
                let distance = if self.config.navigation == NavigationEncoding::B1 {
                    Self::rescore_candidate(rescore, node.node_offset, &self.query, self.metric)?
                } else {
                    node.distance
                };
                Ok(Candidate {
                    distance,
                    node_offset: node.node_offset,
                    values: None,
                })
            })
            .collect()
    }

    fn rescore_candidate(
        rescore: &mut Option<Box<RescoreAccessor<'a>>>,
        node_offset: u64,
        query: &[f32],
        metric: Metric,
    ) -> DevonResult<f32> {
        let accessor = rescore
            .as_mut()
            .ok_or_else(|| invalid_argument("b1 HNSW search requires a rescore vector accessor"))?;
        let vector = accessor(node_offset)?;
        vector_distance(&vector, query, metric, node_offset)
    }

    fn materialize_indexed_rows(
        &mut self,
        selected: &mut [Candidate],
        schema: &mut Option<Vec<LogicalType>>,
    ) -> DevonResult<()> {
        let mut missing = selected
            .iter()
            .enumerate()
            .filter(|(_, candidate)| candidate.values.is_none())
            .map(|(index, candidate)| (candidate.node_offset, index))
            .collect::<BTreeMap<_, _>>();
        if missing.is_empty() {
            return Ok(());
        }
        let mut source = (self.full_scan)();
        let mut node_offset = 0_u64;
        while let Some(chunk) = source.next_chunk()? {
            validate_schema(&chunk, schema, "HNSW full-scan")?;
            materialize_chunk(&chunk, selected, &mut missing, &mut node_offset)?;
            if missing.is_empty() {
                return Ok(());
            }
        }
        Err(corrupt(format!(
            "HNSW candidates are missing from the snapshot row source: {:?}",
            missing.keys().collect::<Vec<_>>()
        )))
    }

    fn run_full_brute(&mut self) -> DevonResult<ChargedOutput<'a>> {
        let source = (self.full_scan)();
        let mut brute = ExactKnnScan::new(
            source,
            self.vector_column,
            self.query.clone(),
            self.k,
            self.metric,
        );
        let chunks = collect_chunks(&mut brute)?;
        let bytes = output_charge_bytes(&chunks)?;
        let charge = ChargedBytes::try_new(self.budget, bytes, || {
            "brute-force KNN final result rows".to_owned()
        })?;
        Ok(ChargedOutput { chunks, charge })
    }

    fn ann_supported(&self) -> bool {
        usize::try_from(self.k).is_ok()
            && metric_matches(self.config.metric, self.metric)
            && (self.config.navigation != NavigationEncoding::B1
                || (self.metric == Metric::Cosine && self.rescore.is_some()))
    }

    fn drop_ann_state(&mut self) {
        self.graph = None;
        self.distance = None;
        self.rescore = None;
        self.tail = None;
    }

    fn install_output(&mut self, output: ChargedOutput<'a>) {
        self.output = output.chunks;
        self.output_charge = Some(output.charge);
    }
}

impl<G: GraphAccess> ChunkSource for KnnScan<'_, G> {
    fn next_chunk(&mut self) -> DevonResult<Option<Chunk>> {
        if !self.initialized {
            self.initialize()?;
            self.initialized = true;
        }
        let chunk = self.output.pop_front();
        if self.output.is_empty() {
            self.output_charge = None;
        }
        Ok(chunk)
    }
}

struct ChargedOutput<'a> {
    chunks: VecDeque<Chunk>,
    charge: ChargedBytes<'a>,
}

struct Candidate {
    distance: f32,
    node_offset: u64,
    values: Option<Vec<Value>>,
}

struct OffsetSource {
    source: Box<dyn ChunkSource>,
    next_offset: u64,
}

impl OffsetSource {
    fn new(source: Box<dyn ChunkSource>, next_offset: u64) -> Self {
        Self {
            source,
            next_offset,
        }
    }

    fn append_offsets(&mut self, chunk: &Chunk) -> DevonResult<Chunk> {
        let mut types = chunk.types().to_vec();
        types.push(LogicalType::String);
        let mut builder = ChunkBuilder::new(types);
        for row in 0..chunk.row_count() {
            let mut values = clone_row(chunk, row)?;
            values.push(Value::String(self.next_offset.to_string()));
            builder.push_row(values)?;
            self.next_offset = self
                .next_offset
                .checked_add(1)
                .ok_or_else(|| invalid_argument("HNSW tail node offset exceeds u64::MAX"))?;
        }
        Ok(builder.finish())
    }
}

impl ChunkSource for OffsetSource {
    fn next_chunk(&mut self) -> DevonResult<Option<Chunk>> {
        let Some(chunk) = self.source.next_chunk()? else {
            return Ok(None);
        };
        self.append_offsets(&chunk).map(Some)
    }
}

fn collect_tail_candidates(
    tail: Box<dyn ChunkSource>,
    first_offset: u64,
    vector_column: usize,
    query: &[f32],
    k: u64,
    metric: Metric,
) -> DevonResult<(Vec<Candidate>, Option<Vec<LogicalType>>)> {
    let source = OffsetSource::new(tail, first_offset);
    let mut brute = ExactKnnScan::new(Box::new(source), vector_column, query.to_vec(), k, metric);
    let chunks = collect_chunks(&mut brute)?;
    parse_tail_output(chunks)
}

fn collect_chunks(source: &mut dyn ChunkSource) -> DevonResult<VecDeque<Chunk>> {
    let mut chunks = VecDeque::new();
    while let Some(chunk) = source.next_chunk()? {
        chunks.push_back(chunk);
    }
    Ok(chunks)
}

fn parse_tail_output(
    chunks: VecDeque<Chunk>,
) -> DevonResult<(Vec<Candidate>, Option<Vec<LogicalType>>)> {
    let mut candidates = Vec::new();
    let mut schema = None;
    for chunk in chunks {
        let base_columns = validate_tail_output(&chunk, &mut schema)?;
        for row in 0..chunk.row_count() {
            candidates.push(parse_tail_candidate(&chunk, row, base_columns)?);
        }
    }
    Ok((candidates, schema))
}

fn validate_tail_output(
    chunk: &Chunk,
    schema: &mut Option<Vec<LogicalType>>,
) -> DevonResult<usize> {
    let base_columns = chunk
        .column_count()
        .checked_sub(2)
        .ok_or_else(|| corrupt("HNSW tail KNN output is missing offset and distance columns"))?;
    if chunk.types().get(base_columns) != Some(&LogicalType::String)
        || chunk.types().last() != Some(&LogicalType::Float64)
    {
        return Err(corrupt(
            "HNSW tail KNN output has invalid offset or distance column types",
        ));
    }
    set_or_validate_types(&chunk.types()[..base_columns], schema, "HNSW exact-tail")?;
    Ok(base_columns)
}

fn parse_tail_candidate(chunk: &Chunk, row: usize, base_columns: usize) -> DevonResult<Candidate> {
    let node_offset = match chunk.value(row, base_columns) {
        Some(Value::String(value)) => value
            .parse::<u64>()
            .map_err(|_| corrupt("HNSW tail KNN produced an invalid node offset"))?,
        _ => return Err(corrupt("HNSW tail KNN did not preserve its node offset")),
    };
    let distance = match chunk.value(row, base_columns + 1) {
        Some(Value::Float64(value)) => value as f32,
        _ => return Err(corrupt("HNSW tail KNN did not produce a distance")),
    };
    let values = (0..base_columns)
        .map(|column| clone_value(chunk, row, column))
        .collect::<DevonResult<Vec<_>>>()?;
    Ok(Candidate {
        distance,
        node_offset,
        values: Some(values),
    })
}

fn union_and_select(candidates: Vec<Candidate>, k: usize) -> Vec<Candidate> {
    let mut union = BTreeMap::<u64, Candidate>::new();
    for candidate in candidates {
        match union.get_mut(&candidate.node_offset) {
            Some(current) if should_replace(current, &candidate) => *current = candidate,
            Some(_) => {}
            None => {
                union.insert(candidate.node_offset, candidate);
            }
        }
    }
    let mut selected = union.into_values().collect::<Vec<_>>();
    selected.sort_unstable_by(compare_candidates);
    selected.truncate(k);
    selected
}

fn should_replace(current: &Candidate, candidate: &Candidate) -> bool {
    match candidate.distance.total_cmp(&current.distance) {
        Ordering::Less => true,
        Ordering::Equal => current.values.is_none() && candidate.values.is_some(),
        Ordering::Greater => false,
    }
}

fn compare_candidates(left: &Candidate, right: &Candidate) -> Ordering {
    left.distance
        .total_cmp(&right.distance)
        .then_with(|| left.node_offset.cmp(&right.node_offset))
}

fn materialize_chunk(
    chunk: &Chunk,
    selected: &mut [Candidate],
    missing: &mut BTreeMap<u64, usize>,
    node_offset: &mut u64,
) -> DevonResult<()> {
    for row in 0..chunk.row_count() {
        if let Some(index) = missing.remove(node_offset) {
            selected[index].values = Some(clone_row(chunk, row)?);
        }
        *node_offset = node_offset
            .checked_add(1)
            .ok_or_else(|| invalid_argument("HNSW full-scan node offset exceeds u64::MAX"))?;
    }
    Ok(())
}

fn build_output(
    selected: Vec<Candidate>,
    schema: Option<Vec<LogicalType>>,
) -> DevonResult<VecDeque<Chunk>> {
    if selected.is_empty() {
        return Ok(VecDeque::new());
    }
    let mut types = schema.ok_or_else(|| corrupt("HNSW selected rows without a source schema"))?;
    types.push(LogicalType::Float64);
    let mut output = VecDeque::new();
    let mut builder = ChunkBuilder::new(types.clone());
    for candidate in selected {
        if builder.is_full() {
            output.push_back(builder.finish());
            builder = ChunkBuilder::new(types.clone());
        }
        let mut values = candidate
            .values
            .ok_or_else(|| corrupt("HNSW selected row was not materialized"))?;
        values.push(Value::Float64(f64::from(candidate.distance)));
        builder.push_row(values)?;
    }
    output.push_back(builder.finish());
    Ok(output)
}

fn vector_distance(
    vector: &[f32],
    query: &[f32],
    metric: Metric,
    node_offset: u64,
) -> DevonResult<f32> {
    if vector.len() != query.len() {
        return Err(invalid_argument(format!(
            "HNSW rescore vector length {} does not match query length {} at node {node_offset}",
            vector.len(),
            query.len()
        )));
    }
    Ok(match metric {
        Metric::L2 => simd::l2_squared(vector, query).sqrt(),
        Metric::Cosine => simd::cosine_distance(vector, query),
    })
}

fn rescore_materialized(
    candidates: &mut [Candidate],
    vector_column: usize,
    query: &[f32],
    metric: Metric,
) -> DevonResult<()> {
    for candidate in candidates {
        let values = candidate
            .values
            .as_ref()
            .ok_or_else(|| corrupt("HNSW indexed candidate was not materialized"))?;
        let vector = match values.get(vector_column) {
            Some(Value::Vector(vector)) => vector,
            Some(Value::Null) => {
                return Err(corrupt(format!(
                    "HNSW candidate {} has a null vector",
                    candidate.node_offset
                )));
            }
            _ => return Err(corrupt("HNSW indexed candidate has a non-vector value")),
        };
        candidate.distance = vector_distance(vector, query, metric, candidate.node_offset)?;
    }
    Ok(())
}

fn merge_schema(
    schema: &mut Option<Vec<LogicalType>>,
    tail_schema: Option<Vec<LogicalType>>,
) -> DevonResult<()> {
    if let Some(tail_schema) = tail_schema {
        set_or_validate_types(&tail_schema, schema, "HNSW exact-tail")?;
    }
    Ok(())
}

fn metric_matches(index_metric: HnswMetric, metric: Metric) -> bool {
    matches!(
        (index_metric, metric),
        (HnswMetric::L2, Metric::L2) | (HnswMetric::Cosine, Metric::Cosine)
    )
}

fn effective_ef_search(k: usize, navigation: NavigationEncoding) -> DevonResult<usize> {
    let mut ef_search = k.max(DEFAULT_EF_SEARCH);
    if navigation == NavigationEncoding::B1 {
        ef_search = ef_search.max(
            k.checked_mul(B1_EF_SEARCH_FACTOR)
                .ok_or_else(|| invalid_argument("b1 HNSW candidate width exceeds usize::MAX"))?,
        );
    }
    Ok(ef_search)
}

fn reserve_ann_working_set(
    budget: &MemoryBudget,
    k: usize,
    dimension: usize,
    navigation: NavigationEncoding,
) -> DevonResult<ChargedBytes<'_>> {
    let width = effective_ef_search(k, navigation)?;
    let entries = width
        .checked_mul(2)
        .and_then(|value| k.checked_mul(3).and_then(|extra| value.checked_add(extra)))
        .ok_or_else(|| invalid_argument("HNSW executor candidate bound exceeds usize::MAX"))?;
    let candidate_bytes = size_of::<Candidate>()
        .checked_add(ALLOCATION_OVERHEAD)
        .and_then(|entry| entry.checked_mul(entries))
        .ok_or_else(|| invalid_argument("HNSW executor arena size exceeds usize::MAX"))?;
    let vector_bytes = dimension
        .checked_mul(size_of::<f32>())
        .and_then(|bytes| bytes.checked_mul(3))
        .ok_or_else(|| invalid_argument("HNSW vector scratch size exceeds usize::MAX"))?;
    let bytes = ALLOCATION_OVERHEAD
        .checked_add(candidate_bytes)
        .and_then(|bytes| bytes.checked_add(vector_bytes))
        .ok_or_else(|| invalid_argument("HNSW executor arena size exceeds usize::MAX"))?;
    ChargedBytes::try_new(budget, bytes, || "HNSW executor arena".to_owned())
}

/// Output working-set charge (`docs/SCALE.md` §6.6). The retained
/// `VecDeque<Chunk>` holds typed columns, so the charge is each
/// column's [`crate::column::Column::approx_bytes`] — `len × width` + bitmap
/// words for fixed-width storage, per-value approx bytes for `Boxed`.
/// Materialized-value accounting would add a 64-byte row overhead and charge
/// 16 bytes for every Int64 actually held at 8 bytes, overstating this
/// working set by roughly 2× and triggering the §7.2 ladder early.
fn output_charge_bytes(chunks: &VecDeque<Chunk>) -> DevonResult<usize> {
    chunks.iter().try_fold(0_usize, |total, chunk| {
        (0..chunk.column_count()).try_fold(total, |total, column| {
            let column = chunk
                .column(column)
                .ok_or_else(|| corrupt("HNSW output chunk is missing a charged column"))?;
            total
                .checked_add(column.approx_bytes())
                .ok_or_else(|| invalid_argument("HNSW output charge exceeds usize::MAX"))
        })
    })
}

fn validate_schema(
    chunk: &Chunk,
    schema: &mut Option<Vec<LogicalType>>,
    context: &str,
) -> DevonResult<()> {
    set_or_validate_types(chunk.types(), schema, context)
}

fn set_or_validate_types(
    types: &[LogicalType],
    schema: &mut Option<Vec<LogicalType>>,
    context: &str,
) -> DevonResult<()> {
    if let Some(expected) = schema {
        if expected.as_slice() != types {
            return Err(invalid_argument(format!(
                "{context} source schema changed between chunks"
            )));
        }
    } else {
        *schema = Some(types.to_vec());
    }
    Ok(())
}

fn clone_row(chunk: &Chunk, row: usize) -> DevonResult<Vec<Value>> {
    (0..chunk.column_count())
        .map(|column| clone_value(chunk, row, column))
        .collect()
}

fn clone_value(chunk: &Chunk, row: usize, column: usize) -> DevonResult<Value> {
    chunk.value(row, column).ok_or_else(|| {
        invalid_argument(format!(
            "HNSW cannot copy missing row {row}, column {column}"
        ))
    })
}

fn invalid_argument(context: impl Into<String>) -> DevonError {
    DevonError::InvalidArgument {
        context: context.into(),
    }
}

fn corrupt(context: impl Into<String>) -> DevonError {
    DevonError::Corrupt {
        context: context.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};

    use devondb_plan::expr::Metric;
    use devondb_storage::{
        budget::MemoryBudget,
        hnsw::types::{GraphAccess, HnswConfig, HnswMetric, NavigationEncoding},
    };
    use devondb_types::{
        DevonResult,
        logical_type::{B1Rescore, LogicalType, VectorEncoding},
        value::Value,
    };

    use super::{DistanceAccessor, FullScanFactory, KnnScan, RescoreAccessor};
    use crate::{
        chunk::{Chunk, ChunkBuilder},
        knn::KnnScan as ExactKnnScan,
        simd,
        source::ChunkSource,
    };

    struct MapGraph {
        covered_rows: u64,
        neighbors: BTreeMap<(u8, u64), Vec<u64>>,
    }

    impl MapGraph {
        fn complete(covered_rows: u64) -> Self {
            let mut neighbors = BTreeMap::new();
            for node in 0..covered_rows {
                let list = (0..covered_rows)
                    .rev()
                    .filter(|neighbor| *neighbor != node)
                    .collect();
                neighbors.insert((0, node), list);
            }
            Self {
                covered_rows,
                neighbors,
            }
        }
    }

    impl GraphAccess for MapGraph {
        fn entry(&self) -> Option<(u64, u8)> {
            (self.covered_rows != 0).then_some((0, 0))
        }

        fn layer_count(&self) -> u8 {
            u8::from(self.covered_rows != 0)
        }

        fn covered_rows(&self) -> u64 {
            self.covered_rows
        }

        fn neighbors(&self, layer: u8, node: u64, scratch: &mut Vec<u64>) -> DevonResult<()> {
            scratch.clear();
            if let Some(neighbors) = self.neighbors.get(&(layer, node)) {
                scratch.extend_from_slice(neighbors);
            }
            Ok(())
        }
    }

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

    #[test]
    fn hnsw_severed_union_keeps_nearest_exact_tail_row() {
        let rows = rows(&[(10, [10.0, 0.0]), (11, [20.0, 0.0]), (12, [0.25, 0.0])]);
        let budget = MemoryBudget::unlimited();
        let output = run_ann(
            rows,
            2,
            vec![10.0, 20.0],
            None,
            vec![0.0, 0.0],
            2,
            Metric::L2,
            NavigationEncoding::F32,
            &budget,
        );

        assert_eq!(ids(&output), vec![12, 10]);
        assert_eq!(distances(&output), vec![0.25, 10.0]);
    }

    #[test]
    fn hnsw_severed_rescore_uses_b1_rescored_order() {
        let rows = rows(&[(20, [0.0, 1.0]), (21, [1.0, 0.0]), (22, [-1.0, 0.0])]);
        let rescored = rows
            .iter()
            .map(|row| match &row[1] {
                Value::Vector(vector) => vector.clone(),
                _ => unreachable!(),
            })
            .collect();
        let budget = MemoryBudget::unlimited();
        let output = run_ann(
            rows,
            3,
            vec![0.0, 0.2, 0.1],
            Some(rescored),
            vec![1.0, 0.0],
            1,
            Metric::Cosine,
            NavigationEncoding::B1,
            &budget,
        );

        assert_eq!(ids(&output), vec![21]);
        assert_eq!(distances(&output), vec![0.0]);
    }

    #[test]
    fn hnsw_budget_fallback_equals_brute_and_releases_ann_charges() {
        let rows = rows(&[(30, [3.0, 0.0]), (31, [1.0, 0.0]), (32, [2.0, 0.0])]);
        let query = vec![0.0, 0.0];
        let expected = run_brute(rows.clone(), query.clone(), 2, Metric::L2);
        let budget = MemoryBudget::new(30_000);
        assert!(budget.try_charge(17));
        let charged_before = budget.charged();
        let output = run_ann(
            rows,
            2,
            vec![3.0, 1.0],
            None,
            query,
            2,
            Metric::L2,
            NavigationEncoding::F32,
            &budget,
        );

        assert_eq!(output, expected);
        assert_eq!(budget.charged(), charged_before);
        budget.release(charged_before);
    }

    #[test]
    fn hnsw_final_ties_break_by_node_offset() {
        let rows = rows(&[(40, [1.0, 0.0]), (10, [-1.0, 0.0]), (30, [0.0, 1.0])]);
        let budget = MemoryBudget::unlimited();
        let output = run_ann(
            rows,
            3,
            vec![1.0, 1.0, 1.0],
            None,
            vec![0.0, 0.0],
            3,
            Metric::L2,
            NavigationEncoding::F32,
            &budget,
        );

        assert_eq!(ids(&output), vec![40, 10, 30]);
    }

    #[test]
    fn hnsw_f32_f16_and_i8_final_distances_equal_brute_distances() {
        let rows = rows(&[(50, [1.0, 1.0]), (51, [4.0, 5.0]), (52, [-1.0, 2.0])]);
        let query = vec![1.0, 2.0];
        let navigation: Vec<f32> = rows
            .iter()
            .map(|row| match &row[1] {
                Value::Vector(vector) => simd::l2_squared(vector, &query).sqrt(),
                _ => unreachable!(),
            })
            .collect();
        let expected = run_brute(rows.clone(), query.clone(), 3, Metric::L2);
        for encoding in [
            NavigationEncoding::F32,
            NavigationEncoding::F16,
            NavigationEncoding::I8,
        ] {
            let budget = MemoryBudget::unlimited();
            let output = run_ann(
                rows.clone(),
                3,
                navigation.clone(),
                None,
                query.clone(),
                3,
                Metric::L2,
                encoding,
                &budget,
            );
            assert_eq!(distances(&output), distances(&expected));
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn run_ann(
        rows: Vec<Vec<Value>>,
        covered_rows: u64,
        navigation: Vec<f32>,
        rescore_vectors: Option<Vec<Vec<f32>>>,
        query: Vec<f32>,
        k: u64,
        metric: Metric,
        encoding: NavigationEncoding,
        budget: &MemoryBudget,
    ) -> Vec<Chunk> {
        let graph = MapGraph::complete(covered_rows);
        let config = config(metric, encoding);
        let full_chunks = vec![encoded_chunk(rows.clone(), encoding)];
        let tail_rows = rows[covered_rows as usize..].to_vec();
        let tail: Box<dyn ChunkSource> =
            Box::new(VecSource::new(vec![encoded_chunk(tail_rows, encoding)]));
        let distance: Box<DistanceAccessor<'_>> =
            Box::new(move |node| Ok(navigation[usize::try_from(node).unwrap()]));
        let rescore: Option<Box<RescoreAccessor<'_>>> = rescore_vectors.map(|vectors| {
            Box::new(move |node| Ok(vectors[usize::try_from(node).unwrap()].clone()))
                as Box<RescoreAccessor<'_>>
        });
        let factory_chunks = full_chunks.clone();
        let full_scan: Box<FullScanFactory<'_>> = Box::new(move || {
            Box::new(VecSource::new(factory_chunks.clone())) as Box<dyn ChunkSource>
        });
        let mut scan = KnnScan::new(
            graph, config, distance, rescore, tail, full_scan, 1, query, k, metric, budget,
        );
        collect(&mut scan)
    }

    fn run_brute(rows: Vec<Vec<Value>>, query: Vec<f32>, k: u64, metric: Metric) -> Vec<Chunk> {
        let source = VecSource::new(vec![chunk(rows)]);
        let mut scan = ExactKnnScan::new(Box::new(source), 1, query, k, metric);
        collect(&mut scan)
    }

    fn collect(source: &mut dyn ChunkSource) -> Vec<Chunk> {
        let mut chunks = Vec::new();
        while let Some(chunk) = source.next_chunk().unwrap() {
            chunks.push(chunk);
        }
        chunks
    }

    fn config(metric: Metric, navigation: NavigationEncoding) -> HnswConfig {
        let metric = match metric {
            Metric::L2 => HnswMetric::L2,
            Metric::Cosine => HnswMetric::Cosine,
        };
        HnswConfig::with_defaults(7, metric, navigation)
    }

    fn rows(rows: &[(i64, [f32; 2])]) -> Vec<Vec<Value>> {
        rows.iter()
            .map(|(id, vector)| vec![Value::Int64(*id), Value::Vector(vector.to_vec())])
            .collect()
    }

    fn chunk(rows: Vec<Vec<Value>>) -> Chunk {
        encoded_chunk(rows, NavigationEncoding::F32)
    }

    fn encoded_chunk(rows: Vec<Vec<Value>>, encoding: NavigationEncoding) -> Chunk {
        let vector_type = match encoding {
            NavigationEncoding::F32 => LogicalType::Vector { dim: 2 },
            NavigationEncoding::F16 => LogicalType::VectorEncoded {
                dim: 2,
                encoding: VectorEncoding::F16,
            },
            NavigationEncoding::I8 => LogicalType::VectorEncoded {
                dim: 2,
                encoding: VectorEncoding::I8,
            },
            NavigationEncoding::B1 => LogicalType::VectorEncoded {
                dim: 2,
                encoding: VectorEncoding::B1 {
                    rotation_seed: 7,
                    rescore: B1Rescore::F32,
                },
            },
        };
        let mut builder = ChunkBuilder::new(vec![LogicalType::Int64, vector_type]);
        for row in rows {
            builder.push_row(row).unwrap();
        }
        builder.finish()
    }

    fn ids(chunks: &[Chunk]) -> Vec<i64> {
        chunks
            .iter()
            .flat_map(Chunk::rows)
            .map(|row| match &row[0] {
                Value::Int64(value) => *value,
                other => panic!("expected Int64 id, got {other}"),
            })
            .collect()
    }

    fn distances(chunks: &[Chunk]) -> Vec<f64> {
        chunks
            .iter()
            .flat_map(Chunk::rows)
            .map(|row| match row.last() {
                Some(Value::Float64(value)) => *value,
                other => panic!("expected trailing distance, got {other:?}"),
            })
            .collect()
    }
}
