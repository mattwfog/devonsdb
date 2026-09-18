//! Columnar row chunks: the executor's unit of vectorized data flow.
//!
//! Chunks use typed [`Column`] storage while preserving byte-identical
//! behavior across the row-to-column bridge described in `docs/SCALE.md` §6.4.

use devondb_types::{DevonError, DevonResult, logical_type::LogicalType, value::Value};

use crate::column::Column;

/// Maximum number of rows in one executor chunk.
pub const CHUNK_CAPACITY: usize = 2048;

/// A column-major batch of runtime values.
///
/// Each column has exactly [`Chunk::row_count`] values.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    types: Vec<LogicalType>,
    columns: Vec<Column>,
    row_count: usize,
}

impl Chunk {
    /// Returns the number of rows in the chunk.
    #[must_use]
    pub const fn row_count(&self) -> usize {
        self.row_count
    }

    /// Returns the logical column types in column order.
    #[must_use]
    pub fn types(&self) -> &[LogicalType] {
        &self.types
    }

    /// Returns the number of columns in the chunk.
    #[must_use]
    pub fn column_count(&self) -> usize {
        self.columns.len()
    }

    /// Returns column `i`, or `None` when it is out of bounds.
    #[must_use]
    pub fn column(&self, i: usize) -> Option<&Column> {
        self.columns.get(i)
    }

    /// Returns the value at `row` and `col`, or `None` when either is out of
    /// bounds. The value is materialized through [`Column::value_at`], so
    /// callers cross the typed-column boundary through owned values.
    #[must_use]
    pub fn value(&self, row: usize, col: usize) -> Option<Value> {
        let column = self.columns.get(col)?;
        if row >= column.len() {
            return None;
        }
        Some(column.value_at(row))
    }

    /// Builds a chunk directly from typed columns. This is the scan path's
    /// chunk constructor, so decoded columns never round-trip through
    /// `Vec<Value>`.
    ///
    /// Applies the validation [`ChunkBuilder::push_row`] applies to rows:
    /// column count must match the type list, every column must have the
    /// same row count, and each column must agree with its declared type.
    /// [`Column::Boxed`] values are checked one by one with the exact error
    /// text `push_row` raises; typed variants agree with their type by
    /// construction, so only the variant itself (and a Decimal column's
    /// scale) is validated. A chunk may not exceed [`CHUNK_CAPACITY`].
    pub fn from_columns(types: Vec<LogicalType>, columns: Vec<Column>) -> DevonResult<Self> {
        if types.len() != columns.len() {
            return Err(DevonError::InvalidArgument {
                context: format!(
                    "column arity mismatch: expected {} columns, actual {}",
                    types.len(),
                    columns.len()
                ),
            });
        }
        let row_count = columns.first().map_or(0, Column::len);
        for (index, (column, logical_type)) in columns.iter().zip(&types).enumerate() {
            if column.len() != row_count {
                return Err(DevonError::InvalidArgument {
                    context: format!(
                        "column {index} has {} rows, expected {row_count}",
                        column.len()
                    ),
                });
            }
            validate_column_storage(index, column, logical_type)?;
        }
        if row_count > CHUNK_CAPACITY {
            return Err(DevonError::InvalidArgument {
                context: format!(
                    "column row count {row_count} exceeds chunk capacity {CHUNK_CAPACITY} rows"
                ),
            });
        }
        Ok(Self {
            types,
            columns,
            row_count,
        })
    }

    /// Iterates over row-major materializations of the columnar values.
    pub fn rows(&self) -> impl Iterator<Item = Vec<Value>> {
        (0..self.row_count).map(|row| {
            self.columns
                .iter()
                .map(|column| column.value_at(row))
                .collect()
        })
    }
}

/// Type/variant agreement check for [`Chunk::from_columns`]. Typed variants
/// hold only values of their type by construction, so the variant itself
/// (plus the Decimal scale) is the whole check; [`Column::Boxed`] values are
/// validated per value with [`ChunkBuilder::push_row`]'s exact error text.
fn validate_column_storage(
    index: usize,
    column: &Column,
    logical_type: &LogicalType,
) -> DevonResult<()> {
    let agrees = match (column, logical_type) {
        (Column::Int64 { .. }, LogicalType::Int64)
        | (Column::Float64 { .. }, LogicalType::Float64)
        | (Column::Bool { .. }, LogicalType::Bool)
        | (Column::Timestamp { .. }, LogicalType::Timestamp) => true,
        (
            Column::Decimal { scale, .. },
            LogicalType::Decimal {
                scale: declared_scale,
                ..
            },
        ) => scale == declared_scale,
        (Column::Boxed(values), _) => {
            for value in values {
                if !value.matches_type(logical_type) {
                    return Err(DevonError::InvalidArgument {
                        context: format!(
                            "column {index} value {value} does not match expected type {logical_type}"
                        ),
                    });
                }
            }
            return Ok(());
        }
        _ => false,
    };
    if !agrees {
        return Err(DevonError::InvalidArgument {
            context: format!("column {index} storage does not match expected type {logical_type}"),
        });
    }
    Ok(())
}

/// Validates typed rows and builds a column-major [`Chunk`].
#[derive(Debug)]
pub struct ChunkBuilder {
    types: Vec<LogicalType>,
    columns: Vec<Vec<Value>>,
    row_count: usize,
}

impl ChunkBuilder {
    /// Creates an empty builder for columns with the supplied logical types.
    #[must_use]
    pub fn new(types: Vec<LogicalType>) -> Self {
        let columns = types
            .iter()
            .map(|_| Vec::with_capacity(CHUNK_CAPACITY))
            .collect();
        Self {
            types,
            columns,
            row_count: 0,
        }
    }

