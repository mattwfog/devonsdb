//! Storage-backed adjacency for `run`-plan Expand: the facade's
//! `NeighborSource` implementation and primary-key → node-offset
//! resolution.

use devondb_exec::source::NeighborSource;
use devondb_plan::ops::Direction;
use devondb_storage::{
    catalog::Catalog,
    node_table::NodeTable,
    pager::Pager,
    rel_table::{Direction as StorageDirection, RelTable},
};
use devondb_types::{
    DevonError, DevonResult,
    schema::{RelTableSchema, fold},
    value::Value,
};

/// A build-time snapshot of one relationship traversal's adjacency and rows.
pub(crate) struct GraphSnapshot {
    rel: String,
    direction: Direction,
    adjacency: Vec<Vec<u64>>,
    node_rows: Vec<Vec<Value>>,
}

impl GraphSnapshot {
    /// Materializes adjacency for every node offset accepted by this traversal.
    pub(crate) fn materialize(
        table: &RelTable,
        pager: &Pager,
        catalog: &Catalog,
        direction: Direction,
        source_table: &str,
        source_row_count: usize,
        node_rows: Vec<Vec<Value>>,
    ) -> DevonResult<Self> {
        let destination = destination_table(table, direction, source_table)?;
        validate_destination_rows(destination, pager, catalog, &node_rows)?;
        let storage_direction = storage_direction(traversal_direction(
            table.schema(),
            direction,
            source_table,
        )?);
        let mut adjacency = Vec::with_capacity(source_row_count);
        for offset in 0..source_row_count {
            let offset = u64::try_from(offset)
                .map_err(|_| corrupt("node offset cannot be represented as u64"))?;
            adjacency.push(table.neighbors(pager, catalog, storage_direction, offset)?);
        }
        Ok(Self {
            rel: table.schema().name().to_owned(),
            direction,
            adjacency,
            node_rows,
        })
    }

    fn validate_request(&self, rel: &str, direction: Direction) -> DevonResult<()> {
        if fold(rel) != fold(&self.rel) || direction != self.direction {
            return Err(corrupt(format!(
                "Expand requested relationship `{rel}` direction {direction:?} from snapshot of `{}` direction {:?}",
                self.rel, self.direction
            )));
        }
        Ok(())
    }
}

fn validate_destination_rows(
    destination: &str,
    pager: &Pager,
    catalog: &Catalog,
    node_rows: &[Vec<Value>],
) -> DevonResult<()> {
    let schema = catalog
        .node_table(destination)
        .ok_or_else(|| DevonError::NotFound {
            what: format!("node table `{destination}`"),
        })?;
    let checkpointed_rows = NodeTable::new(schema.clone()).scan(pager, catalog)?;
    if node_rows.len() < checkpointed_rows.len() {
        return Err(invalid_argument(format!(
            "destination node rows for `{destination}` contain {} rows, fewer than its {} checkpointed rows",
            node_rows.len(),
            checkpointed_rows.len()
        )));
    }
    if !node_rows.starts_with(&checkpointed_rows) {
        return Err(invalid_argument(format!(
            "destination node rows for `{destination}` do not match its {} checkpointed rows",
            checkpointed_rows.len()
        )));
    }
    validate_destination_row_types(destination, schema.columns(), node_rows)
}

fn validate_destination_row_types(
    destination: &str,
    columns: &[devondb_types::schema::Column],
    rows: &[Vec<Value>],
) -> DevonResult<()> {
    for (row_index, row) in rows.iter().enumerate() {
        if row.len() != columns.len() {
            return Err(invalid_argument(format!(
                "destination node row {row_index} for `{destination}` has {} columns, expected {}",
                row.len(),
                columns.len()
            )));
        }
        for (column, value) in columns.iter().zip(row) {
            if !value.matches_type(&column.ty) {
                return Err(invalid_argument(format!(
                    "destination node row {row_index} for `{destination}` column `{}` expects {} but received {value}",
                    column.name, column.ty
                )));
            }
        }
    }
    Ok(())
}

fn destination_table<'table>(
    table: &'table RelTable,
    direction: Direction,
    source_table: &str,
) -> DevonResult<&'table str> {
    match traversal_direction(table.schema(), direction, source_table)? {
        Direction::Out | Direction::Both => Ok(table.schema().to()),
        Direction::In => Ok(table.schema().from()),
    }
}

/// Resolves a plan direction to the physical adjacency direction for a source table.
pub(crate) fn traversal_direction(
    schema: &RelTableSchema,
    direction: Direction,
    source_table: &str,
) -> DevonResult<Direction> {
    let source = fold(source_table);
    let from = fold(schema.from());
    let to = fold(schema.to());
    match direction {
        Direction::Out if source == from => Ok(Direction::Out),
        Direction::In if source == to => Ok(Direction::In),
        Direction::Both if from == to && source == from => Ok(Direction::Both),
        Direction::Both if source == from => Ok(Direction::Out),
        Direction::Both if source == to => Ok(Direction::In),
        _ => Err(invalid_argument(format!(
            "Expand direction {direction:?} on relationship `{}` does not accept source node table `{source_table}`",
            schema.name()
        ))),
    }
}

impl NeighborSource for GraphSnapshot {
    fn neighbors(&mut self, rel: &str, direction: Direction, from: u64) -> DevonResult<Vec<u64>> {
        self.validate_request(rel, direction)?;
        let index = usize::try_from(from)
            .map_err(|_| corrupt(format!("node offset {from} cannot index adjacency")))?;
        self.adjacency.get(index).cloned().ok_or_else(|| {
            corrupt(format!(
                "node offset {from} is outside relationship `{rel}` adjacency"
            ))
        })
    }

