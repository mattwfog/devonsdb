//! Deterministic fixture and expectation generator for `tools/stress`.
//!
//! The generator never opens devondb. It writes only public CLI inputs,
//! Parquet COPY sources, and independently computed expected text.

use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use parquet::{
    basic::{
        Compression, ConvertedType, DecimalType, LogicalType as ParquetLogical, Repetition,
        Type as Physical,
    },
    data_type::{BoolType, ByteArray, ByteArrayType, DoubleType, Int64Type},
    file::{
        properties::WriterProperties,
        writer::{SerializedFileWriter, SerializedRowGroupWriter},
    },
    schema::types::{Type, TypePtr},
};

type GenResult<T> = Result<T, Box<dyn Error>>;

// One wide opaque value reaches byte volume without asking the debug build to
// run a text codec over tens of thousands of small strings. Bytes use the
// same var-length heap framing while remaining incompressible and exact.
const NODE_PAYLOAD_BYTES: usize = 64 * 1024;
const REL_PAYLOAD_BYTES: usize = 1024;
const PARQUET_GROUP_BYTES: usize = 4 * 1024 * 1024;
const CHURN_PAYLOAD_BYTES: usize = 800;

fn main() {
    if let Err(error) = run() {
        eprintln!("stress-gen: {error}");
        std::process::exit(1);
    }
}

fn run() -> GenResult<()> {
    let mut args = std::env::args().skip(1);
    let mode = args.next().ok_or("missing mode")?;
    let out = PathBuf::from(args.next().ok_or("missing output directory")?);
    fs::create_dir_all(&out)?;
    match mode.as_str() {
        "volume" => {
            let target = parse::<u64>(args.next(), "target bytes")?;
            let seed = parse::<u64>(args.next(), "seed")?;
            ensure_done(args)?;
            generate_volume(&out, target, seed)
        }
        "churn" => {
            let rows = parse::<usize>(args.next(), "rows")?;
            let cycles = parse::<usize>(args.next(), "cycles")?;
            let seed = parse::<u64>(args.next(), "seed")?;
            ensure_done(args)?;
            generate_churn(&out, rows, cycles, seed)
        }
        "reopen" => {
            let seed = parse::<u64>(args.next(), "seed")?;
            ensure_done(args)?;
            generate_reopen(&out, seed)
        }
        "crash" => {
            let rows = parse::<usize>(args.next(), "rows")?;
            let batch = parse::<usize>(args.next(), "batch")?;
            let payload = parse::<usize>(args.next(), "payload bytes")?;
            let iterations = parse::<usize>(args.next(), "iterations")?;
            let seed = parse::<u64>(args.next(), "seed")?;
            ensure_done(args)?;
            generate_crash(&out, rows, batch, payload, iterations, seed)
        }
        _ => Err(format!("unknown mode `{mode}`").into()),
    }
}

fn parse<T: std::str::FromStr>(value: Option<String>, name: &str) -> GenResult<T>
where
    T::Err: Error + 'static,
{
    Ok(value.ok_or_else(|| format!("missing {name}"))?.parse()?)
}

fn ensure_done(mut args: impl Iterator<Item = String>) -> GenResult<()> {
    if let Some(extra) = args.next() {
        return Err(format!("unexpected argument `{extra}`").into());
    }
    Ok(())
}

fn primitive(name: &str, physical: Physical) -> parquet::schema::types::PrimitiveTypeBuilder<'_> {
    Type::primitive_type_builder(name, physical).with_repetition(Repetition::REQUIRED)
}

fn int64_field(name: &str) -> GenResult<TypePtr> {
    Ok(Arc::new(primitive(name, Physical::INT64).build()?))
}

fn bool_field(name: &str) -> GenResult<TypePtr> {
    Ok(Arc::new(primitive(name, Physical::BOOLEAN).build()?))
}

fn float64_field(name: &str) -> GenResult<TypePtr> {
    Ok(Arc::new(primitive(name, Physical::DOUBLE).build()?))
}

fn string_field(name: &str) -> GenResult<TypePtr> {
    Ok(Arc::new(
        primitive(name, Physical::BYTE_ARRAY)
            .with_logical_type(Some(ParquetLogical::String))
            .with_converted_type(ConvertedType::UTF8)
            .build()?,
    ))
}

fn bytes_field(name: &str) -> GenResult<TypePtr> {
    Ok(Arc::new(primitive(name, Physical::BYTE_ARRAY).build()?))
}

