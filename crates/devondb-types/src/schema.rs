//! Schema model: node tables, rel tables, columns.

use std::{borrow::Cow, collections::HashSet};

use serde::{Deserialize, Serialize};

use crate::{DevonError, DevonResult, decimal::MAX_PRECISION, logical_type::LogicalType};

/// Returns the canonical lookup spelling of an identifier.
///
/// Only ASCII uppercase letters are folded to lowercase. Non-ASCII bytes are
/// preserved exactly, and an already-folded name is returned without an
/// allocation.
#[must_use]
pub fn fold(name: &str) -> Cow<'_, str> {
    if name.bytes().any(|byte| byte.is_ascii_uppercase()) {
        Cow::Owned(name.to_ascii_lowercase())
    } else {
        Cow::Borrowed(name)
    }
}

/// Returns the display spelling of the candidate nearest to `input`, when
/// that nearest candidate is close enough to be a plausible typo.
///
/// Distance is Levenshtein over the ASCII-folded spellings ([`fold`]), so a
/// wrong-case reference never counts as an edit; distances are measured over
/// UTF-8 bytes. The edit budget scales with the folded input's length
/// (1 edit up to 4 bytes, 2 up to 8, 3 beyond). A fold-equal candidate
/// (distance 0) is never suggested — a failed lookup that folds equal is a
/// different failure than a typo. Ties go to the earliest candidate, so
/// callers supply candidates in catalog/declaration order to keep
/// suggestions deterministic.
#[must_use]
pub fn did_you_mean<'a, I>(input: &str, candidates: I) -> Option<String>
where
    I: IntoIterator<Item = &'a str>,
{
    let folded_input = fold(input);
    let budget = suggestion_budget(folded_input.len());
    let mut best: Option<(usize, &str)> = None;
    for candidate in candidates {
        let distance = levenshtein(&folded_input, &fold(candidate));
        let closer = best.is_none_or(|(best_distance, _)| distance < best_distance);
        if distance != 0 && distance <= budget && closer {
            best = Some((distance, candidate));
        }
    }
    best.map(|(_, name)| name.to_owned())
}

/// Formats the standard did-you-mean suffix for an error message.
///
/// Returns `` ` (did you mean `X`?)` `` when [`did_you_mean`] finds a
/// suggestion and an empty string otherwise, so call sites can append it
/// unconditionally. This is the one spelling of the suggestion every
/// devondb error message uses.
#[must_use]
pub fn suggestion_suffix<'a, I>(input: &str, candidates: I) -> String
where
    I: IntoIterator<Item = &'a str>,
{
    match did_you_mean(input, candidates) {
        Some(name) => format!(" (did you mean `{name}`?)"),
        None => String::new(),
    }
}

fn suggestion_budget(folded_input_len: usize) -> usize {
    match folded_input_len {
        0..=4 => 1,
        5..=8 => 2,
        _ => 3,
    }
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a = a.as_bytes();
    let b = b.as_bytes();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0; b.len() + 1];
    for (row, a_byte) in a.iter().enumerate() {
        current[0] = row + 1;
        for (column, b_byte) in b.iter().enumerate() {
            let substitution = previous[column] + usize::from(a_byte != b_byte);
            current[column + 1] = substitution
                .min(previous[column + 1] + 1)
                .min(current[column] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()]
}

/// A named, typed property in a node or relationship table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Column {
    /// The column name.
    pub name: String,
    /// The logical type of values stored in the column.
    pub ty: LogicalType,
    /// Whether this column is the table's primary key.
    ///
    /// Node tables require exactly one primary key. Relationship-table
    /// columns cannot be primary keys.
    pub primary_key: bool,
}

/// The validated schema of a node table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawNodeTableSchema")]
pub struct NodeTableSchema {
    name: String,
    columns: Vec<Column>,
}

#[derive(Deserialize)]
struct RawNodeTableSchema {
    name: String,
    columns: Vec<Column>,
}

impl TryFrom<RawNodeTableSchema> for NodeTableSchema {
    type Error = DevonError;

    fn try_from(raw: RawNodeTableSchema) -> Result<Self, Self::Error> {
        Self::new(raw.name, raw.columns)
    }
}

impl NodeTableSchema {
    /// Creates a node-table schema after validating its name, columns, and key.
    pub fn new(name: String, columns: Vec<Column>) -> DevonResult<Self> {
        validate_table_name("node", &name)?;
        validate_columns("node", &name, &columns)?;
        validate_node_primary_key(&name, &columns)?;

        Ok(Self { name, columns })
    }