    fn node_row(
        &mut self,
        rel: &str,
        direction: Direction,
        offset: u64,
    ) -> DevonResult<Vec<Value>> {
        self.validate_request(rel, direction)?;
        let index = usize::try_from(offset)
            .map_err(|_| corrupt(format!("neighbor offset {offset} cannot index node rows")))?;
        self.node_rows.get(index).cloned().ok_or_else(|| {
            corrupt(format!(
                "neighbor offset {offset} from relationship `{rel}` has no node row"
            ))
        })
    }
}

/// Resolves a primary-key value to its stable scan-position node offset.
pub(crate) fn resolve_node_offset(
    table: &NodeTable,
    pager: &Pager,
    catalog: &Catalog,
    key: &Value,
) -> DevonResult<u64> {
    let (key_index, key_column) = table
        .schema()
        .columns()
        .iter()
        .enumerate()
        .find(|(_, column)| column.primary_key)
        .ok_or_else(|| {
            corrupt(format!(
                "node table `{}` has no primary-key column",
                table.schema().name()
            ))
        })?;
    if !key.matches_type(&key_column.ty) {
        return Err(invalid_argument(format!(
            "primary key `{}` in node table `{}` expects {} but received {key}",
            key_column.name,
            table.schema().name(),
            key_column.ty
        )));
    }

    let rows = table.scan(pager, catalog)?;
    let offset = rows
        .iter()
        .position(|row| row.get(key_index) == Some(key))
        .ok_or_else(|| DevonError::NotFound {
            what: format!("node table `{}` primary key {key}", table.schema().name()),
        })?;
    u64::try_from(offset).map_err(|_| corrupt("node offset cannot be represented as u64"))
}

const fn storage_direction(direction: Direction) -> StorageDirection {
    match direction {
        Direction::Out => StorageDirection::Out,
        Direction::In => StorageDirection::In,
        Direction::Both => StorageDirection::Both,
    }
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
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use devondb_plan::ops::Direction;
    use devondb_storage::{
        catalog::Catalog, node_table::NodeTable, pager::Pager, rel_table::RelTable,
    };
    use devondb_types::{
        DevonError,
        logical_type::LogicalType,
        schema::{Column, NodeTableSchema, RelTableSchema},
        value::Value,
    };

    use super::GraphSnapshot;

    const PAGE_SIZE: u32 = 4096;
    const DB_ID: [u8; 16] = *b"graph-snapshot!!";
    static NEXT_PATH: AtomicU64 = AtomicU64::new(0);

    struct TestDatabaseFile(PathBuf);

    impl TestDatabaseFile {
        fn create(name: &str) -> (Self, Pager) {
            let directory =
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/graph-tests");
            fs::create_dir_all(&directory).unwrap();
            let sequence = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
            let path = directory.join(format!("{name}-{}-{sequence}.devondb", std::process::id()));
            let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
            (Self(path), pager)
        }
    }

    impl Drop for TestDatabaseFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    fn person_schema() -> NodeTableSchema {
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

    fn city_schema() -> NodeTableSchema {
        NodeTableSchema::new(
            "City".to_owned(),
            vec![Column {
                name: "id".to_owned(),
                ty: LogicalType::Int64,
                primary_key: true,
            }],
        )
        .unwrap()
    }

    fn lives_in_schema() -> RelTableSchema {
        RelTableSchema::new(
            "LivesIn".to_owned(),
            "Person".to_owned(),
            "City".to_owned(),
            Vec::new(),
        )
        .unwrap()
    }

    fn checkpoint_rows(
        pager: &Pager,
        catalog: &mut Catalog,
        schema: NodeTableSchema,
        rows: Vec<Vec<Value>>,
    ) -> NodeTable {
        let mut table = NodeTable::new(schema);
        for row in rows {
            table.recover_row(row).unwrap();
        }
        table.checkpoint(pager, catalog).unwrap();
        table
    }

    #[test]
    fn materialize_rejects_rows_from_wrong_length_destination_table() {
        let (_file, pager) = TestDatabaseFile::create("wrong-destination");
        let person = person_schema();
        let city = city_schema();
        let mut catalog = Catalog::default();
        catalog.add_node_table(person.clone()).unwrap();
        catalog.add_node_table(city.clone()).unwrap();
        catalog.add_rel_table(lives_in_schema()).unwrap();
        let people = checkpoint_rows(
            &pager,
            &mut catalog,
            person,
            vec![
                vec![Value::Int64(1), Value::String("Ada".to_owned())],
                vec![Value::Int64(2), Value::String("Grace".to_owned())],
            ],
        );
        checkpoint_rows(&pager, &mut catalog, city, vec![vec![Value::Int64(10)]]);
        let wrong_rows = people.scan(&pager, &catalog).unwrap();
        let table = RelTable::new(lives_in_schema());

        let missing_rows = GraphSnapshot::materialize(
            &table,
            &pager,
            &catalog,
            Direction::Out,
            "Person",
            wrong_rows.len(),
            Vec::new(),
        );
        let Err(error) = missing_rows else {
            panic!("expected missing destination rows to be rejected");
        };
        let DevonError::InvalidArgument { context } = error else {
            panic!("expected InvalidArgument, got {error}");
        };
        assert!(context.contains("City"));

        let wrong_table_rows = GraphSnapshot::materialize(
            &table,
            &pager,
            &catalog,
            Direction::Out,
            "Person",
            wrong_rows.len(),
            wrong_rows,
        );

        let Err(error) = wrong_table_rows else {
            panic!("expected wrong destination rows to be rejected");
        };
        let DevonError::InvalidArgument { context } = error else {
            panic!("expected InvalidArgument, got {error}");
        };
        assert!(context.contains("City"));
    }
}