fn decimal_field(name: &str) -> GenResult<TypePtr> {
    Ok(Arc::new(
        primitive(name, Physical::INT64)
            .with_logical_type(Some(ParquetLogical::Decimal(DecimalType {
                scale: 2,
                precision: 18,
            })))
            .with_converted_type(ConvertedType::DECIMAL)
            .with_precision(18)
            .with_scale(2)
            .build()?,
    ))
}

fn parquet_schema(fields: Vec<TypePtr>) -> GenResult<TypePtr> {
    Ok(Arc::new(
        Type::group_type_builder("schema")
            .with_fields(fields)
            .build()?,
    ))
}

fn parquet_writer(path: &Path, schema: TypePtr) -> GenResult<SerializedFileWriter<fs::File>> {
    let properties = Arc::new(
        WriterProperties::builder()
            .set_compression(Compression::UNCOMPRESSED)
            .build(),
    );
    Ok(SerializedFileWriter::new(
        fs::File::create(path)?,
        schema,
        properties,
    )?)
}

fn write_i64(group: &mut SerializedRowGroupWriter<'_, fs::File>, values: &[i64]) -> GenResult<()> {
    let mut column = group.next_column()?.ok_or("missing i64 column")?;
    column
        .typed::<Int64Type>()
        .write_batch(values, None, None)?;
    column.close()?;
    Ok(())
}

fn write_bool(
    group: &mut SerializedRowGroupWriter<'_, fs::File>,
    values: &[bool],
) -> GenResult<()> {
    let mut column = group.next_column()?.ok_or("missing bool column")?;
    column.typed::<BoolType>().write_batch(values, None, None)?;
    column.close()?;
    Ok(())
}

fn write_f64(group: &mut SerializedRowGroupWriter<'_, fs::File>, values: &[f64]) -> GenResult<()> {
    let mut column = group.next_column()?.ok_or("missing f64 column")?;
    column
        .typed::<DoubleType>()
        .write_batch(values, None, None)?;
    column.close()?;
    Ok(())
}

fn write_bytes(
    group: &mut SerializedRowGroupWriter<'_, fs::File>,
    values: &[ByteArray],
) -> GenResult<()> {
    let mut column = group.next_column()?.ok_or("missing byte-array column")?;
    column
        .typed::<ByteArrayType>()
        .write_batch(values, None, None)?;
    column.close()?;
    Ok(())
}

fn ceil_div(value: u64, divisor: u64) -> u64 {
    value.div_ceil(divisor)
}

fn generate_volume(out: &Path, target: u64, seed: u64) -> GenResult<()> {
    // Relationship property bytes are persisted in both CSR directions and
    // are deliberately a small slice: the suite still loads a real rel table,
    // while the node Bytes column supplies predictable target-size volume.
    let node_target = target.saturating_mul(99) / 100;
    let rel_target = target.saturating_sub(node_target);
    let node_rows = ceil_div(node_target, NODE_PAYLOAD_BYTES as u64).max(2);
    let rel_rows = ceil_div(rel_target, REL_PAYLOAD_BYTES as u64).max(1);
    write_volume_nodes(&out.join("payload.parquet"), node_rows, seed)?;
    write_volume_rels(&out.join("links.parquet"), node_rows, rel_rows, seed)?;
    write_volume_inputs(out, node_rows, rel_rows, seed)?;
    let logical_bytes = node_rows * NODE_PAYLOAD_BYTES as u64 + rel_rows * REL_PAYLOAD_BYTES as u64;
    fs::write(
        out.join("manifest.txt"),
        format!(
            "seed={seed}\ntarget_bytes={target}\nlogical_bytes={logical_bytes}\nnode_rows={node_rows}\nrel_rows={rel_rows}\n"
        ),
    )?;
    println!(
        "stress-gen: volume seed={seed} nodes={node_rows} rels={rel_rows} logical_bytes={logical_bytes}"
    );
    Ok(())
}

