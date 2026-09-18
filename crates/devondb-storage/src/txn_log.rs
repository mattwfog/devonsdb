//! WAL transactional framing: the v1 payload registry and the
//! transaction-grouping reader (`docs/MVCC.md` §2.1).

use devondb_types::{
    DevonError, DevonResult,
    schema::{NodeTableSchema, RelTableSchema},
    value::Value,
};
use serde::{Deserialize, Serialize};

/// The endpoint role governed by a relationship tombstone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RelEndpoint {
    /// The relationship's source endpoint.
    From,
    /// The relationship's destination endpoint.
    To,
}

/// A schema change carried by a [`WalPayload::Ddl`] record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DdlPayload {
    /// Creates a node table with the exact schema JSON used by the catalog.
    CreateNodeTable(NodeTableSchema),
    /// Creates a relationship table with the exact schema JSON used by the catalog.
    CreateRelTable(RelTableSchema),
}

/// A typed v1 WAL payload.
#[derive(Debug, Clone, PartialEq)]
pub enum WalPayload {
    /// Inserts one node row.
    NodeInsert {
        /// Target node-table name.
        table: String,
        /// Values in catalog declaration order.
        row: Vec<Value>,
    },
    /// Inserts one relationship edge using commit-time-resolved offsets.
    RelInsert {
        /// Target relationship-table name.
        rel: String,
        /// Source node offset.
        from: u64,
        /// Destination node offset.
        to: u64,
        /// Property values in catalog declaration order.
        values: Vec<Value>,
    },
    /// Replaces one node row addressed by the primary key it carries
    /// (`docs/UI.md` §12.2-§12.3). An indexed target additionally requires
    /// HNSW_MUTATION_WAL (bit 14); recovery validates this against its catalog.
    NodeUpdate {
        /// Target node-table name.
        table: String,
        /// Full replacement values in catalog declaration order.
        row: Vec<Value>,
    },
    /// Tombstones every relationship row matching one endpoint-role offset.
    RelDelete {
        /// Target relationship-table name.
        rel: String,
        /// Endpoint role to match.
        endpoint: RelEndpoint,
        /// Physical endpoint offset in the current checkpoint epoch.
        offset: u64,
    },
    /// Tombstones one node row by primary key (`docs/UI.md` §12.2-§12.3).
    /// Indexed targets require HNSW_MUTATION_WAL; no topology enters the WAL.
    NodeDelete {
        /// Target node-table name.
        table: String,
        /// The deleted row's primary-key value.
        key: Value,
    },
    /// Applies a catalog schema change.
    Ddl(DdlPayload),
    /// Commits the preceding payload records as one transaction.
    Commit {
        /// Number of preceding records in this transaction, excluding this record.
        records: u64,
    },
}

/// One committed payload record stamped with its transaction's commit LSN.
#[derive(Debug, Clone, PartialEq)]
pub struct CommittedRecord {
    /// Visibility timestamp, equal to the enclosing commit record's LSN.
    pub begin_lsn: u64,
    /// The non-commit payload made visible at `begin_lsn`.
    pub payload: WalPayload,
}

