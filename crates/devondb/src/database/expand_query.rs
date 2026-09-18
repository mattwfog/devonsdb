//! Selected destination rows for ordinary Expand, indexed by physical offset.

use super::*;

type NodeRows = Vec<Option<Vec<Value>>>;

/// Required destination columns, including the primary key for MVCC identity.
pub(super) fn projection(
    root: &Operator,
    binding: &str,
    schema: &NodeTableSchema,
) -> DevonResult<Option<BTreeSet<usize>>> {
    let (key, _) = primary_key(schema)?;
    Ok(with_required_columns(
        referenced_columns(root, binding, schema),
        schema,
        [key],
    ))
}

/// Collects selected visible values in charged, hole-preserving physical slots.
pub(super) fn materialize<'a>(
    view: &'a ReadView,
    schema: &NodeTableSchema,
    selected: &BTreeSet<usize>,
    charges: &mut Vec<ChargedBytes<'a>>,
) -> DevonResult<(NodeRows, NodeTableSchema)> {
    let bytes = relationship_query::source_decode_peak(view, schema, Some(selected))?;
    let decode = charge_working_set(&view.shared.budget, &view.shared.pager, bytes, || {
        "Expand projected destination decode peak".into()
    })?;
    let count = visible_node_count(view, schema.name())?;
    let bytes = count
        .checked_mul(size_of::<Option<Vec<Value>>>())
        .ok_or_else(|| corrupt("Expand destination slot capacity overflow"))?;
    let mut retained = charge_working_set(&view.shared.budget, &view.shared.pager, bytes, || {
        "Expand projected destination slots".into()
    })?;
    let mut rows = vec![None; count];
    let mut source = ScanSource::new(view, schema.clone(), None, Some(selected.clone()))?;
    while let Some(row) = next_row(&mut source)? {
        retain_row(view, row, &mut rows, &mut retained)?;
    }
    drop(source);
    drop(decode);
    charges.push(retained);
    let columns = selected
        .iter()
        .map(|&index| schema.columns()[index].clone())
        .collect();
    Ok((
        rows,
        NodeTableSchema::new(schema.name().to_owned(), columns)?,
    ))
}

fn next_row(source: &mut ScanSource) -> DevonResult<Option<Vec<Value>>> {
    if let Some(row) = source.next_persisted_row()? {
        return Ok(Some(row));
    }
    source
        .overlay_rows
        .next()
        .map(|row| source.overlay_row(row))
        .transpose()
}

fn retain_row(
    view: &ReadView,
    mut row: Vec<Value>,
    rows: &mut [Option<Vec<Value>>],
    charge: &mut ChargedBytes<'_>,
) -> DevonResult<()> {
    let Some(Value::Int64(offset)) = row.pop() else {
        return Err(corrupt("Expand destination scan lost physical offset"));
    };
    let offset = usize::try_from(offset).map_err(|_| corrupt("negative destination offset"))?;
    let slot = rows
        .get_mut(offset)
        .ok_or_else(|| corrupt("destination offset exceeds slots"))?;
    if slot.is_some() {
        return Err(corrupt("destination scan repeated a physical offset"));
    }
    let bytes = row
        .capacity()
        .checked_mul(size_of::<Value>())
        .and_then(|n| n.checked_add(payload_bytes(&row).ok()?))
        .ok_or_else(|| corrupt("Expand selected row size overflow"))?;
    grow_working_set_charge(
        charge,
        &view.shared.budget,
        &view.shared.pager,
        bytes,
        || "Expand retained selected destination rows".into(),
    )?;
    *slot = Some(row);
    Ok(())
}

fn payload_bytes(row: &[Value]) -> DevonResult<usize> {
    row.iter().try_fold(0_usize, |bytes, value| {
        let payload = match value {
            Value::String(value) | Value::Json(value) => value.capacity(),
            Value::Bytes(value) => value.capacity(),
            Value::Vector(value) => value
                .capacity()
                .checked_mul(size_of::<f32>())
                .ok_or_else(|| corrupt("Expand vector capacity overflow"))?,
            _ => 0,
        };
        bytes
            .checked_add(payload)
            .ok_or_else(|| corrupt("Expand row payload overflow"))
    })
}

/// Reserves legacy output slots and clones of the selected destination payload.
pub(super) fn charge_output<'a>(
    view: &'a ReadView,
    width: usize,
    rows: &[Option<Vec<Value>>],
    charges: &mut Vec<ChargedBytes<'a>>,
) -> DevonResult<()> {
    let payload = rows.iter().flatten().try_fold(0, |maximum, row| {
        payload_bytes(row).map(|bytes| maximum.max(bytes))
    })?;
    // The legacy builder reserves a full chunk; one extra column covers
    // typed conversion overlap. Payload clones and the pending row coexist.
    let fixed = width
        .checked_add(1)
        .and_then(|n| n.checked_mul(size_of::<Value>() + 16))
        .ok_or_else(|| corrupt("Expand output column capacity overflow"))?;
    let bytes = payload
        .checked_mul(2)
        .and_then(|n| n.checked_add(fixed))
        .and_then(|n| n.checked_mul(CHUNK_CAPACITY + 2))
        .ok_or_else(|| corrupt("Expand output construction peak overflow"))?;
    charges.push(charge_working_set(
        &view.shared.budget,
        &view.shared.pager,
        bytes,
        || "Expand projected output construction peak".into(),
    )?);
    Ok(())
}