fn write_volume_nodes(path: &Path, rows: u64, seed: u64) -> GenResult<()> {
    let schema = parquet_schema(vec![
        int64_field("id")?,
        bytes_field("payload")?,
        int64_field("bucket")?,
        decimal_field("amount")?,
    ])?;
    let mut writer = parquet_writer(path, schema)?;
    let group_rows = (PARQUET_GROUP_BYTES / NODE_PAYLOAD_BYTES).max(1) as u64;
    let mut start = 1_u64;
    while start <= rows {
        let end = (start + group_rows - 1).min(rows);
        let ids: Vec<i64> = (start..=end).map(as_i64).collect::<GenResult<_>>()?;
        let payloads: Vec<ByteArray> = (start..=end)
            .map(|id| ByteArray::from(binary_payload(seed, id, NODE_PAYLOAD_BYTES, 0x4e)))
            .collect();
        let buckets: Vec<i64> = (start..=end).map(|id| ((id - 1) % 4) as i64).collect();
        let amounts: Vec<i64> = (start..=end).map(amount_cents).collect::<GenResult<_>>()?;
        let mut group = writer.next_row_group()?;
        write_i64(&mut group, &ids)?;
        write_bytes(&mut group, &payloads)?;
        write_i64(&mut group, &buckets)?;
        write_i64(&mut group, &amounts)?;
        group.close()?;
        start = end + 1;
    }
    writer.close()?;
    Ok(())
}

fn write_volume_rels(path: &Path, nodes: u64, rows: u64, seed: u64) -> GenResult<()> {
    let schema = parquet_schema(vec![
        int64_field("from")?,
        int64_field("to")?,
        string_field("tag")?,
        int64_field("code")?,
        float64_field("weight")?,
        bool_field("active")?,
    ])?;
    let mut writer = parquet_writer(path, schema)?;
    let group_rows = (PARQUET_GROUP_BYTES / REL_PAYLOAD_BYTES).max(1) as u64;
    let mut start = 1_u64;
    while start <= rows {
        let end = (start + group_rows - 1).min(rows);
        let from: Vec<i64> = (start..=end)
            .map(|id| as_i64((id - 1) % nodes + 1))
            .collect::<GenResult<_>>()?;
        let to: Vec<i64> = (start..=end)
            .map(|id| as_i64((id.saturating_mul(17) - 1) % nodes + 1))
            .collect::<GenResult<_>>()?;
        let tags: Vec<ByteArray> = (start..=end)
            .map(|id| ByteArray::from(payload(seed, id, REL_PAYLOAD_BYTES, 0x52).into_bytes()))
            .collect();
        let codes: Vec<i64> = (start..=end).map(as_i64).collect::<GenResult<_>>()?;
        let weights: Vec<f64> = (start..=end)
            .map(|id| ((id - 1) % 8) as f64 + 0.5)
            .collect();
        let active: Vec<bool> = (start..=end).map(|id| id % 2 == 0).collect();
        let mut group = writer.next_row_group()?;
        write_i64(&mut group, &from)?;
        write_i64(&mut group, &to)?;
        write_bytes(&mut group, &tags)?;
        write_i64(&mut group, &codes)?;
        write_f64(&mut group, &weights)?;
        write_bool(&mut group, &active)?;
        group.close()?;
        start = end + 1;
    }
    writer.close()?;
    Ok(())
}

