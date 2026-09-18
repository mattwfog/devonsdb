# devondb Python bindings

Build the extension from the repository root:

```sh
PYO3_BUILD_EXTENSION_MODULE=1 cargo build -p devondb-python --release
```

Manual macOS builds also need PyO3's `-undefined dynamic_lookup` linker flags.

The Cargo library target remains `devondb_python`. Rename the resulting
`libdevondb_python.so` (Linux) or `libdevondb_python.dylib` (macOS) to
`devondb.so` in a directory on `PYTHONPATH` before importing it.

```python
import devondb
db = devondb.Database.create("graph.devondb")
db.execute("create node table Person (id Int64 primary key, name String)")
```

`Database.open` accepts the same keyword-only `page_size` and `memory_limit`
options as `Database.create`; `Database(path)` opens with facade defaults.
Queries return an object with `columns` and `rows`, and `schema()` returns a
native Python dictionary. The v0.1.0 release process packages this source
crate, not Python wheels; a wheel build and publication matrix remains a
separate post-v0.1.0 task.

## Arrow interoperability

Query results implement the [Arrow PyCapsule
Interface](https://arrow.apache.org/docs/format/CDataInterface/PyCapsuleInterface.html)
(`__arrow_c_schema__` / `__arrow_c_array__`), so Arrow-aware consumers read
them with zero copies and no devondb-specific code:

```python
import pyarrow as pa, polars as pl
table = pa.table(db.query("nodes(Person) as p | project p.id as id, p.name as name"))
frame = pl.from_arrow(db.query("nodes(Person) as p"))
```

The export is an Arrow struct array (`+s`) whose children are the result
columns, produced by devondb itself (no `pyarrow`/`arrow-rs` dependency).
Column types come from the first non-null value; a column of mixed types is
an error naming the column; NULL slots clear the validity bit and hold zero
in typed buffers. The mapping:

| devondb | Arrow |
|---|---|
| Int64 | `l` (int64) |
| Float64 | `g` (float64) |
| Bool | `b` (boolean) |
| String | `u` (utf8, int32 offsets; export fails past i32::MAX total bytes) |
| Timestamp (epoch µs UTC) | `tsu:UTC` (timestamp µs, UTC) |
| Decimal(p, s) | `d:p,s` (decimal128) |
| Bytes | `z` (binary) |
| Json | `u` with extension metadata `ARROW:extension:name` = `arrow.json` |
| Vector(n) | `+w:n` fixed-size list over `f` (float32); every row has exactly n |
| GeoPoint | `+s` struct with children `lat_deg: g`, `lng_deg: g` |
| column of only NULLs with unknown type | `n` (null) |

`requested_schema` is accepted by `__arrow_c_array__` and ignored, as the
PyCapsule Interface permits: each devondb column type has exactly one Arrow
representation.
