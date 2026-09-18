//! WAL-free bulk builders for `COPY` (`docs/SCALE.md` §5.4).
//!
//! Node rows stream into fresh groups as each group fills. Relationship
//! edges stay resident because CSR construction needs to group them in both
//! directions, but every retained edge charges the shared memory budget.
//! Neither builder touches the WAL — crash safety is the bulk fence
//! (`docs/SCALE.md` §5.3), not this module's concern.

use devondb_types::{
    DevonError, DevonResult, GeoPoint,
    logical_type::LogicalType,
    schema::{NodeTableSchema, RelTableSchema},
    value::Value,
};

use crate::budget::MemoryBudget;
use crate::catalog::{Catalog, RelStorage, TableStorage};
use crate::node_group::{NODE_GROUP_CAPACITY, NodeGroup};
use crate::node_table::validate_row;
use crate::overlay::OVERLAY_EDGE_OVERHEAD_BYTES;
use crate::pager::Pager;
use crate::rel_table::RelTable;

// Per input edge, reserve the canonical checkpoint's directional-map entry
// plus neighbor/property-vector capacity. Property values are charged twice:
// once in RelTable's input buffer and once for the direction being built.
const CSR_BUILD_EDGE_OVERHEAD_BYTES: usize = 40;

/// Returns the DevonGrid atom key used to order a GeoPoint bulk sort.
///
/// This deliberately follows the same resolution-15 assignment path as
/// GeoPoint zone-map statistics. COPY sorts nulls separately and places them
/// after every non-null atom key.
pub fn geo_atom_sort_key(point: GeoPoint) -> DevonResult<u64> {
    devondb_geo::grid::atom(point.lat_deg(), point.lng_deg())
        .map(|atom| atom.raw())
        .map_err(|error| DevonError::InvalidArgument {
            context: format!("GeoPoint cannot be assigned a DevonGrid atom: {error}"),
        })
}

/// A streaming, WAL-free builder for a node table's persisted group list.
///
/// Full groups are written immediately and removed from memory. If the
/// existing storage ends in a partial group, that group is decoded into the
/// initial buffer and its old directory id is omitted from the returned
/// storage, preserving immutable tail-rewrite semantics.
pub struct BulkNodeWriter {
    schema: NodeTableSchema,
    groups: Vec<u64>,
    buffered: NodeGroup,
}

impl BulkNodeWriter {
    /// Starts a bulk writer from the table's current persisted storage.
    ///
    /// Existing full groups are retained by id. A partial final group is read
    /// into the one-group buffer so new rows top it up before another group is
    /// started.
    pub fn new(
        schema: NodeTableSchema,
        existing: Option<&TableStorage>,
        pager: &Pager,
    ) -> DevonResult<Self> {
        let types = schema_types(&schema);
        let mut groups = existing.cloned().unwrap_or_default().groups;
        let buffered = load_partial_tail(pager, &types, &mut groups)?;
        Ok(Self {
            schema,
            groups,
            buffered,
        })
    }

    /// Validates and appends one row, flushing immediately at group capacity.
    ///
    /// A validation error leaves the writer unchanged. The writer remains
    /// usable, and [`Self::finish`] includes only rows accepted successfully.
    pub fn push_row(&mut self, pager: &Pager, row: Vec<Value>) -> DevonResult<()> {
        validate_row(&self.schema, &row)?;
        self.buffered.push_row(row)?;
        if self.buffered.row_count() == NODE_GROUP_CAPACITY {
            self.flush_buffered(pager)?;
        }
        Ok(())
    }

    /// Flushes the final partial group and returns the unpublished storage.
    pub fn finish(mut self, pager: &Pager) -> DevonResult<TableStorage> {
        if self.buffered.row_count() != 0 {
            self.flush_buffered(pager)?;
        }
        Ok(TableStorage {
            groups: self.groups,
        })
    }

    fn flush_buffered(&mut self, pager: &Pager) -> DevonResult<()> {
        let empty = NodeGroup::new(schema_types(&self.schema))?;
        let group_id = self.buffered.write(pager)?;
        self.groups.push(group_id);
        self.buffered = empty;
        Ok(())
    }

    #[cfg(test)]
    fn buffered_row_count(&self) -> usize {
        self.buffered.row_count()
    }
}

/// A WAL-free builder for a relationship table's forward and backward CSR.
///
/// Edges remain buffered until [`Self::finish`] because the canonical
/// relationship checkpoint path groups the same input in both directions.
/// The caller must keep the ordinary COPY bulk fence held until the returned
/// storage is published through the catalog.
pub struct BulkRelWriter<'budget> {
    table: RelTable,
    charge: ReclaimingCharge<'budget>,
}