fn write_volume_inputs(out: &Path, node_rows: u64, rel_rows: u64, seed: u64) -> GenResult<()> {
    let payload_path = quote_string(&out.join("payload.parquet").to_string_lossy());
    let links_path = quote_string(&out.join("links.parquet").to_string_lossy());
    let load = format!(
        "create node table Payload (id Int64 primary key, payload Bytes, bucket Int64, amount Decimal(18, 2))\n\
         create node table Types (id Int64 primary key, flag Bool, signed Int64, ratio Float64, name String, embedding Vector(3), place GeoPoint, happened Timestamp, blob Bytes, amount Decimal(18, 2), doc Json)\n\
         create rel table Links from Payload to Payload (tag String, code Int64, weight Float64, active Bool)\n\
         create rel table TypedLink from Types to Types (enabled Bool, rank Int64, weight Float64, note String, direction Vector(3))\n\
         insert into Types values (1, true, -7, 1.5, \"alpha\", [1.0, 0.0, -1.0], geo(42.5, -71.25), timestamp(\"2026-01-01T00:00:00Z\"), bytes(\"00ff10\"), decimal(\"19.99\"), json(\"{{\\\"k\\\":1,\\\"ok\\\":true}}\")), (2, false, 9, 2.5, \"beta\", [0.0, 1.0, 0.0], geo(41.0, -70.0), timestamp(\"2026-01-02T00:00:00Z\"), bytes(\"aabb\"), decimal(\"20.01\"), json(\"[1,2,3]\"))\n\
         insert rel into TypedLink values (1 -> 2, true, 3, 4.5, \"typed\", [0.0, 1.0, 0.0])\n\
         copy Payload from {payload_path}\n\
         copy Links from {links_path}\n\
         .checkpoint\n.exit\n"
    );
    fs::write(out.join("load.txt"), load)?;

    let first_payload = hex(&binary_payload(seed, 1, NODE_PAYLOAD_BYTES, 0x4e));
    let queries = format!(
        "nodes(Payload) as p | aggregate count(p.id) as n\n\
         nodes(Payload) as p | aggregate sum(p.amount) as total\n\
         nodes(Payload) as p | aggregate count(p.id) as n by p.bucket\n\
         nodes(Payload) as p | expand Links out as neighbor | aggregate count(neighbor.id) as n\n\
         nodes(Payload) as p | expand Links in as neighbor | aggregate count(neighbor.id) as n\n\
         nodes(Payload) as p | filter p.id = 1 and p.payload = bytes(\"{first_payload}\") | aggregate count(p.id) as n\n\
         nodes(Types) as t | filter t.id = 1 | project t.flag as flag, t.signed as signed, t.ratio as ratio, t.name as name, t.embedding as embedding, t.place as place, t.happened as happened, t.blob as blob, t.amount as amount, t.doc as doc\n\
         nodes(Types) as t | filter t.id = 1 | expand TypedLink out as neighbor | project neighbor.id as id, neighbor.name as name\n\
         knn(Types.embedding, [1.0, 0.0, -1.0], 1, cosine) | project Types.id as id\n\
         within(Types.place, geo(42.5, -71.25), 1.0) | project Types.id as id\n.exit\n"
    );
    fs::write(out.join("queries.txt"), queries)?;

    let mut expected = String::new();
    push_table(&mut expected, &["n"], &[vec![node_rows.to_string()]]);
    push_table(
        &mut expected,
        &["total"],
        &[vec![format!(
            "decimal(\"{}\")",
            money(node_amount_total(node_rows)?)
        )]],
    );
    let bucket_rows: Vec<Vec<String>> = (0_u64..4)
        .map(|bucket| {
            vec![
                bucket.to_string(),
                bucket_count(node_rows, bucket).to_string(),
            ]
        })
        .collect();
    push_table(&mut expected, &["p.bucket", "n"], &bucket_rows);
    push_table(&mut expected, &["n"], &[vec![rel_rows.to_string()]]);
    push_table(&mut expected, &["n"], &[vec![rel_rows.to_string()]]);
    push_table(&mut expected, &["n"], &[vec!["1".to_owned()]]);
    push_table(
        &mut expected,
        &[
            "flag",
            "signed",
            "ratio",
            "name",
            "embedding",
            "place",
            "happened",
            "blob",
            "amount",
            "doc",
        ],
        &[vec![
            "true".to_owned(),
            "-7".to_owned(),
            "1.5".to_owned(),
            "\"alpha\"".to_owned(),
            "[1, 0, -1]".to_owned(),
            "geo(42.5, -71.25)".to_owned(),
            "timestamp(\"2026-01-01T00:00:00Z\")".to_owned(),
            "bytes(\"00ff10\")".to_owned(),
            "decimal(\"19.99\")".to_owned(),
            "json(\"{\\\"k\\\":1,\\\"ok\\\":true}\")".to_owned(),
        ]],
    );
    push_table(
        &mut expected,
        &["id", "name"],
        &[vec!["2".to_owned(), "\"beta\"".to_owned()]],
    );
    push_table(&mut expected, &["id"], &[vec!["1".to_owned()]]);
    push_table(&mut expected, &["id"], &[vec!["1".to_owned()]]);
    fs::write(out.join("expected-query.out"), expected)?;
    Ok(())
}

fn node_amount_total(rows: u64) -> GenResult<i64> {
    let mut total = 0_i64;
    for id in 1..=rows {
        total = total
            .checked_add(amount_cents(id)?)
            .ok_or("volume decimal expectation overflow")?;
    }
    Ok(total)
}

fn bucket_count(rows: u64, bucket: u64) -> u64 {
    if rows <= bucket {
        0
    } else {
        (rows - 1 - bucket) / 4 + 1
    }
}

fn amount_cents(id: u64) -> GenResult<i64> {
    let value = 1_000_u64 + id.saturating_mul(37) % 90_000;
    as_i64(value)
}