    /// Returns the table name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the table's columns in declaration order.
    #[must_use]
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    /// Returns the declaration-order index of an ASCII-folded matching column name.
    #[must_use]
    pub fn column_index(&self, name: &str) -> Option<usize> {
        let folded = fold(name);
        self.columns
            .iter()
            .position(|column| fold(&column.name) == folded)
    }
}

/// The validated schema of a relationship table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawRelTableSchema")]
pub struct RelTableSchema {
    name: String,
    from: String,
    to: String,
    columns: Vec<Column>,
}

#[derive(Deserialize)]
struct RawRelTableSchema {
    name: String,
    from: String,
    to: String,
    columns: Vec<Column>,
}

impl TryFrom<RawRelTableSchema> for RelTableSchema {
    type Error = DevonError;

    fn try_from(raw: RawRelTableSchema) -> Result<Self, Self::Error> {
        Self::new(raw.name, raw.from, raw.to, raw.columns)
    }
}

impl RelTableSchema {
    /// Creates a relationship-table schema after validating its names and columns.
    pub fn new(name: String, from: String, to: String, columns: Vec<Column>) -> DevonResult<Self> {
        validate_table_name("relationship", &name)?;
        validate_endpoint_name(&name, "from", &from)?;
        validate_endpoint_name(&name, "to", &to)?;
        validate_columns("relationship", &name, &columns)?;
        validate_rel_primary_keys(&name, &columns)?;

        Ok(Self {
            name,
            from,
            to,
            columns,
        })
    }

    /// Returns the relationship-table name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the name of the source node table.
    #[must_use]
    pub fn from(&self) -> &str {
        &self.from
    }

    /// Returns the name of the destination node table.
    #[must_use]
    pub fn to(&self) -> &str {
        &self.to
    }

    /// Returns the table's columns in declaration order.
    #[must_use]
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    /// Returns the declaration-order index of an ASCII-folded matching column name.
    #[must_use]
    pub fn column_index(&self, name: &str) -> Option<usize> {
        let folded = fold(name);
        self.columns
            .iter()
            .position(|column| fold(&column.name) == folded)
    }
}

fn validate_table_name(kind: &str, name: &str) -> DevonResult<()> {
    if name.is_empty() {
        return Err(invalid_argument(format!(
            "{kind} table has an empty table name"
        )));
    }
    Ok(())
}

fn validate_endpoint_name(rel_name: &str, endpoint: &str, name: &str) -> DevonResult<()> {
    if name.is_empty() {
        return Err(invalid_argument(format!(
            "relationship table `{rel_name}` has an empty {endpoint} node-table name"
        )));
    }
    Ok(())
}

fn validate_columns(kind: &str, table_name: &str, columns: &[Column]) -> DevonResult<()> {
    let mut names = HashSet::with_capacity(columns.len());
    for (index, column) in columns.iter().enumerate() {
        if column.name.is_empty() {
            return Err(invalid_argument(format!(
                "column at index {index} in {kind} table `{table_name}` has an empty name"
            )));
        }
        if !names.insert(fold(&column.name)) {
            return Err(invalid_argument(format!(
                "column `{}` is duplicated in {kind} table `{table_name}`",
                column.name
            )));
        }
        validate_decimal_column(kind, table_name, column)?;
        // GeoPoint columns are live for NODE tables (geo codec + feature
        // bit 4, docs/GEO.md §5). Relationship properties stay rejected:
        // the CSR property codec has no geo layout.
        if kind == "relationship" && matches!(column.ty, LogicalType::GeoPoint) {
            return Err(invalid_argument(format!(
                "column `{}` in {kind} table `{table_name}`: GeoPoint \
                 relationship properties are not supported",
                column.name
            )));
        }
        // Scalar-v2 columns are live for NODE tables. Relationship
        // properties remain rejected because the CSR property codec has no
        // scalar-v2 layout, matching the GeoPoint property restriction.
        if kind == "relationship"
            && matches!(
                column.ty,
                LogicalType::Timestamp
                    | LogicalType::Bytes
                    | LogicalType::Decimal { .. }
                    | LogicalType::Json
            )
        {
            return Err(invalid_argument(format!(
                "column `{}` in {kind} table `{table_name}`: {} \
                 relationship properties are not supported",
                column.name, column.ty
            )));
        }
    }
    Ok(())
}

fn validate_decimal_column(kind: &str, table_name: &str, column: &Column) -> DevonResult<()> {
    let LogicalType::Decimal { precision, scale } = column.ty else {
        return Ok(());
    };
    if !(1..=MAX_PRECISION).contains(&precision) {
        return Err(invalid_argument(format!(
            "column `{}` in {kind} table `{table_name}` has Decimal precision {precision}; \
             precision must be between 1 and {MAX_PRECISION}",
            column.name
        )));
    }
    if scale > precision {
        return Err(invalid_argument(format!(
            "column `{}` in {kind} table `{table_name}` has Decimal scale {scale}; \
             scale must not exceed precision {precision}",
            column.name
        )));
    }
    Ok(())
}