impl<'budget> BulkRelWriter<'budget> {
    /// Starts an empty relationship bulk writer under `budget`.
    pub fn new(schema: RelTableSchema, budget: &'budget MemoryBudget) -> Self {
        Self {
            table: RelTable::new(schema),
            charge: ReclaimingCharge::new(budget),
        }
    }

    /// Validates and retains one resolved edge without writing the WAL.
    ///
    /// Validation uses [`RelTable::recover_edge`], the same row-shape path as
    /// relationship INSERT recovery. A rejected edge releases its tentative
    /// memory charge and leaves the writer usable.
    pub fn push_edge(&mut self, from: u64, to: u64, values: Vec<Value>) -> DevonResult<()> {
        let bytes = retained_edge_bytes(&values)?;
        self.charge.grow(bytes, || {
            format!("COPY relationship edge buffer requested {bytes} additional bytes")
        })?;
        if let Err(error) = self.table.recover_edge(from, to, values) {
            self.charge.shrink(bytes);
            return Err(error);
        }
        Ok(())
    }

    /// Builds canonical CSR groups and returns their unpublished storage.
    ///
    /// `catalog` must be an unpublished clone owned by the COPY operation.
    /// The canonical checkpoint implementation updates that clone; the
    /// caller atomically publishes it only after this method succeeds.
    pub fn finish(mut self, pager: &Pager, catalog: &mut Catalog) -> DevonResult<RelStorage> {
        let table_name = self.table.schema().name().to_owned();
        self.table.checkpoint(pager, catalog)?;
        Ok(catalog
            .rel_storage(&table_name)
            .cloned()
            .unwrap_or_default())
    }
}

fn retained_edge_bytes(values: &[Value]) -> DevonResult<usize> {
    let base = OVERLAY_EDGE_OVERHEAD_BYTES
        .checked_add(CSR_BUILD_EDGE_OVERHEAD_BYTES)
        .ok_or_else(edge_charge_overflow)?;
    values.iter().try_fold(base, |total, value| {
        let value_bytes = value
            .approx_bytes()
            .checked_mul(2)
            .ok_or_else(edge_charge_overflow)?;
        total
            .checked_add(value_bytes)
            .ok_or_else(edge_charge_overflow)
    })
}

fn edge_charge_overflow() -> DevonError {
    DevonError::BudgetExceeded {
        context: "COPY relationship edge charge exceeds usize::MAX".to_owned(),
    }
}

struct ReclaimingCharge<'budget> {
    budget: &'budget MemoryBudget,
    bytes: usize,
}

impl<'budget> ReclaimingCharge<'budget> {
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

    fn shrink(&mut self, bytes: usize) {
        let released = self.bytes.min(bytes);
        self.bytes -= released;
        self.budget.release(released);
    }
}

impl Drop for ReclaimingCharge<'_> {
    fn drop(&mut self) {
        self.budget.release(self.bytes);
    }
}

fn load_partial_tail(
    pager: &Pager,
    types: &[LogicalType],
    groups: &mut Vec<u64>,
) -> DevonResult<NodeGroup> {
    let Some(tail_id) = groups.last().copied() else {
        return NodeGroup::new(types.to_vec());
    };
    let tail = NodeGroup::read(pager, tail_id, types)?;
    if tail.row_count() >= NODE_GROUP_CAPACITY {
        return NodeGroup::new(types.to_vec());
    }
    let _old_tail = groups.pop();
    Ok(tail)
}