fn generate_churn(out: &Path, rows: usize, cycles: usize, seed: u64) -> GenResult<()> {
    if rows == 0 || cycles == 0 {
        return Err("churn rows and cycles must be positive".into());
    }
    let mut seed_input = String::from(
        "create node table Churn (id Int64 primary key, version Int64, payload String)\n",
    );
    for chunk_start in (1..=rows).step_by(25) {
        let chunk_end = (chunk_start + 24).min(rows);
        seed_input.push_str("insert into Churn values ");
        for id in chunk_start..=chunk_end {
            if id > chunk_start {
                seed_input.push_str(", ");
            }
            let text = payload(seed, id as u64, CHURN_PAYLOAD_BYTES, 0x43);
            seed_input.push_str(&format!("({id}, 0, {})", quote_string(&text)));
        }
        seed_input.push('\n');
    }
    seed_input.push_str(".checkpoint\n.exit\n");
    fs::write(out.join("seed.txt"), seed_input)?;

    let mut versions = vec![0_usize; rows];
    let mut latest_payloads: Vec<String> = (1..=rows)
        .map(|id| payload(seed, id as u64, CHURN_PAYLOAD_BYTES, 0x43))
        .collect();
    let mut updates = String::new();
    for cycle in 1..=cycles {
        let id = (cycle - 1) % rows + 1;
        let text = payload(seed ^ cycle as u64, id as u64, CHURN_PAYLOAD_BYTES, 0x55);
        updates.push_str(&format!(
            "upsert Churn values ({id}, {cycle}, {})\n",
            quote_string(&text)
        ));
        versions[id - 1] = cycle;
        latest_payloads[id - 1] = text;
    }
    fs::write(out.join("updates.txt"), updates)?;

    let sum: usize = versions.iter().sum();
    let last_id = (cycles - 1) % rows + 1;
    let queries = format!(
        "nodes(Churn) as j | aggregate count(j.id) as n\n\
         nodes(Churn) as j | aggregate sum(j.version) as total\n\
         nodes(Churn) as j | filter j.id = {last_id} and j.payload = {} | aggregate count(j.id) as n\n.exit\n",
        quote_string(&latest_payloads[last_id - 1])
    );
    fs::write(out.join("queries.txt"), queries)?;
    let mut expected = String::new();
    push_table(&mut expected, &["n"], &[vec![rows.to_string()]]);
    push_table(&mut expected, &["total"], &[vec![sum.to_string()]]);
    push_table(&mut expected, &["n"], &[vec!["1".to_owned()]]);
    fs::write(out.join("expected-query.out"), expected)?;
    let logical_bytes = rows
        .checked_mul(CHURN_PAYLOAD_BYTES + 16)
        .ok_or("churn logical byte count overflow")?;
    fs::write(
        out.join("manifest.txt"),
        format!(
            "seed={seed}\nrows={rows}\ncycles={cycles}\nlogical_bytes={logical_bytes}\nversion_sum={sum}\n"
        ),
    )?;
    println!("stress-gen: churn seed={seed} rows={rows} cycles={cycles}");
    Ok(())
}

fn generate_reopen(out: &Path, seed: u64) -> GenResult<()> {
    let load = "create node table Item (id Int64 primary key, amount Decimal(18, 2), name String)\n\
                create rel table Next from Item to Item (weight Int64)\n\
                insert into Item values (1, decimal(\"10.00\"), \"one\"), (2, decimal(\"20.00\"), \"two\"), (3, decimal(\"30.00\"), \"three\"), (4, decimal(\"40.00\"), \"four\")\n\
                insert rel into Next values (1 -> 2, 10), (2 -> 3, 20), (3 -> 4, 30)\n.exit\n";
    let queries = "nodes(Item) as i | aggregate count(i.id) as n\n\
                   nodes(Item) as i | aggregate sum(i.amount) as total\n\
                   nodes(Item) as i | expand Next out as neighbor | aggregate count(neighbor.id) as n\n\
                   nodes(Item) as i | filter i.id = 3 | project i.name as name, i.amount as amount\n.exit\n";
    fs::write(out.join("load.txt"), load)?;
    fs::write(out.join("queries.txt"), queries)?;
    let mut expected = String::new();
    push_table(&mut expected, &["n"], &[vec!["4".to_owned()]]);
    push_table(
        &mut expected,
        &["total"],
        &[vec!["decimal(\"100.00\")".to_owned()]],
    );
    push_table(&mut expected, &["n"], &[vec!["3".to_owned()]]);
    push_table(
        &mut expected,
        &["name", "amount"],
        &[vec![
            "\"three\"".to_owned(),
            "decimal(\"30.00\")".to_owned(),
        ]],
    );
    fs::write(out.join("expected-query.out"), expected)?;
    fs::write(
        out.join("manifest.txt"),
        format!("seed={seed}\nrows=4\nrels=3\n"),
    )?;
    println!("stress-gen: reopen seed={seed} rows=4 rels=3");
    Ok(())
}