fn validate_node_primary_key(table_name: &str, columns: &[Column]) -> DevonResult<()> {
    let primary_keys: Vec<&Column> = columns.iter().filter(|column| column.primary_key).collect();
    if primary_keys.len() != 1 {
        let names: Vec<&str> = primary_keys
            .iter()
            .map(|column| column.name.as_str())
            .collect();
        return Err(invalid_argument(format!(
            "node table `{table_name}` must have exactly one primary key column; found {names:?}"
        )));
    }

    let primary_key = primary_keys[0];
    if !matches!(primary_key.ty, LogicalType::Int64 | LogicalType::String) {
        return Err(invalid_argument(format!(
            "primary key column `{}` in node table `{table_name}` has type {}; v0 requires Int64 or String",
            primary_key.name, primary_key.ty
        )));
    }
    Ok(())
}

fn validate_rel_primary_keys(table_name: &str, columns: &[Column]) -> DevonResult<()> {
    if let Some(column) = columns.iter().find(|column| column.primary_key) {
        return Err(invalid_argument(format!(
            "column `{}` in relationship table `{table_name}` cannot be a primary key",
            column.name
        )));
    }
    Ok(())
}

fn invalid_argument(context: String) -> DevonError {
    DevonError::InvalidArgument { context }
}

#[cfg(test)]
mod tests {
    use super::{Column, NodeTableSchema, RelTableSchema, fold};

    /// GeoPoint columns are live for node tables and rejected for
    /// relationship properties (`docs/GEO.md` §5).
    #[test]
    fn geo_point_columns_node_only() {
        let node = NodeTableSchema::new(
            "Place".to_owned(),
            vec![
                column("id", LogicalType::Int64, true),
                column("loc", LogicalType::GeoPoint, false),
            ],
        )
        .expect("GeoPoint node columns are supported");
        assert_eq!(node.columns()[1].ty, LogicalType::GeoPoint);

        let rel = RelTableSchema::new(
            "Visited".to_owned(),
            "Person".to_owned(),
            "Person".to_owned(),
            vec![column("where", LogicalType::GeoPoint, false)],
        )
        .expect_err("GeoPoint rel properties must be rejected");
        assert!(
            rel.to_string()
                .contains("GeoPoint relationship properties are not supported"),
            "{rel}"
        );
    }

    #[test]
    fn scalar_v2_columns_are_node_only() {
        let scalar_types = [
            LogicalType::Timestamp,
            LogicalType::Bytes,
            LogicalType::Decimal {
                precision: 38,
                scale: 0,
            },
            LogicalType::Json,
        ];
        for ty in scalar_types {
            let node = NodeTableSchema::new(
                "Scalar".to_owned(),
                vec![
                    column("id", LogicalType::Int64, true),
                    column("value", ty, false),
                ],
            )
            .expect("scalar-v2 node columns are supported");
            assert_eq!(node.columns()[1].ty, ty);

            let error = RelTableSchema::new(
                "Carries".to_owned(),
                "Scalar".to_owned(),
                "Scalar".to_owned(),
                vec![column("value", ty, false)],
            )
            .expect_err("scalar-v2 relationship properties must be rejected");
            assert!(
                error
                    .to_string()
                    .contains("relationship properties are not supported"),
                "{error}"
            );
        }
    }

    #[test]
    fn decimal_declarations_validate_precision_and_scale() {
        for (name, precision, scale, expected) in [
            ("zero_precision", 0, 0, "zero_precision"),
            ("high_precision", 39, 0, "high_precision"),
            ("high_scale", 4, 5, "high_scale"),
        ] {
            let result = NodeTableSchema::new(
                "Amounts".to_owned(),
                vec![
                    column("id", LogicalType::Int64, true),
                    column(name, LogicalType::Decimal { precision, scale }, false),
                ],
            );
            assert_invalid_argument_mentions(result, expected);
        }

        NodeTableSchema::new(
            "Amounts".to_owned(),
            vec![
                column("id", LogicalType::Int64, true),
                column(
                    "amount",
                    LogicalType::Decimal {
                        precision: 38,
                        scale: 38,
                    },
                    false,
                ),
            ],
        )
        .expect("Decimal(38, 38) is valid");
    }
    use crate::{DevonError, logical_type::LogicalType};

    fn column(name: &str, ty: LogicalType, primary_key: bool) -> Column {
        Column {
            name: name.to_owned(),
            ty,
            primary_key,
        }
    }