/// A transaction recovered from one contiguous WAL record group.
#[derive(Debug, Clone, PartialEq)]
pub struct CommittedGroup {
    /// LSN of the commit record that terminates this group.
    pub commit_lsn: u64,
    /// Payload records in canonical in-transaction order.
    pub records: Vec<CommittedRecord>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NodeInsertPayload {
    table: String,
    row: Vec<Value>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RelInsertPayload {
    rel: String,
    from: u64,
    to: u64,
    values: Vec<Value>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NodeUpdateEnvelope {
    update: NodeUpdatePayload,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NodeUpdatePayload {
    table: String,
    row: Vec<Value>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RelDeleteEnvelope {
    rel_delete: RelDeletePayload,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RelDeletePayload {
    rel: String,
    endpoint: RelEndpoint,
    offset: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NodeDeleteEnvelope {
    delete: NodeDeletePayload,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NodeDeletePayload {
    table: String,
    key: Value,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DdlEnvelope {
    ddl: DdlPayload,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommitEnvelope {
    commit: CommitPayload,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommitPayload {
    records: u64,
}

/// Encodes one typed payload as canonical compact JSON.
///
/// Non-finite floats are rejected because `serde_json` encodes them as JSON
/// `null`, which cannot deserialize back into [`Value`] and would make a
/// successfully written WAL record unrecoverable.
pub fn encode_payload(payload: &WalPayload) -> DevonResult<Vec<u8>> {
    match payload {
        WalPayload::NodeInsert { table, row } => {
            validate_finite_values(table, "node", row)?;
            encode_json(
                &NodeInsertPayload {
                    table: table.clone(),
                    row: row.clone(),
                },
                "node-insert WAL payload",
            )
        }
        WalPayload::RelInsert {
            rel,
            from,
            to,
            values,
        } => {
            validate_finite_values(rel, "relationship", values)?;
            encode_json(
                &RelInsertPayload {
                    rel: rel.clone(),
                    from: *from,
                    to: *to,
                    values: values.clone(),
                },
                "relationship-insert WAL payload",
            )
        }
        WalPayload::NodeUpdate { table, row } => {
            validate_finite_values(table, "node", row)?;
            encode_json(
                &NodeUpdateEnvelope {
                    update: NodeUpdatePayload {
                        table: table.clone(),
                        row: row.clone(),
                    },
                },
                "node-update WAL payload",
            )
        }
        WalPayload::RelDelete {
            rel,
            endpoint,
            offset,
        } => encode_json(
            &RelDeleteEnvelope {
                rel_delete: RelDeletePayload {
                    rel: rel.clone(),
                    endpoint: *endpoint,
                    offset: *offset,
                },
            },
            "relationship-delete WAL payload",
        ),
        WalPayload::NodeDelete { table, key } => {
            validate_finite_values(table, "node", std::slice::from_ref(key))?;
            encode_json(
                &NodeDeleteEnvelope {
                    delete: NodeDeletePayload {
                        table: table.clone(),
                        key: key.clone(),
                    },
                },
                "node-delete WAL payload",
            )
        }
        WalPayload::Ddl(ddl) => encode_json(&DdlEnvelope { ddl: ddl.clone() }, "DDL WAL payload"),
        WalPayload::Commit { records } => {
            if *records == 0 {
                return Err(invalid_argument(
                    "commit WAL payload must name at least one record",
                ));
            }
            encode_json(
                &CommitEnvelope {
                    commit: CommitPayload { records: *records },
                },
                "commit WAL payload",
            )
        }
    }
}

/// Decodes one v1 WAL payload after checking its unique top-level discriminator.
pub fn decode_payload(payload: &[u8]) -> DevonResult<WalPayload> {
    let value: serde_json::Value = serde_json::from_slice(payload)
        .map_err(|error| corrupt(format!("WAL payload is not valid JSON: {error}")))?;
    let object = value
        .as_object()
        .ok_or_else(|| corrupt("WAL payload must be a JSON object"))?;
    let discriminators: Vec<&str> = [
        "commit",
        "ddl",
        "table",
        "rel",
        "update",
        "rel_delete",
        "delete",
    ]
    .into_iter()
    .filter(|key| object.contains_key(*key))
    .collect();
    if discriminators.len() != 1 {
        return Err(corrupt(format!(
            "WAL payload must contain exactly one discriminator key (commit, ddl, table, rel, update, rel_delete, delete); found {discriminators:?}"
        )));
    }

    match discriminators[0] {
        "commit" => decode_commit(payload),
        "ddl" => decode_ddl(payload),
        "table" => decode_node_insert(payload),
        "rel" => decode_rel_insert(payload),
        "update" => decode_node_update(payload),
        "rel_delete" => decode_rel_delete(payload),
        "delete" => decode_node_delete(payload),
        _ => Err(corrupt("WAL payload discriminator is unknown")),
    }
}

/// Encodes a complete non-empty transaction, including its final commit payload.
///
/// Input records must already follow the canonical WAL order: DDL in statement
/// order, then node inserts ordered lexicographically by table, then relationship
/// inserts ordered lexicographically by table. A commit payload in `records` is
/// rejected because this function creates the only commit record itself.
pub fn encode_transaction(records: &[WalPayload]) -> DevonResult<Vec<Vec<u8>>> {
    if records.is_empty() {
        return Err(invalid_argument(
            "empty transactions do not write WAL records",
        ));
    }
    validate_record_order(records).map_err(invalid_argument)?;
    let record_count = u64::try_from(records.len())
        .map_err(|_| invalid_argument("transaction record count exceeds u64::MAX"))?;
    let mut encoded = Vec::with_capacity(records.len() + 1);
    for record in records {
        encoded.push(encode_payload(record)?);
    }
    encoded.push(encode_payload(&WalPayload::Commit {
        records: record_count,
    })?);
    Ok(encoded)
}

/// Groups an ordered iterator of `(lsn, payload)` pairs into committed transactions.
///
/// A trailing group without a commit record is discarded. Complete groups at or
/// below `checkpoint_lsn` are skipped entirely. Commit LSNs must strictly ascend
/// across complete groups.
pub fn group_transactions<I, P>(records: I, checkpoint_lsn: u64) -> DevonResult<Vec<CommittedGroup>>
where
    I: IntoIterator<Item = (u64, P)>,
    P: AsRef<[u8]>,
{
    let mut pending = Vec::new();
    let mut groups = Vec::new();
    let mut previous_commit_lsn = None;
    for (lsn, bytes) in records {
        let payload = decode_payload(bytes.as_ref()).map_err(|error| error_at_lsn(error, lsn))?;
        match payload {
            WalPayload::Commit { records } => {
                ensure_ascending_commit_lsn(previous_commit_lsn, lsn)?;
                finish_group(&mut groups, &mut pending, lsn, records, checkpoint_lsn)?;
                previous_commit_lsn = Some(lsn);
            }
            record => pending.push(record),
        }
    }
    Ok(groups)
}

fn finish_group(
    groups: &mut Vec<CommittedGroup>,
    pending: &mut Vec<WalPayload>,
    commit_lsn: u64,
    expected_records: u64,
    checkpoint_lsn: u64,
) -> DevonResult<()> {
    if commit_lsn <= checkpoint_lsn {
        pending.clear();
        return Ok(());
    }
    let actual_records = u64::try_from(pending.len()).map_err(|_| {
        corrupt(format!(
            "WAL group at LSN {commit_lsn} exceeds u64::MAX records"
        ))
    })?;
    if expected_records != actual_records {
        return Err(corrupt(format!(
            "WAL commit at LSN {commit_lsn} declares {expected_records} records but the group contains {actual_records}"
        )));
    }
    validate_record_order(pending).map_err(|context| {
        corrupt(format!(
            "WAL group committed at LSN {commit_lsn}: {context}"
        ))
    })?;
    let records = pending
        .drain(..)
        .map(|payload| CommittedRecord {
            begin_lsn: commit_lsn,
            payload,
        })
        .collect();
    groups.push(CommittedGroup {
        commit_lsn,
        records,
    });
    Ok(())
}

fn ensure_ascending_commit_lsn(previous: Option<u64>, current: u64) -> DevonResult<()> {
    if let Some(previous) = previous
        && current <= previous
    {
        return Err(corrupt(format!(
            "WAL commit LSNs must strictly ascend across groups: previous commit LSN {previous}, current commit LSN {current}"
        )));
    }
    Ok(())
}

fn validate_finite_values(table: &str, table_kind: &str, values: &[Value]) -> DevonResult<()> {
    for value in values {
        let value_kind = match value {
            Value::Float64(value) if !value.is_finite() => Some("Float64"),
            Value::Vector(vector) if vector.iter().any(|element| !element.is_finite()) => {
                Some("Vector")
            }
            _ => None,
        };
        if let Some(value_kind) = value_kind {
            return Err(invalid_argument(format!(
                "{table_kind} table `{table}` contains a non-finite {value_kind} value"
            )));
        }
    }
    Ok(())
}

fn decode_node_insert(payload: &[u8]) -> DevonResult<WalPayload> {
    let record: NodeInsertPayload = decode_json(payload, "node-insert WAL payload")?;
    Ok(WalPayload::NodeInsert {
        table: record.table,
        row: record.row,
    })
}

fn decode_rel_insert(payload: &[u8]) -> DevonResult<WalPayload> {
    let record: RelInsertPayload = decode_json(payload, "relationship-insert WAL payload")?;
    Ok(WalPayload::RelInsert {
        rel: record.rel,
        from: record.from,
        to: record.to,
        values: record.values,
    })
}

fn decode_node_update(payload: &[u8]) -> DevonResult<WalPayload> {
    let envelope: NodeUpdateEnvelope = decode_json(payload, "node-update WAL payload")?;
    Ok(WalPayload::NodeUpdate {
        table: envelope.update.table,
        row: envelope.update.row,
    })
}

fn decode_rel_delete(payload: &[u8]) -> DevonResult<WalPayload> {
    let envelope: RelDeleteEnvelope = decode_json(payload, "relationship-delete WAL payload")?;
    Ok(WalPayload::RelDelete {
        rel: envelope.rel_delete.rel,
        endpoint: envelope.rel_delete.endpoint,
        offset: envelope.rel_delete.offset,
    })
}

fn decode_node_delete(payload: &[u8]) -> DevonResult<WalPayload> {
    let envelope: NodeDeleteEnvelope = decode_json(payload, "node-delete WAL payload")?;
    Ok(WalPayload::NodeDelete {
        table: envelope.delete.table,
        key: envelope.delete.key,
    })
}

fn decode_ddl(payload: &[u8]) -> DevonResult<WalPayload> {
    let envelope: DdlEnvelope = decode_json(payload, "DDL WAL payload")?;
    validate_ddl_schema(&envelope.ddl)?;
    Ok(WalPayload::Ddl(envelope.ddl))
}

fn decode_commit(payload: &[u8]) -> DevonResult<WalPayload> {
    let envelope: CommitEnvelope = decode_json(payload, "commit WAL payload")?;
    if envelope.commit.records == 0 {
        return Err(corrupt("commit WAL payload must name at least one record"));
    }
    Ok(WalPayload::Commit {
        records: envelope.commit.records,
    })
}

fn validate_ddl_schema(ddl: &DdlPayload) -> DevonResult<()> {
    let result = match ddl {
        DdlPayload::CreateNodeTable(schema) => {
            NodeTableSchema::new(schema.name().to_owned(), schema.columns().to_vec()).map(|_| ())
        }
        DdlPayload::CreateRelTable(schema) => RelTableSchema::new(
            schema.name().to_owned(),
            schema.from().to_owned(),
            schema.to().to_owned(),
            schema.columns().to_vec(),
        )
        .map(|_| ()),
    };
    result.map_err(|error| corrupt(format!("DDL WAL payload has an invalid schema: {error}")))
}

fn validate_record_order(records: &[WalPayload]) -> Result<(), String> {
    let mut phase = 0_u8;
    let mut last_node_table: Option<&str> = None;
    let mut last_rel_table: Option<&str> = None;
    let mut last_rel_delete_table: Option<&str> = None;
    let mut rel_delete_claims = std::collections::BTreeSet::new();
    for record in records {
        match record {
            WalPayload::Ddl(_) => advance_phase(&mut phase, 0, "DDL")?,
            WalPayload::NodeInsert { table, .. } => {
                advance_phase(&mut phase, 1, "node insert")?;
                require_lexicographic_table(&mut last_node_table, table, "node insert")?;
            }
            WalPayload::RelInsert { rel, .. } => {
                advance_phase(&mut phase, 2, "relationship insert")?;
                require_lexicographic_table(&mut last_rel_table, rel, "relationship insert")?;
            }
            WalPayload::NodeUpdate { .. } => {
                advance_phase(&mut phase, 3, "node update")?;
            }
            WalPayload::RelDelete {
                rel,
                endpoint,
                offset,
            } => {
                advance_phase(&mut phase, 4, "relationship endpoint delete")?;
                require_lexicographic_table(
                    &mut last_rel_delete_table,
                    rel,
                    "relationship endpoint delete",
                )?;
                if !rel_delete_claims.insert((rel.as_str(), *endpoint, *offset)) {
                    return Err(format!(
                        "duplicate relationship endpoint delete claim for `{rel}` {endpoint:?} offset {offset}"
                    ));
                }
            }
            WalPayload::NodeDelete { .. } => {
                advance_phase(&mut phase, 5, "node delete")?;
            }
            WalPayload::Commit { .. } => {
                return Err("transaction records must not contain a commit payload".to_owned());
            }
        }
    }
    Ok(())
}

fn advance_phase(current: &mut u8, next: u8, kind: &str) -> Result<(), String> {
    if next < *current {
        return Err(format!(
            "{kind} appears after a later record kind; expected DDL, then node inserts, then relationship inserts, then node updates, then relationship endpoint deletes, then node deletes"
        ));
    }
    *current = next;
    Ok(())
}

fn require_lexicographic_table<'a>(
    previous: &mut Option<&'a str>,
    table: &'a str,
    kind: &str,
) -> Result<(), String> {
    if previous.is_some_and(|name| table < name) {
        return Err(format!(
            "{kind} table `{table}` is out of lexicographic order"
        ));
    }
    *previous = Some(table);
    Ok(())
}

fn encode_json<T: Serialize>(value: &T, kind: &str) -> DevonResult<Vec<u8>> {
    serde_json::to_vec(value)
        .map_err(|error| invalid_argument(format!("{kind} cannot be encoded: {error}")))
}

fn decode_json<T>(payload: &[u8], kind: &str) -> DevonResult<T>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_slice(payload)
        .map_err(|error| corrupt(format!("{kind} is malformed: {error}")))
}

fn error_at_lsn(error: DevonError, lsn: u64) -> DevonError {
    match error {
        DevonError::Corrupt { context } => corrupt(format!("WAL payload at LSN {lsn}: {context}")),
        other => other,
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
    use devondb_types::{
        DevonError,
        logical_type::LogicalType,
        schema::{Column, NodeTableSchema, RelTableSchema},
        value::Value,
    };

    use super::{
        DdlPayload, RelEndpoint, WalPayload, decode_payload, encode_payload, encode_transaction,
        group_transactions,
    };

    fn column(name: &str, ty: LogicalType, primary_key: bool) -> Column {
        Column {
            name: name.to_owned(),
            ty,
            primary_key,
        }
    }

    fn node_schema(name: &str) -> NodeTableSchema {
        NodeTableSchema::new(
            name.to_owned(),
            vec![column("id", LogicalType::Int64, true)],
        )
        .unwrap()
    }

    fn rel_schema(name: &str) -> RelTableSchema {
        RelTableSchema::new(
            name.to_owned(),
            "Person".to_owned(),
            "Person".to_owned(),
            vec![column("since", LogicalType::Int64, false)],
        )
        .unwrap()
    }

    fn node(table: &str, id: i64) -> WalPayload {
        WalPayload::NodeInsert {
            table: table.to_owned(),
            row: vec![Value::Int64(id)],
        }
    }

    fn rel(table: &str, from: u64, to: u64) -> WalPayload {
        WalPayload::RelInsert {
            rel: table.to_owned(),
            from,
            to,
            values: vec![Value::Int64(2026)],
        }
    }

    fn update(table: &str, id: i64, name: &str) -> WalPayload {
        WalPayload::NodeUpdate {
            table: table.to_owned(),
            row: vec![Value::Int64(id), Value::String(name.to_owned())],
        }
    }

    fn delete(table: &str, id: i64) -> WalPayload {
        WalPayload::NodeDelete {
            table: table.to_owned(),
            key: Value::Int64(id),
        }
    }

    fn rel_delete(table: &str, endpoint: RelEndpoint, offset: u64) -> WalPayload {
        WalPayload::RelDelete {
            rel: table.to_owned(),
            endpoint,
            offset,
        }
    }

    fn with_lsns(start: u64, payloads: Vec<Vec<u8>>) -> Vec<(u64, Vec<u8>)> {
        payloads
            .into_iter()
            .enumerate()
            .map(|(index, payload)| (start + index as u64, payload))
            .collect()
    }

    #[test]
    fn txn_log_node_insert_payload_round_trips_canonically() {
        let payload = WalPayload::NodeInsert {
            table: "Person".to_owned(),
            row: vec![Value::Int64(7), Value::String("Ada".to_owned())],
        };

        let encoded = encode_payload(&payload).unwrap();

        assert_eq!(
            String::from_utf8(encoded.clone()).unwrap(),
            r#"{"table":"Person","row":[{"Int64":7},{"String":"Ada"}]}"#
        );
        assert_eq!(decode_payload(&encoded).unwrap(), payload);
    }

    #[test]
    fn txn_log_rel_insert_payload_round_trips_canonically() {
        let payload = rel("Knows", 4, 9);

        let encoded = encode_payload(&payload).unwrap();

        assert_eq!(
            String::from_utf8(encoded.clone()).unwrap(),
            r#"{"rel":"Knows","from":4,"to":9,"values":[{"Int64":2026}]}"#
        );
        assert_eq!(decode_payload(&encoded).unwrap(), payload);
    }

    #[test]
    fn txn_log_node_update_payload_round_trips_canonically() {
        let payload = update("Person", 7, "Ada");

        let encoded = encode_payload(&payload).unwrap();

        assert_eq!(
            String::from_utf8(encoded.clone()).unwrap(),
            r#"{"update":{"table":"Person","row":[{"Int64":7},{"String":"Ada"}]}}"#
        );
        assert_eq!(decode_payload(&encoded).unwrap(), payload);
    }

    #[test]
    fn txn_log_node_delete_payload_round_trips_canonically() {
        let payload = delete("Person", 7);

        let encoded = encode_payload(&payload).unwrap();

        assert_eq!(
            String::from_utf8(encoded.clone()).unwrap(),
            r#"{"delete":{"table":"Person","key":{"Int64":7}}}"#
        );
        assert_eq!(decode_payload(&encoded).unwrap(), payload);
    }

    #[test]
    fn txn_log_rel_delete_payload_round_trips_canonically() {
        let payload = rel_delete("Knows", RelEndpoint::From, 7);

        let encoded = encode_payload(&payload).unwrap();

        assert_eq!(
            String::from_utf8(encoded.clone()).unwrap(),
            r#"{"rel_delete":{"rel":"Knows","endpoint":"from","offset":7}}"#
        );
        assert_eq!(decode_payload(&encoded).unwrap(), payload);
    }

    #[test]
    fn txn_log_rel_delete_rejects_unknown_role_and_fields() {
        for payload in [
            br#"{"rel_delete":{"rel":"Knows","endpoint":"side","offset":7}}"#.as_slice(),
            br#"{"rel_delete":{"rel":"Knows","endpoint":"from","offset":7,"extra":true}}"#
                .as_slice(),
        ] {
            assert!(matches!(
                decode_payload(payload),
                Err(DevonError::Corrupt { .. })
            ));
        }
    }

    #[test]
    fn txn_log_node_float_and_vector_payloads_round_trip() {
        let payload = WalPayload::NodeInsert {
            table: "Reading".to_owned(),
            row: vec![Value::Float64(1.25), Value::Vector(vec![0.5, -2.5, 3.0])],
        };

        let encoded = encode_payload(&payload).unwrap();

        assert_eq!(decode_payload(&encoded).unwrap(), payload);
    }

    #[test]
    fn txn_log_relationship_float_and_vector_payloads_round_trip() {
        let payload = WalPayload::RelInsert {
            rel: "Measured".to_owned(),
            from: 4,
            to: 9,
            values: vec![Value::Float64(-8.75), Value::Vector(vec![1.0, 2.0])],
        };

        let encoded = encode_payload(&payload).unwrap();

        assert_eq!(decode_payload(&encoded).unwrap(), payload);
    }

    #[test]
    fn txn_log_rejects_nonfinite_float64_values_in_node_rows() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let payload = WalPayload::NodeInsert {
                table: "Reading".to_owned(),
                row: vec![Value::Float64(value)],
            };

            assert_nonfinite_error(&payload, "Reading", "Float64");
        }
    }

    #[test]
    fn txn_log_rejects_nonfinite_float64_values_in_relationship_properties() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let payload = WalPayload::RelInsert {
                rel: "Measured".to_owned(),
                from: 4,
                to: 9,
                values: vec![Value::Float64(value)],
            };

            assert_nonfinite_error(&payload, "Measured", "Float64");
        }
    }

    #[test]
    fn txn_log_rejects_nonfinite_vector_elements_in_all_row_payloads() {
        for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let node_payload = WalPayload::NodeInsert {
                table: "Reading".to_owned(),
                row: vec![Value::Vector(vec![0.0, value])],
            };
            let rel_payload = WalPayload::RelInsert {
                rel: "Measured".to_owned(),
                from: 4,
                to: 9,
                values: vec![Value::Vector(vec![value, 1.0])],
            };

            assert_nonfinite_error(&node_payload, "Reading", "Vector");
            assert_nonfinite_error(&rel_payload, "Measured", "Vector");
        }
    }

    #[test]
    fn txn_log_create_node_table_payload_round_trips_exact_catalog_schema() {
        let payload = WalPayload::Ddl(DdlPayload::CreateNodeTable(node_schema("Person")));

        let encoded = encode_payload(&payload).unwrap();

        assert_eq!(
            String::from_utf8(encoded.clone()).unwrap(),
            r#"{"ddl":{"create_node_table":{"name":"Person","columns":[{"name":"id","ty":"Int64","primary_key":true}]}}}"#
        );
        assert_eq!(decode_payload(&encoded).unwrap(), payload);
    }

    #[test]
    fn txn_log_create_rel_table_payload_round_trips_exact_catalog_schema() {
        let payload = WalPayload::Ddl(DdlPayload::CreateRelTable(rel_schema("Knows")));

        let encoded = encode_payload(&payload).unwrap();

        assert_eq!(
            String::from_utf8(encoded.clone()).unwrap(),
            r#"{"ddl":{"create_rel_table":{"name":"Knows","from":"Person","to":"Person","columns":[{"name":"since","ty":"Int64","primary_key":false}]}}}"#
        );
        assert_eq!(decode_payload(&encoded).unwrap(), payload);
    }

    #[test]
    fn txn_log_commit_payload_round_trips_canonically() {
        let payload = WalPayload::Commit { records: 3 };

        let encoded = encode_payload(&payload).unwrap();

        assert_eq!(
            String::from_utf8(encoded.clone()).unwrap(),
            r#"{"commit":{"records":3}}"#
        );
        assert_eq!(decode_payload(&encoded).unwrap(), payload);
    }

    #[test]
    fn txn_log_discriminator_must_be_mutually_exclusive() {
        let both = br#"{"table":"Person","row":[],"commit":{"records":1}}"#;
        let neither = br#"{"unknown":true}"#;

        assert!(matches!(
            decode_payload(both),
            Err(DevonError::Corrupt { .. })
        ));
        assert!(matches!(
            decode_payload(neither),
            Err(DevonError::Corrupt { .. })
        ));
    }

    #[test]
    fn txn_log_rule_1_records_are_contiguous_and_commit_is_last() {
        let records = vec![node("Person", 1), node("Person", 2)];

        let encoded = encode_transaction(&records).unwrap();

        assert_eq!(encoded.len(), 3);
        assert!(matches!(
            decode_payload(&encoded[2]).unwrap(),
            WalPayload::Commit { records: 2 }
        ));
        let groups = group_transactions(with_lsns(10, encoded), 0).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].records.len(), 2);
    }

    #[test]
    fn txn_log_rule_2_count_excludes_commit_and_empty_transaction_is_rejected() {
        let encoded = encode_transaction(&[node("Person", 1), node("Person", 2)]).unwrap();

        assert!(matches!(
            decode_payload(encoded.last().unwrap()).unwrap(),
            WalPayload::Commit { records: 2 }
        ));
        assert!(matches!(
            encode_transaction(&[]),
            Err(DevonError::InvalidArgument { .. })
        ));
        assert!(matches!(
            encode_payload(&WalPayload::Commit { records: 0 }),
            Err(DevonError::InvalidArgument { .. })
        ));
    }

    #[test]
    fn txn_log_rule_3_encode_enforces_canonical_record_order() {
        let canonical = vec![
            WalPayload::Ddl(DdlPayload::CreateNodeTable(node_schema("Person"))),
            node("A", 1),
            node("A", 2),
            node("B", 3),
            rel("AEdge", 0, 1),
            rel("BEdge", 1, 2),
            update("A", 1, "updated"),
            rel_delete("AEdge", RelEndpoint::From, 0),
            rel_delete("AEdge", RelEndpoint::To, 0),
            delete("B", 3),
        ];
        assert!(encode_transaction(&canonical).is_ok());

        let wrong_kind_order = vec![node("A", 1), canonical[0].clone()];
        let wrong_node_order = vec![node("B", 1), node("A", 2)];
        let wrong_rel_order = vec![rel("BEdge", 0, 1), rel("AEdge", 1, 2)];
        let wrong_rel_delete_order = vec![
            rel_delete("BEdge", RelEndpoint::From, 0),
            rel_delete("AEdge", RelEndpoint::From, 0),
        ];
        let duplicate_rel_delete = vec![
            rel_delete("AEdge", RelEndpoint::From, 0),
            rel_delete("AEdge", RelEndpoint::From, 0),
        ];
        for records in [
            wrong_kind_order,
            wrong_node_order,
            wrong_rel_order,
            wrong_rel_delete_order,
            duplicate_rel_delete,
        ] {
            assert!(matches!(
                encode_transaction(&records),
                Err(DevonError::InvalidArgument { .. })
            ));
        }
    }

    #[test]
    fn txn_log_rule_4_commit_lsn_stamps_every_group_entry() {
        let encoded = encode_transaction(&[node("Person", 1), node("Person", 2)]).unwrap();

        let groups = group_transactions(with_lsns(40, encoded), 0).unwrap();

        assert_eq!(groups[0].commit_lsn, 42);
        assert!(
            groups[0]
                .records
                .iter()
                .all(|record| record.begin_lsn == 42)
        );
    }

    #[test]
    fn txn_log_rule_4_trailing_unterminated_group_is_silently_discarded() {
        let mut records = with_lsns(50, encode_transaction(&[node("A", 1)]).unwrap());
        records.push((52, encode_payload(&node("B", 2)).unwrap()));

        let groups = group_transactions(records, 0).unwrap();

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].commit_lsn, 51);
    }

    #[test]
    fn txn_log_rule_5_count_mismatch_is_corrupt_and_names_commit_lsn() {
        let records = vec![
            (70, encode_payload(&node("Person", 1)).unwrap()),
            (
                71,
                encode_payload(&WalPayload::Commit { records: 2 }).unwrap(),
            ),
        ];

        let error = group_transactions(records, 0).unwrap_err();

        match error {
            DevonError::Corrupt { context } => assert!(context.contains("71")),
            other => panic!("expected corrupt WAL error, got {other}"),
        }
    }

    #[test]
    fn txn_log_rule_6_checkpointed_groups_are_skipped_at_transaction_granularity() {
        let first = encode_transaction(&[node("A", 1)]).unwrap();
        let second = encode_transaction(&[node("B", 2)]).unwrap();
        let mut records = with_lsns(80, first);
        records.extend(with_lsns(82, second));

        let groups = group_transactions(records, 81).unwrap();

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].commit_lsn, 83);
        assert_eq!(groups[0].records[0].payload, node("B", 2));
    }

    #[test]
    fn txn_log_rule_6_checkpointed_count_mismatch_is_skipped() {
        let records = vec![
            (70, encode_payload(&node("Person", 1)).unwrap()),
            (
                71,
                encode_payload(&WalPayload::Commit { records: 2 }).unwrap(),
            ),
        ];

        let groups = group_transactions(records, 71).unwrap();

        assert!(groups.is_empty());
    }

    #[test]
    fn txn_log_commit_lsns_must_strictly_ascend_across_groups() {
        let records = vec![
            (100, encode_payload(&node("A", 1)).unwrap()),
            (
                101,
                encode_payload(&WalPayload::Commit { records: 1 }).unwrap(),
            ),
            (102, encode_payload(&node("B", 2)).unwrap()),
            (
                99,
                encode_payload(&WalPayload::Commit { records: 1 }).unwrap(),
            ),
        ];

        let error = group_transactions(records, 0).unwrap_err();

        match error {
            DevonError::Corrupt { context } => {
                assert!(context.contains("101"));
                assert!(context.contains("99"));
            }
            other => panic!("expected corrupt WAL error, got {other}"),
        }
    }

    #[test]
    fn txn_log_unknown_payload_is_corrupt_and_names_its_lsn() {
        let error =
            group_transactions(vec![(99, br#"{"mystery":true}"#.as_slice())], 0).unwrap_err();

        match error {
            DevonError::Corrupt { context } => assert!(context.contains("99")),
            other => panic!("expected corrupt WAL error, got {other}"),
        }
    }

    fn assert_nonfinite_error(payload: &WalPayload, table: &str, value_kind: &str) {
        let error = encode_payload(payload).unwrap_err();
        match error {
            DevonError::InvalidArgument { context } => {
                assert!(context.contains(table));
                assert!(context.contains(value_kind));
            }
            other => panic!("expected invalid argument, got {other}"),
        }
    }
}