fn generate_crash(
    out: &Path,
    rows: usize,
    batch: usize,
    payload_bytes: usize,
    iterations: usize,
    seed: u64,
) -> GenResult<()> {
    if rows == 0 || batch == 0 || iterations == 0 || !rows.is_multiple_of(batch) {
        return Err(
            "crash rows/iterations/batch must be positive and rows divisible by batch".into(),
        );
    }
    fs::write(
        out.join("load.txt"),
        "create node table Crash (id Int64 primary key, payload String, amount Decimal(18, 2))\n.exit\n",
    )?;
    let mut work = String::new();
    for start in (1..=rows).step_by(batch) {
        work.push_str("insert into Crash values ");
        for id in start..start + batch {
            if id > start {
                work.push_str(", ");
            }
            let text = payload(seed, id as u64, payload_bytes, 0x4b);
            work.push_str(&format!(
                "({id}, {}, decimal(\"{}\"))",
                quote_string(&text),
                money(id as i64)
            ));
        }
        work.push('\n');
    }
    work.push_str(".exit\n");
    fs::write(out.join("work.txt"), work)?;
    fs::write(
        out.join("queries.txt"),
        "nodes(Crash) as c | aggregate count(c.id) as n, sum(c.id) as total, min(c.id) as first, max(c.id) as last, sum(c.amount) as money\n.exit\n",
    )?;
    fs::write(out.join("checkpoint.txt"), ".checkpoint\n.exit\n")?;
    let mut delays = String::new();
    let mut state = seed;
    for iteration in 1..=iterations {
        state = splitmix64(state);
        let delay_ms = 5 + state % 36;
        delays.push_str(&format!("{iteration}\t{delay_ms}\n"));
    }
    fs::write(out.join("delays.tsv"), delays)?;
    fs::write(
        out.join("manifest.txt"),
        format!(
            "seed={seed}\nrows={rows}\nbatch={batch}\npayload_bytes={payload_bytes}\niterations={iterations}\n"
        ),
    )?;
    println!(
        "stress-gen: crash seed={seed} rows={rows} batch={batch} payload_bytes={payload_bytes} iterations={iterations}"
    );
    Ok(())
}

fn payload(seed: u64, id: u64, len: usize, lane: u64) -> String {
    const ALPHABET: &[u8; 64] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz-_";
    let mut state = seed ^ id.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ lane;
    let mut bytes = Vec::with_capacity(len);
    while bytes.len() < len {
        state = splitmix64(state);
        let mut word = state;
        for _ in 0..10 {
            if bytes.len() == len {
                break;
            }
            bytes.push(ALPHABET[(word & 63) as usize]);
            word >>= 6;
        }
    }
    String::from_utf8(bytes).unwrap_or_default()
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn binary_payload(seed: u64, id: u64, len: usize, lane: u64) -> Vec<u8> {
    let mut state = seed ^ id.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ lane;
    let mut bytes = Vec::with_capacity(len);
    while bytes.len() < len {
        state = splitmix64(state);
        let word = state.to_le_bytes();
        let remaining = len - bytes.len();
        bytes.extend_from_slice(&word[..remaining.min(word.len())]);
    }
    bytes
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(DIGITS[(byte >> 4) as usize]));
        encoded.push(char::from(DIGITS[(byte & 0x0f) as usize]));
    }
    encoded
}

fn quote_string(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

fn money(cents: i64) -> String {
    format!("{}.{:02}", cents / 100, cents % 100)
}

fn as_i64(value: u64) -> GenResult<i64> {
    Ok(i64::try_from(value)?)
}

fn push_table(output: &mut String, columns: &[&str], rows: &[Vec<String>]) {
    let header = columns.join(" | ");
    output.push_str(&header);
    output.push('\n');
    output.push_str(&"-".repeat(header.chars().count()));
    output.push('\n');
    for row in rows {
        output.push_str(&row.join(" | "));
        output.push('\n');
    }
    output.push_str(&format!("({} rows)\n", rows.len()));
}