    #[test]
    fn valid_node_table_constructs() {
        let columns = vec![
            column("id", LogicalType::Int64, true),
            column("name", LogicalType::String, false),
        ];

        let schema = NodeTableSchema::new("Person".to_owned(), columns.clone()).unwrap();

        assert_eq!(schema.name(), "Person");
        assert_eq!(schema.columns(), columns);
    }

    #[test]
    fn fold_equal_column_name_is_rejected() {
        let result = NodeTableSchema::new(
            "Person".to_owned(),
            vec![
                column("Name", LogicalType::Int64, true),
                column("name", LogicalType::String, false),
            ],
        );

        assert_invalid_argument_mentions(result, "name");
    }

    #[test]
    fn node_table_without_primary_key_is_rejected() {
        let result = NodeTableSchema::new(
            "Person".to_owned(),
            vec![column("name", LogicalType::String, false)],
        );

        assert_invalid_argument_mentions(result, "Person");
    }

    #[test]
    fn node_table_with_two_primary_keys_is_rejected() {
        let result = NodeTableSchema::new(
            "Person".to_owned(),
            vec![
                column("id", LogicalType::Int64, true),
                column("external_id", LogicalType::String, true),
            ],
        );

        assert_invalid_argument_mentions(result, "external_id");
    }

    #[test]
    fn float_primary_key_is_rejected() {
        let result = NodeTableSchema::new(
            "Measurement".to_owned(),
            vec![column("reading", LogicalType::Float64, true)],
        );

        assert_invalid_argument_mentions(result, "reading");
    }

    #[test]
    fn relationship_table_with_primary_key_is_rejected() {
        let result = RelTableSchema::new(
            "Knows".to_owned(),
            "Person".to_owned(),
            "Person".to_owned(),
            vec![column("since", LogicalType::Int64, true)],
        );

        assert_invalid_argument_mentions(result, "since");
    }

    #[test]
    fn column_index_is_ascii_case_insensitive() {
        let schema = NodeTableSchema::new(
            "Person".to_owned(),
            vec![
                column("id", LogicalType::Int64, true),
                column("displayName", LogicalType::String, false),
            ],
        )
        .unwrap();

        assert_eq!(schema.column_index("displayName"), Some(1));
        assert_eq!(schema.column_index("missing"), None);
        assert_eq!(schema.column_index("DisplayName"), Some(1));
        assert_eq!(schema.column_index("DISPLAYNAME"), Some(1));
    }

    #[test]
    fn did_you_mean_suggests_the_nearest_candidate_case_insensitively() {
        let candidates = ["Person", "City"];
        assert_eq!(
            super::did_you_mean("Persn", candidates),
            Some("Person".to_owned())
        );
        assert_eq!(
            super::did_you_mean("persn", candidates),
            Some("Person".to_owned()),
            "the display spelling is returned, matching is folded"
        );
    }

    #[test]
    fn did_you_mean_respects_the_edit_budget_and_skips_fold_equal_names() {
        assert_eq!(
            super::did_you_mean("xy", ["ab"]),
            None,
            "2 edits on a 2-byte name exceeds the 1-edit budget"
        );
        assert_eq!(
            super::did_you_mean("PERSON", ["Person"]),
            None,
            "a fold-equal candidate is not a typo and is never suggested"
        );
        assert_eq!(super::did_you_mean("anything", []), None);
    }

    #[test]
    fn did_you_mean_breaks_ties_toward_the_earliest_candidate() {
        assert_eq!(
            super::did_you_mean("ac", ["ab", "ad"]),
            Some("ab".to_owned())
        );
    }

    #[test]
    fn suggestion_suffix_formats_the_one_canonical_spelling() {
        assert_eq!(
            super::suggestion_suffix("Persn", ["Person"]),
            " (did you mean `Person`?)"
        );
        assert_eq!(super::suggestion_suffix("zzz", ["Person"]), "");
    }

    #[test]
    fn fold_preserves_non_ascii_and_borrows_when_already_canonical() {
        assert_eq!(fold("Already_lower"), "already_lower");
        assert_eq!(fold("Å"), "Å");
        assert_ne!(fold("Å"), fold("å"));
        assert!(matches!(
            fold("already_lower"),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    fn assert_invalid_argument_mentions<T: std::fmt::Debug>(
        result: Result<T, DevonError>,
        expected: &str,
    ) {
        let error = result.unwrap_err();
        let DevonError::InvalidArgument { context } = error else {
            panic!("expected InvalidArgument, got {error}");
        };
        assert!(
            context.contains(expected),
            "expected `{context}` to mention `{expected}`"
        );
    }
}
