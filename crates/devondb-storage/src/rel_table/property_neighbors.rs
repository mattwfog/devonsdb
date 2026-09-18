//! Property-preserving directional adjacency over existing CSR and overlays.

use super::*;

impl RelTable {
    /// Returns incident edges in directional insertion order, keeping properties
    /// paired with each occurrence, including parallel edges.
    pub fn neighbors_with_properties(
        &self,
        pager: &Pager,
        catalog: &Catalog,
        direction: Direction,
        from: u64,
    ) -> DevonResult<Vec<ScannedRelationship>> {
        ensure_catalog_schema(&self.schema, catalog)?;
        validate_direction(&self.schema, direction)?;
        let from_layout = endpoint_layout(pager, catalog, self.schema.from())?;
        let to_layout = endpoint_layout(pager, catalog, self.schema.to())?;
        let storage = catalog.rel_storage(self.schema.name());
        if let Some(storage) = storage {
            validate_storage_lengths(storage, &from_layout, &to_layout)?;
        }
        let mut edges = Vec::new();
        if matches!(direction, Direction::Out | Direction::Both) {
            edges = self.property_neighbors_one(
                pager,
                storage.map_or(&[], |s| &s.fwd),
                &from_layout,
                to_layout.total_rows,
                Direction::Out,
                from,
            )?;
        }
        if matches!(direction, Direction::In | Direction::Both) {
            let incoming = self.property_neighbors_one(
                pager,
                storage.map_or(&[], |s| &s.bwd),
                &to_layout,
                from_layout.total_rows,
                Direction::In,
                from,
            )?;
            edges.extend(
                incoming
                    .into_iter()
                    .filter(|edge| direction != Direction::Both || edge.from != edge.to),
            );
        }
        Ok(edges)
    }

    /// Conservatively sizes a complete property adjacency snapshot and its
    /// simultaneous CSR, directional-list and conversion scratch before decode.
    pub fn property_neighbors_working_set_bytes(
        &self,
        pager: &Pager,
        catalog: &Catalog,
        source_rows: usize,
    ) -> DevonResult<usize> {
        let types = schema_types(&self.schema);
        let mut bytes = source_rows
            .checked_mul(128)
            .ok_or_else(|| corrupt("relationship adjacency size overflows"))?;
        if let Some(storage) = catalog.rel_storage(self.schema.name()) {
            for page in storage
                .fwd
                .iter()
                .chain(&storage.bwd)
                .filter(|page| **page != 0)
            {
                let estimate = CsrGroup::checkpoint_estimate(pager, *page, &types)?;
                let rows = estimate
                    .edge_count
                    .checked_mul(size_of::<ScannedRelationship>() + 64)
                    .ok_or_else(|| corrupt("relationship edge size overflows"))?;
                bytes = bytes
                    .checked_add(estimate.resident_bytes)
                    .and_then(|n| n.checked_add(estimate.encoded_bytes))
                    .and_then(|n| n.checked_add(rows))
                    .ok_or_else(|| corrupt("relationship adjacency size overflows"))?;
            }
        }
        for edge in &self.buffered_edges {
            let values = CsrGroup::checkpoint_property_bytes(&edge.values)?;
            bytes = bytes
                .checked_add(values)
                .and_then(|n| n.checked_add(128))
                .ok_or_else(|| corrupt("relationship overlay size overflows"))?;
        }
        bytes
            .checked_mul(4)
            .ok_or_else(|| corrupt("relationship working set overflows"))
    }

    #[allow(clippy::too_many_arguments)]
    fn property_neighbors_one(
        &self,
        pager: &Pager,
        ids: &[u64],
        layout: &EndpointLayout,
        neighbor_rows: u64,
        direction: Direction,
        from: u64,
    ) -> DevonResult<Vec<ScannedRelationship>> {
        if self.grouped_endpoint_is_tombstoned(direction, from) {
            return Ok(Vec::new());
        }
        let mut edges = Vec::new();
        if let Some((index, slot)) = layout.locate(from)
            && let Some(page) = stored_group_id(ids, index)
        {
            let group =
                CsrGroup::read_checked(pager, page, &schema_types(&self.schema), neighbor_rows)?;
            validate_csr_row_count(&group, layout.row_counts[index], index)?;
            if let Some(range) = group.edge_range(slot) {
                for position in range {
                    let neighbor = csr_neighbor(&group, position)?;
                    if self.neighbor_is_tombstoned(direction, neighbor) {
                        continue;
                    }
                    let (source, destination) = if direction == Direction::Out {
                        (from, neighbor)
                    } else {
                        (neighbor, from)
                    };
                    let values = (0..group.column_count())
                        .map(|column| {
                            group
                                .value(position, column)
                                .cloned()
                                .ok_or_else(|| corrupt("CSR edge property is missing"))
                        })
                        .collect::<DevonResult<Vec<_>>>()?;
                    edges.push(ScannedRelationship {
                        from: source,
                        to: destination,
                        values,
                    });
                }
            }
        }
        for edge in &self.buffered_edges {
            let matches = if direction == Direction::Out {
                edge.from == from
            } else {
                edge.to == from
            };
            if matches && !self.edge_is_tombstoned(edge.from, edge.to) {
                edges.push(ScannedRelationship {
                    from: edge.from,
                    to: edge.to,
                    values: edge.values.clone(),
                });
            }
        }
        Ok(edges)
    }
}
