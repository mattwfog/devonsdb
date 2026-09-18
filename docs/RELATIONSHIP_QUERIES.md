# Relationship-property queries

`PLAN_IR.md` records the binding additive operator semantics. No
storage layout, WAL record, feature bit, or existing query changes.

## User-facing contract

Add `Operator::ExpandRel { rel, direction, from_binding, binding,
rel_binding, input }`. Keep `Expand` unchanged, including every constructor,
canonical JSON field and canonical text spelling. The new operator's JSON
fields appear in the order listed, after `op: "ExpandRel"`. Adding an operator
keeps plan version zero under the existing closed-vocabulary law.

The text form is:

```text
nodes(Person) as p | expand_rel Knows out as q via e | filter e.since > 2000 | project p.name, q.name, e.since
nodes(Person) as p | expand_rel Knows both from p as q via e | aggregate sum(e.weight) as total
```

`expand_rel` is contextual only at stage head; `via` is contextual only after
the neighbor binding in that stage. Neither becomes a lexer keyword: existing
tables, columns, and bindings with these names retain their canonical spelling.
The optional `from` has exactly `Expand`'s nearest-node-binding default. The
new nearest node binding is `q`, never `e`. Canonical printing omits `from`
under the existing rule, always prints `via`, and uses ordinary identifier
quoting. Old `expand ... as q` still lowers to `Expand`.

The output appends neighbor-node columns followed by relationship columns in
their respective catalog declaration orders. Hidden node offsets stay hidden.
`e.column` has the relationship column's logical type and ordinary NULL,
filter, projection, sort, aggregate, join and scalar-expression behavior.
Zero-property relationships are legal and still introduce a binding name.
Both new binding names must be distinct under ASCII folding and absent from
local and correlated-outer binding namespaces. The relationship binding owns
no node offset, table provenance, implicit ID, endpoint fields, or `classof`
discriminator. Expanding from it and `classof(e)` must fail validation.

## Edge identity and order

One visible physical edge occurrence produces one row, including parallel
edges with identical endpoints and different or identical values. Never join
properties onto neighbors using `(from,to)` as an identity: that would lose or
misassociate parallel edges. Neighbor and properties travel in one record.

For each upstream row, `out` and `in` preserve their physical CSR slot order
followed by overlay insertion order. `both` with coincident endpoint tables
emits out occurrences followed by in occurrences; each self-loop occurrence
appears once on the out side. Multiple self-loops remain multiple rows. With
different endpoint tables, `both` resolves to out for a from-table binding and
in for a to-table binding. The resulting neighbor type is therefore concrete.
The source node's physical offset domain, not its primary key or compacted
visible row index, selects adjacency.

## Existing storage seams and necessary extension

The source already has two property-preserving APIs:

- `RelTable::scan_cursor` returns `ScannedRelationship { from, to, values }`,
  merges buffered edges, and applies endpoint tombstones, one CSR group at a
  time (`crates/devondb-storage/src/rel_table.rs`).
- `Database::visible_relationships` returns a pinned, streaming facade cursor
  with values (`crates/devondb/src/database/dump.rs`).

Neither is the exact query seam: both scan forward CSR only. Grouping that
scan by destination changes incoming per-slot insertion order after a
checkpoint. The facade cursor also omits transaction-local writes. It is
suited to dump/inspection, and should stay unchanged.

Add a direction-aware property traversal on `RelTable`, for example
`neighbors_with_properties(pager, catalog, direction, from) ->
DevonResult<Vec<ScannedRelationship>>`. Factor its direction selection and
visibility from `neighbors`, and use `CsrGroup::edge_range`, `neighbor`, and
`value` in the chosen fwd/bwd group. For incoming reads reconstruct the real
source/destination offsets, preserving values from that same CSR entry.
Append matching buffered edges in order, apply the existing endpoint-role
tombstones, and suppress incoming self-loop occurrences for `both`.

The storage implementation may live in `rel_table/property_neighbors.rs` as a
child module to reuse private layout, tombstone, and CSR helpers. Return
`Corrupt` for malformed groups, missing values, or invalid endpoint domains,
as existing adjacency does. No new persistent edge ID or dependency is needed.

## Execution, MVCC and budget

Add an executor edge-neighbor trait whose records pair neighbor offsets with
relationship values, and a separate `ExpandRel` pull operator. The old
`NeighborSource` and `Expand` public constructor remain source-compatible.
The new physical output is input values, neighbor offset, neighbor values,
relationship values. The operator validates property count/types and resumes
within a pending input row across chunk boundaries.

The facade's new `database/relationship_query.rs` child module builds a
property-preserving adjacency snapshot from the same `ReadView` as all other
operators. Reuse `node_rows_for_view`, `traversal_tables`, `visible_node_count`,
`rel_overlay_bytes`, and `effective_rel_tombstones`. Recover committed edges
from `view.state.rel_edges`, then transaction-local edges, into `RelTable`.
Use the existing hole-aware node rows; a surviving edge into a hole is
corruption. Keep relationships distinct from `Pipeline.binding_tables` and
`Pipeline.offsets`, adding their columns/types only. Preserve their names in
validator and parser namespace tracking, even when they have zero columns.

This reuses the established bounded-materialization approach of `Expand`:
reserve the overlay clone, destination rows, adjacency vector capacities,
every relationship `Value` and variable-length payload, and CSR decode scratch
before allocation. Use `CsrGroup::checkpoint_estimate` for encoded/resident
scratch and `checkpoint_property_bytes` where useful; do not estimate a string
only as `size_of::<Value>()`. Include the simultaneous out/in copies before
self-loop removal. Keep charges alive as long as the allocations they cover;
the pending neighbor list and chunk-building clones need their own reservation
or an explicitly proven peak allowance. Return a measured `BudgetExceeded`
instead of bypassing the database limit. A streaming incident cursor is a
future performance improvement; the initial implementation must be honest
about refusing an adjacency working set above the limit.

Snapshot readers and scalar subqueries use only their captured `ReadView`.
Transaction queries include their own inserted relationships and effective
detach tombstones. An old snapshot keeps old values and old offset epoch
across detach/checkpoint; new snapshots use the atomically remapped catalog.
Recovery and follower refresh require no new machinery because all properties
already reside in the existing WAL/CSR records. Query tests must prove those
paths, including reinserted keys receiving new offset tails.

## Implemented memory policy

Property adjacency retains shared immutable incident lists; the executor never
clones a whole high-degree list for a pending input row. Output batches hold
at most 128 rows, avoiding the full 2048-row builder capacity for small edge
queries. The executor reserves pending input and output construction peaks
before cloning values and retains its high-water scratch allowance until drop.

`NodeGroupDirectory::column_decode_peak_bytes(column, types)` provides a
conservative predecode bound, preserving the String-specific encoding bounds
and including b1 auxiliary rescore storage. Neighbor materialization reserves
its peak before decoding and releases that temporary allowance once the
existing materializer installs its actual retained-row charge. Source scans
feeding ExpandRel reserve a group-decode peak before constructing/pulling the
upstream pipeline; their allowance lasts through query execution. These bounds
favor predictable refusal over allocating beyond the limit, so a wide decoded
column may require more budget than the final projected result. Existing
ordinary Expand and standalone scan memory behavior is unchanged.

Relationship DDL currently supports Bool, Int64, Float64, String and vector
properties. Scalar-v2 and GeoPoint relationship DDL refusals remain unchanged;
the new binding exposes every property type the stored relationship admits.