fn schema_types(schema: &NodeTableSchema) -> Vec<LogicalType> {
    schema.columns().iter().map(|column| column.ty).collect()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use devondb_types::{
        DevonError,
        logical_type::LogicalType,
        schema::{Column, NodeTableSchema},
        value::Value,
    };
    use tempfile::tempdir;

    use super::{BulkNodeWriter, BulkRelWriter, NODE_GROUP_CAPACITY, schema_types};
    use crate::budget::MemoryBudget;
    use crate::catalog::{Catalog, TableStorage};
    use crate::node_group::{NodeGroup, ZoneMapValue};
    use crate::node_table::NodeTable;
    use crate::pager::Pager;
    use crate::rel_table::{Direction, RelTable};
    use crate::wal::WalWriter;

    const PAGE_SIZE: u32 = 4096;
    const DB_ID: [u8; 16] = *b"bulk-node-db-id!";

    fn schema() -> NodeTableSchema {
        NodeTableSchema::new(
            "Person".to_owned(),
            vec![
                Column {
                    name: "id".to_owned(),
                    ty: LogicalType::Int64,
                    primary_key: true,
                },
                Column {
                    name: "name".to_owned(),
                    ty: LogicalType::String,
                    primary_key: false,
                },
            ],
        )
        .unwrap()
    }

    fn rel_schema() -> devondb_types::schema::RelTableSchema {
        devondb_types::schema::RelTableSchema::new(
            "Knows".to_owned(),
            "Person".to_owned(),
            "Person".to_owned(),
            vec![Column {
                name: "since".to_owned(),
                ty: LogicalType::Int64,
                primary_key: false,
            }],
        )
        .unwrap()
    }

    fn row(id: usize) -> Vec<Value> {
        vec![
            Value::Int64(id as i64),
            Value::String(format!("person-{id}")),
        ]
    }

    /// Commits the superblock feature bits a governed read of freshly
    /// written groups requires (`ZONE_MAPS` + `COLUMN_ENCODINGS`): these
    /// tests read before any catalog save, which is the code that derives
    /// the bits in production. The default writer selects encodings, so
    /// directories may carry flags bit 1.
    fn publish_feature_bits(pager: &Pager) {
        let mut superblock = pager.superblock();
        superblock.feature_flags |=
            crate::superblock::ZONE_MAPS_FLAG | crate::superblock::COLUMN_ENCODINGS_FLAG;
        superblock.checkpoint_lsn += 1;
        pager.commit_superblock(superblock).unwrap();
    }

    fn publish_and_scan(
        pager: &Pager,
        table_schema: NodeTableSchema,
        storage: TableStorage,
    ) -> Vec<Vec<Value>> {
        publish_feature_bits(pager);
        let mut catalog = Catalog::default();
        catalog.add_node_table(table_schema.clone()).unwrap();
        catalog
            .set_table_storage(table_schema.name(), storage)
            .unwrap();
        NodeTable::new(table_schema).scan(pager, &catalog).unwrap()
    }

    fn group_row_counts(
        pager: &Pager,
        table_schema: &NodeTableSchema,
        storage: &TableStorage,
    ) -> Vec<usize> {
        publish_feature_bits(pager);
        let types = schema_types(table_schema);
        storage
            .groups
            .iter()
            .map(|group_id| {
                NodeGroup::read(pager, *group_id, &types)
                    .unwrap()
                    .row_count()
            })
            .collect()
    }

    #[test]
    fn round_trip_streams_five_thousand_rows_in_order() {
        let directory = tempdir().unwrap();
        let pager = Pager::create(
            directory.path().join("round-trip.devondb"),
            PAGE_SIZE,
            DB_ID,
        )
        .unwrap();
        let table_schema = schema();
        let expected = (0..5_000).map(row).collect::<Vec<_>>();
        let mut writer = BulkNodeWriter::new(table_schema.clone(), None, &pager).unwrap();
        for value in &expected {
            writer.push_row(&pager, value.clone()).unwrap();
        }

        let storage = writer.finish(&pager).unwrap();

        assert_eq!(storage.groups.len(), 3);
        assert_eq!(publish_and_scan(&pager, table_schema, storage), expected);
    }

    #[test]
    fn partial_existing_tail_is_rewritten_and_topped_up() {
        let directory = tempdir().unwrap();
        let pager = Pager::create(
            directory.path().join("tail-top-up.devondb"),
            PAGE_SIZE,
            DB_ID,
        )
        .unwrap();
        let table_schema = schema();
        let mut old_tail = NodeGroup::new(schema_types(&table_schema)).unwrap();
        for id in 0..100 {
            old_tail.push_row(row(id)).unwrap();
        }
        let old_tail_id = old_tail.write(&pager).unwrap();
        publish_feature_bits(&pager);
        let existing = TableStorage {
            groups: vec![old_tail_id],
        };
        let mut writer =
            BulkNodeWriter::new(table_schema.clone(), Some(&existing), &pager).unwrap();
        assert_eq!(writer.buffered_row_count(), 100);
        for id in 100..3_100 {
            writer.push_row(&pager, row(id)).unwrap();
        }

        let storage = writer.finish(&pager).unwrap();

        assert!(!storage.groups.contains(&old_tail_id));
        assert_eq!(
            group_row_counts(&pager, &table_schema, &storage),
            vec![NODE_GROUP_CAPACITY, 3_100 - NODE_GROUP_CAPACITY]
        );
        assert_eq!(
            publish_and_scan(&pager, table_schema, storage),
            (0..3_100).map(row).collect::<Vec<_>>()
        );
    }

    #[test]
    fn capacity_edges_have_exact_group_placement() {
        let directory = tempdir().unwrap();
        let cases = [
            (NODE_GROUP_CAPACITY - 1, vec![NODE_GROUP_CAPACITY - 1]),
            (NODE_GROUP_CAPACITY, vec![NODE_GROUP_CAPACITY]),
            (NODE_GROUP_CAPACITY + 1, vec![NODE_GROUP_CAPACITY, 1]),
        ];
        for (case_index, (row_count, expected_counts)) in cases.into_iter().enumerate() {
            let pager = Pager::create(
                directory
                    .path()
                    .join(format!("capacity-{case_index}.devondb")),
                PAGE_SIZE,
                DB_ID,
            )
            .unwrap();
            let table_schema = schema();
            let mut writer = BulkNodeWriter::new(table_schema.clone(), None, &pager).unwrap();
            for id in 0..row_count {
                writer.push_row(&pager, row(id)).unwrap();
            }

            let storage = writer.finish(&pager).unwrap();

            assert_eq!(
                group_row_counts(&pager, &table_schema, &storage),
                expected_counts
            );
            assert_eq!(
                publish_and_scan(&pager, table_schema, storage),
                (0..row_count).map(row).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn validation_matches_insert_and_keeps_only_valid_rows() {
        let directory = tempdir().unwrap();
        let db_path = directory.path().join("validation.devondb");
        let wal_path = directory.path().join("validation.devondb-wal");
        let pager = Pager::create(&db_path, PAGE_SIZE, DB_ID).unwrap();
        let table_schema = schema();
        let mut table = NodeTable::new(table_schema.clone());
        let mut wal = WalWriter::open(&wal_path, 1).unwrap();
        let invalid_rows = [
            vec![Value::Int64(1)],
            vec![Value::Int64(1), Value::Bool(true)],
        ];
        let insert_errors = invalid_rows
            .iter()
            .map(|invalid| table.insert(&mut wal, invalid.clone()).unwrap_err())
            .collect::<Vec<_>>();
        let mut writer = BulkNodeWriter::new(table_schema.clone(), None, &pager).unwrap();
        writer.push_row(&pager, row(7)).unwrap();
        let file_len_before_errors = fs::metadata(&db_path).unwrap().len();

        for (invalid, insert_error) in invalid_rows.into_iter().zip(insert_errors) {
            let bulk_error = writer.push_row(&pager, invalid).unwrap_err();
            assert!(matches!(bulk_error, DevonError::InvalidArgument { .. }));
            assert_eq!(bulk_error.to_string(), insert_error.to_string());
        }

        assert_eq!(
            fs::metadata(&db_path).unwrap().len(),
            file_len_before_errors
        );
        assert_eq!(fs::metadata(wal_path).unwrap().len(), 0);
        let storage = writer.finish(&pager).unwrap();
        assert_eq!(
            publish_and_scan(&pager, table_schema, storage),
            vec![row(7)]
        );
    }

    #[test]
    fn bulk_group_contains_int64_zone_maps() {
        let directory = tempdir().unwrap();
        let pager =
            Pager::create(directory.path().join("zone-maps.devondb"), PAGE_SIZE, DB_ID).unwrap();
        let table_schema = schema();
        let types = schema_types(&table_schema);
        let mut writer = BulkNodeWriter::new(table_schema, None, &pager).unwrap();
        for id in [7_i64, -4, 19] {
            writer
                .push_row(
                    &pager,
                    vec![Value::Int64(id), Value::String(id.to_string())],
                )
                .unwrap();
        }
        let storage = writer.finish(&pager).unwrap();
        let group_id = storage.groups[0];

        let group = NodeGroup::read(&pager, group_id, &types).unwrap();
        assert_eq!(group.row_count(), 3);
        let directory = NodeGroup::read_directory(&pager, group_id, &types).unwrap();
        let zone_maps = directory.zone_maps().unwrap();
        assert_eq!(zone_maps[0].null_count, 0);
        assert_eq!(zone_maps[0].min, Some(ZoneMapValue::Int64(-4)));
        assert_eq!(zone_maps[0].max, Some(ZoneMapValue::Int64(19)));
    }

    #[test]
    fn three_full_groups_leave_at_most_one_group_buffered() {
        let directory = tempdir().unwrap();
        let pager =
            Pager::create(directory.path().join("streaming.devondb"), PAGE_SIZE, DB_ID).unwrap();
        let mut writer = BulkNodeWriter::new(schema(), None, &pager).unwrap();
        for id in 0..(3 * NODE_GROUP_CAPACITY) {
            writer.push_row(&pager, row(id)).unwrap();
            assert!(writer.buffered_row_count() <= NODE_GROUP_CAPACITY);
        }

        assert_eq!(writer.groups.len(), 3);
        assert_eq!(writer.buffered_row_count(), 0);
    }

    #[test]
    fn bulk_rel_round_trip_is_byte_compatible_with_checkpoint() {
        let directory = tempdir().unwrap();
        let baseline_path = directory.path().join("rel-baseline.devondb");
        let bulk_path = directory.path().join("rel-bulk.devondb");
        let baseline_pager = Pager::create(&baseline_path, PAGE_SIZE, DB_ID).unwrap();
        let bulk_pager = Pager::create(&bulk_path, PAGE_SIZE, DB_ID).unwrap();
        let mut baseline_catalog = catalog_with_nodes(&baseline_pager);
        let mut bulk_catalog = catalog_with_nodes(&bulk_pager);
        seed_existing_edge(&baseline_pager, &mut baseline_catalog);
        seed_existing_edge(&bulk_pager, &mut bulk_catalog);
        let edges = [
            (0, 2, vec![Value::Int64(1843)]),
            (2, 1, vec![Value::Int64(1957)]),
        ];

        let mut checkpoint = RelTable::new(rel_schema());
        for (from, to, values) in &edges {
            checkpoint.recover_edge(*from, *to, values.clone()).unwrap();
        }
        checkpoint
            .checkpoint(&baseline_pager, &mut baseline_catalog)
            .unwrap();

        let budget = MemoryBudget::unlimited();
        let mut bulk = BulkRelWriter::new(rel_schema(), &budget);
        for (from, to, values) in &edges {
            bulk.push_edge(*from, *to, values.clone()).unwrap();
        }
        let storage = bulk.finish(&bulk_pager, &mut bulk_catalog).unwrap();

        assert_eq!(bulk_catalog.rel_storage("Knows"), Some(&storage));
        assert_eq!(
            bulk_catalog.rel_storage("Knows"),
            baseline_catalog.rel_storage("Knows")
        );
        assert_eq!(
            fs::read(bulk_path).unwrap(),
            fs::read(baseline_path).unwrap()
        );
        assert_eq!(budget.charged(), 0);
        assert_rel_neighbors(&bulk_pager, &bulk_catalog);
    }

    #[test]
    fn bulk_rel_budget_failure_releases_tentative_charge() {
        let budget = MemoryBudget::new(55);
        let mut writer = BulkRelWriter::new(rel_schema(), &budget);

        let error = writer
            .push_edge(0, 1, vec![Value::Int64(1843)])
            .unwrap_err();

        assert!(matches!(error, DevonError::BudgetExceeded { .. }));
        assert_eq!(budget.charged(), 0);
    }

    fn catalog_with_nodes(pager: &Pager) -> Catalog {
        let node_schema = schema();
        let mut writer = BulkNodeWriter::new(node_schema.clone(), None, pager).unwrap();
        for id in 0..3 {
            writer.push_row(pager, row(id)).unwrap();
        }
        let storage = writer.finish(pager).unwrap();
        let mut catalog = Catalog::default();
        catalog.add_node_table(node_schema).unwrap();
        catalog.add_rel_table(rel_schema()).unwrap();
        catalog.set_table_storage("Person", storage).unwrap();
        catalog
    }

    fn seed_existing_edge(pager: &Pager, catalog: &mut Catalog) {
        let mut relationships = RelTable::new(rel_schema());
        relationships
            .recover_edge(1, 0, vec![Value::Int64(1815)])
            .unwrap();
        relationships.checkpoint(pager, catalog).unwrap();
    }

    fn assert_rel_neighbors(pager: &Pager, catalog: &Catalog) {
        let relationships = RelTable::new(rel_schema());
        assert_eq!(
            relationships
                .neighbors(pager, catalog, Direction::Out, 0)
                .unwrap(),
            vec![2]
        );
        assert_eq!(
            relationships
                .neighbors(pager, catalog, Direction::In, 0)
                .unwrap(),
            vec![1]
        );
    }
}