    /// Appends a row after validating its arity, values, and chunk capacity.
    pub fn push_row(&mut self, row: Vec<Value>) -> DevonResult<()> {
        if row.len() != self.types.len() {
            return Err(DevonError::InvalidArgument {
                context: format!(
                    "row arity mismatch: expected {} values, actual {}",
                    self.types.len(),
                    row.len()
                ),
            });
        }
        if self.is_full() {
            return Err(DevonError::InvalidArgument {
                context: format!("chunk is full at capacity {CHUNK_CAPACITY} rows"),
            });
        }
        for (index, (value, logical_type)) in row.iter().zip(&self.types).enumerate() {
            if !value.matches_type(logical_type) {
                return Err(DevonError::InvalidArgument {
                    context: format!(
                        "column {index} value {value} does not match expected type {logical_type}"
                    ),
                });
            }
        }
        for (column, value) in self.columns.iter_mut().zip(row) {
            column.push(value);
        }
        self.row_count += 1;
        Ok(())
    }

    /// Returns whether the builder contains exactly [`CHUNK_CAPACITY`] rows.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.row_count == CHUNK_CAPACITY
    }

    /// Finishes the builder and returns its chunk, converting each validated
    /// row column into typed [`Column`] storage. The builder accepts
    /// `Vec<Value>` rows, and `from_values` provides the bridge.
    #[must_use]
    pub fn finish(self) -> Chunk {
        let columns = self
            .types
            .iter()
            .zip(self.columns)
            .map(|(ty, values)| Column::from_values(ty, values))
            .collect();
        Chunk {
            types: self.types,
            columns,
            row_count: self.row_count,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CHUNK_CAPACITY, ChunkBuilder};
    use devondb_types::{DevonError, logical_type::LogicalType, value::Value};

    fn three_column_builder() -> ChunkBuilder {
        ChunkBuilder::new(vec![
            LogicalType::Int64,
            LogicalType::String,
            LogicalType::Bool,
        ])
    }

    #[test]
    fn row_and_column_accessors_agree() {
        let mut builder = three_column_builder();
        builder
            .push_row(vec![
                Value::Int64(1),
                Value::String("Ada".into()),
                Value::Bool(true),
            ])
            .unwrap();
        builder
            .push_row(vec![
                Value::Int64(2),
                Value::String("Grace".into()),
                Value::Bool(false),
            ])
            .unwrap();
        let chunk = builder.finish();

        assert_eq!(chunk.row_count(), 2);
        assert_eq!(chunk.column_count(), 3);
        assert_eq!(
            chunk
                .column(1)
                .map(|column| { vec![column.value_at(0), column.value_at(1),] }),
            Some(vec![
                Value::String("Ada".into()),
                Value::String("Grace".into())
            ])
        );
        assert_eq!(chunk.value(1, 2), Some(Value::Bool(false)));
        assert_eq!(chunk.value(2, 0), None);
        assert_eq!(chunk.value(0, 3), None);
        assert_eq!(chunk.column(3), None);

        let rows: Vec<Vec<Value>> = chunk.rows().collect();
        assert_eq!(rows.len(), chunk.row_count());
        for (row_index, row) in rows.iter().enumerate() {
            for (column_index, value) in row.iter().enumerate() {
                assert_eq!(chunk.value(row_index, column_index).as_ref(), Some(value));
            }
        }
    }

    #[test]
    fn arity_mismatch_names_expected_and_actual_counts() {
        let mut builder = three_column_builder();
        let error = builder
            .push_row(vec![Value::Int64(1), Value::String("Ada".into())])
            .unwrap_err();
        let DevonError::InvalidArgument { context } = error else {
            panic!("expected InvalidArgument, got {error}");
        };

        assert!(context.contains("expected 3"));
        assert!(context.contains("actual 2"));
    }

    #[test]
    fn type_mismatch_names_column_and_value() {
        let mut builder = three_column_builder();
        let error = builder
            .push_row(vec![Value::Int64(1), Value::Bool(true), Value::Bool(false)])
            .unwrap_err();
        let DevonError::InvalidArgument { context } = error else {
            panic!("expected InvalidArgument, got {error}");
        };

        assert!(context.contains("column 1"));
        assert!(context.contains("true"));
    }

    #[test]
    fn null_is_accepted_in_every_column() {
        let mut builder = three_column_builder();
        builder
            .push_row(vec![Value::Null, Value::Null, Value::Null])
            .unwrap();

        let chunk = builder.finish();
        assert_eq!(chunk.rows().next(), Some(vec![Value::Null; 3]));
    }

    #[test]
    fn capacity_is_enforced_at_exactly_chunk_capacity() {
        let mut builder = ChunkBuilder::new(vec![LogicalType::Int64]);
        for value in 0..CHUNK_CAPACITY {
            assert!(!builder.is_full());
            builder.push_row(vec![Value::Int64(value as i64)]).unwrap();
        }
        assert!(builder.is_full());

        let error = builder.push_row(vec![Value::Int64(2049)]).unwrap_err();
        assert!(matches!(error, DevonError::InvalidArgument { .. }));
        assert!(builder.is_full());
        assert_eq!(builder.finish().row_count(), CHUNK_CAPACITY);
    }

    #[test]
    fn empty_chunk_has_columns_but_no_rows() {
        let chunk = three_column_builder().finish();

        assert_eq!(chunk.row_count(), 0);
        assert_eq!(chunk.column_count(), 3);
        assert_eq!(chunk.column(0).map(|column| column.len()), Some(0));
        assert_eq!(chunk.value(0, 0), None);
        assert_eq!(chunk.rows().next(), None);
    }
}
