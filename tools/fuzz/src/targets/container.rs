use std::{fs, path::Path};

use devondb::{Database, DevonError, Options, text::parser::Parsed, text::parser::parse};

use crate::{mutate::mutate_bytes, rng::Rng};

const PAGE_SIZE: usize = 4096;
const MAX_CONTAINER_INPUT_BYTES: usize = 4 * 1024 * 1024;
const PACK_HEADER_LEN: usize = 40;
const PACK_DIRECTORY_ENTRY_LEN: usize = 16;

#[derive(Clone, Copy)]
enum ImageKind {
    Main = 0,
    Wal = 1,
    Pack = 2,
}

impl ImageKind {
    fn from_byte(byte: u8) -> Self {
        match byte % 3 {
            0 => Self::Main,
            1 => Self::Wal,
            _ => Self::Pack,
        }
    }
}

/// Deterministic storage images built from the public database facade.
pub(crate) struct Fixtures {
    root: std::path::PathBuf,
    main: Vec<u8>,
    wal: Vec<u8>,
    pack: Vec<u8>,
}

impl Fixtures {
    /// Directory containing the pristine WAL base main file.
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }
}

/// Builds checkpoint, WAL-recovery, and DEVONPACK fixtures.
pub(crate) fn build_fixtures(root: &Path) -> Result<Fixtures, String> {
    fs::create_dir_all(root).map_err(|error| format!("create fixture directory: {error}"))?;
    let main_path = root.join("checkpoint.devondb");
    build_checkpoint_fixture(&main_path)?;
    normalize_db_id(&main_path, *b"devondb-fuzz-db!")?;

    let pack_path = root.join("checkpoint.devonpack");
    let mut database = Database::open(&main_path)
        .map_err(|error| format!("reopen checkpoint fixture for pack: {error}"))?;
    database
        .pack(&pack_path, 2)
        .map_err(|error| format!("pack checkpoint fixture: {error}"))?;
    drop(database);

    let wal_main_path = root.join("wal-base.devondb");
    let wal_writer_path = root.join("wal-writer.devondb");
    // Keep the writer alive until both images are captured: clean close would
    // otherwise checkpoint the insert and erase the live-WAL coverage.
    let wal_writer = build_wal_fixture(&wal_writer_path)?;
    fs::copy(&wal_writer_path, &wal_main_path)
        .map_err(|error| format!("capture WAL checkpoint base: {error}"))?;
    normalize_db_id(&wal_main_path, *b"devondb-fuzz-wal")?;
    let wal = read(&sidecar_path(&wal_writer_path))?;
    if wal.is_empty() {
        return Err("WAL fixture lost its acknowledged pending insert".to_owned());
    }
    drop(wal_writer);

    Ok(Fixtures {
        root: root.to_path_buf(),
        main: read(&main_path)?,
        wal,
        pack: read(&pack_path)?,
    })
}

fn build_checkpoint_fixture(path: &Path) -> Result<(), String> {
    let mut database = Database::create(path, PAGE_SIZE as u32)
        .map_err(|error| format!("create checkpoint fixture: {error}"))?;
    execute_text(
        &mut database,
        "create node table Person (id Int64 primary key, name String, score Float64, active Bool, bucket Int64, embedding Vector(3))",
    )?;
    execute_text(
        &mut database,
        "create node table Company (id Int64 primary key, name String)",
    )?;
    execute_text(
        &mut database,
        "create rel table WorksAt from Person to Company (since Int64)",
    )?;
    execute_text(&mut database, &person_insert())?;
    execute_text(
        &mut database,
        "insert into Company values (10, \"Devon\"), (11, \"Analytical Engines\")",
    )?;
    execute_text(&mut database, &relationship_insert())?;
    database
        .checkpoint()
        .map_err(|error| format!("checkpoint fixture: {error}"))?;
    Ok(())
}

fn build_wal_fixture(path: &Path) -> Result<Database, String> {
    let mut database = Database::create(path, PAGE_SIZE as u32)
        .map_err(|error| format!("create WAL fixture: {error}"))?;
    execute_text(
        &mut database,
        "create node table Person (id Int64 primary key, name String, score Float64, active Bool, bucket Int64, embedding Vector(3))",
    )?;
    database
        .checkpoint()
        .map_err(|error| format!("checkpoint WAL fixture schema: {error}"))?;
    execute_text(
        &mut database,
        "insert into Person values (999, \"WAL resident\", 4.5, true, 7, [1, 2, 3])",
    )?;
    Ok(database)
}

fn person_insert() -> String {
    let rows: Vec<String> = (0..96)
        .map(|id| {
            format!(
                "({id}, \"person-{id:03}\", {}.25, {}, 7, [{}, {}, {}])",
                id % 9,
                if id % 2 == 0 { "true" } else { "false" },
                id % 3,
                (id + 1) % 3,
                (id + 2) % 3
            )
        })
        .collect();
    format!("insert into Person values {}", rows.join(", "))
}

fn relationship_insert() -> String {
    let rows: Vec<String> = (0..64)
        .map(|id| format!("({id} -> {}, {})", 10 + id % 2, 2000 + id))
        .collect();
    format!("insert rel into WorksAt values {}", rows.join(", "))
}

fn execute_text(database: &mut Database, text: &str) -> Result<(), String> {
    let parsed = parse(text).map_err(|error| format!("fixture parse `{text}`: {error}"))?;
    let Parsed::Statement(envelope) = parsed else {
        return Err(format!("fixture text parsed as a query: {text}"));
    };
    database
        .execute(&envelope.stmt)
        .map_err(|error| format!("fixture execute `{text}`: {error}"))
}

fn normalize_db_id(path: &Path, db_id: [u8; 16]) -> Result<(), String> {
    let mut bytes = read(path)?;
    for slot in 0..2 {
        let start = slot * PAGE_SIZE;
        let Some(header) = bytes.get_mut(start..start + 64) else {
            return Err("fixture has no complete dual superblock".to_owned());
        };
        header[28..44].copy_from_slice(&db_id);
        let checksum = crc32c(&header[..60]);
        header[60..64].copy_from_slice(&checksum.to_le_bytes());
    }
    fs::write(path, bytes).map_err(|error| format!("normalize fixture db id: {error}"))
}

/// Selects a fixture and applies region-aware plus raw-byte mutations.
pub(crate) fn generate(
    execution: u64,
    rng: &mut Rng,
    intensity: usize,
    fixtures: &Fixtures,
) -> Vec<u8> {
    let kind = if execution < 3 {
        ImageKind::from_byte(execution as u8)
    } else {
        ImageKind::from_byte(rng.byte())
    };
    let mut payload = match kind {
        ImageKind::Main => fixtures.main.clone(),
        ImageKind::Wal => fixtures.wal.clone(),
        ImageKind::Pack => fixtures.pack.clone(),
    };
    if execution >= 3 {
        mutate_image(kind, &mut payload, rng, intensity);
    }
    payload.truncate(MAX_CONTAINER_INPUT_BYTES.saturating_sub(1));
    let mut input = Vec::with_capacity(payload.len() + 1);
    input.push(kind as u8);
    input.extend_from_slice(&payload);
    input
}

fn mutate_image(kind: ImageKind, bytes: &mut Vec<u8>, rng: &mut Rng, intensity: usize) {
    let rounds = rng.range(1, intensity.max(1));
    match kind {
        ImageKind::Main => mutate_main(bytes, rng, rounds),
        ImageKind::Wal => mutate_wal(bytes, rng, rounds),
        ImageKind::Pack => mutate_pack(bytes, rng, rounds),
    }
}

fn mutate_main(bytes: &mut Vec<u8>, rng: &mut Rng, rounds: usize) {
    match rng.index(4) {
        0 => mutate_superblocks(bytes, rng),
        1 => mutate_checked_payload(bytes, rng),
        2 => mutate_directory(bytes, rng),
        _ => mutate_bytes(bytes, rng, rounds, MAX_CONTAINER_INPUT_BYTES),
    }
    if rounds > 8 && rng.index(2) == 0 {
        mutate_bytes(bytes, rng, rounds / 4, MAX_CONTAINER_INPUT_BYTES);
    }
}

fn mutate_superblocks(bytes: &mut [u8], rng: &mut Rng) {
    // Repaired LSN/root mutations can expose another valid catalog, including
    // the original empty slot. Schema-aware probes cover these outcomes too.
    // Generic non-repaired mutations still cover every header byte.
    const OFFSETS: [usize; 28] = [
        0, 1, 2, 3, 4, 5, 6, 7, 24, 25, 26, 27, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56,
        57, 58, 59,
    ];
    let field = OFFSETS[rng.index(OFFSETS.len())];
    for slot in 0..2 {
        let start = slot * PAGE_SIZE;
        let Some(header) = bytes.get_mut(start..start + 64) else {
            return;
        };
        header[field] ^= 1 << rng.index(8);
        let checksum = crc32c(&header[..60]);
        header[60..64].copy_from_slice(&checksum.to_le_bytes());
    }
}

fn mutate_directory(bytes: &mut [u8], rng: &mut Rng) {
    let directories = directory_pages(bytes);
    let Some(page) = directories.get(rng.index(directories.len())).copied() else {
        return;
    };
    let start = page * PAGE_SIZE;
    let end = (start + 128).min(bytes.len());
    if start < end {
        let index = rng.range(start, end - 1);
        bytes[index] ^= 1 << rng.index(8);
    }
}

fn mutate_checked_payload(bytes: &mut [u8], rng: &mut Rng) {
    let entries = payload_entries(bytes);
    let Some(entry) = entries.get(rng.index(entries.len())).copied() else {
        return;
    };
    if entry.length == 0 || entry.start >= bytes.len() {
        return;
    }
    let end = entry.start.saturating_add(entry.length).min(bytes.len());
    if entry.start >= end {
        return;
    }
    let index = rng.range(entry.start, end - 1);
    bytes[index] ^= 1 << rng.index(8);
    let checksum = crc32c(&bytes[entry.start..end]);
    if let Some(destination) = bytes.get_mut(entry.crc_offset..entry.crc_offset + 4) {
        destination.copy_from_slice(&checksum.to_le_bytes());
    }
}

#[derive(Clone, Copy)]
struct PayloadEntry {
    start: usize,
    length: usize,
    crc_offset: usize,
}

fn directory_pages(bytes: &[u8]) -> Vec<usize> {
    bytes
        .as_chunks::<PAGE_SIZE>()
        .0
        .iter()
        .enumerate()
        .skip(2)
        .filter_map(|(page, bytes)| {
            (&bytes[..4] == b"NGRP" || &bytes[..4] == b"RCSR").then_some(page)
        })
        .collect()
}

fn payload_entries(bytes: &[u8]) -> Vec<PayloadEntry> {
    let mut entries = Vec::new();
    for page in directory_pages(bytes) {
        append_page_entries(bytes, page, &mut entries);
    }
    entries
}

fn append_page_entries(bytes: &[u8], page: usize, entries: &mut Vec<PayloadEntry>) {
    let start = page * PAGE_SIZE;
    let Some(directory) = bytes.get(start..start + PAGE_SIZE) else {
        return;
    };
    let count = if &directory[..4] == b"NGRP" {
        read_u32(directory, 8).unwrap_or(0) as usize
    } else {
        2_usize.saturating_add(read_u32(directory, 12).unwrap_or(0) as usize)
    };
    if count > 256 || 16_usize.saturating_add(count.saturating_mul(16)) > PAGE_SIZE {
        return;
    }
    for index in 0..count {
        let offset = 16 + index * 16;
        let first_page = read_u64(directory, offset).unwrap_or(u64::MAX);
        let length = read_u32(directory, offset + 8).unwrap_or(0) as usize;
        let Ok(first_page) = usize::try_from(first_page) else {
            continue;
        };
        let payload_start = first_page.saturating_mul(PAGE_SIZE);
        if payload_start.saturating_add(length) <= bytes.len() {
            entries.push(PayloadEntry {
                start: payload_start,
                length,
                crc_offset: start + offset + 12,
            });
        }
    }
}

fn mutate_wal(bytes: &mut Vec<u8>, rng: &mut Rng, rounds: usize) {
    mutate_bytes(bytes, rng, rounds, MAX_CONTAINER_INPUT_BYTES);
    if rng.index(2) == 0 {
        repair_wal_checksums(bytes);
    }
}

fn repair_wal_checksums(bytes: &mut [u8]) {
    let mut cursor = 0;
    while let Some(header) = bytes.get(cursor..cursor + 16) {
        let length = read_u32(header, 0).unwrap_or(u32::MAX) as usize;
        let end = cursor.saturating_add(16).saturating_add(length);
        if end > bytes.len() {
            return;
        }
        let checksum = crc32c(&bytes[cursor + 8..end]);
        bytes[cursor + 4..cursor + 8].copy_from_slice(&checksum.to_le_bytes());
        cursor = end;
    }
}

fn mutate_pack(bytes: &mut Vec<u8>, rng: &mut Rng, rounds: usize) {
    if rng.index(2) == 0 && mutate_pack_frame(bytes, rng) {
        return;
    }
    mutate_bytes(bytes, rng, rounds, MAX_CONTAINER_INPUT_BYTES);
}

fn mutate_pack_frame(bytes: &mut [u8], rng: &mut Rng) -> bool {
    let Some(frame_count) = read_u64(bytes, 28).and_then(|value| usize::try_from(value).ok())
    else {
        return false;
    };
    if frame_count == 0 || frame_count > 4096 {
        return false;
    }
    let frame = rng.index(frame_count);
    let entry = PACK_HEADER_LEN + frame * PACK_DIRECTORY_ENTRY_LEN;
    let Some(offset) = read_u64(bytes, entry).and_then(|value| usize::try_from(value).ok()) else {
        return false;
    };
    let Some(length) = read_u32(bytes, entry + 8).map(|value| value as usize) else {
        return false;
    };
    let end = offset.saturating_add(length);
    if length == 0 || end > bytes.len() {
        return false;
    }
    let index = rng.range(offset, end - 1);
    bytes[index] ^= 1 << rng.index(8);
    let checksum = crc32c(&bytes[offset..end]);
    bytes[entry + 12..entry + 16].copy_from_slice(&checksum.to_le_bytes());
    repair_pack_directory_crc(bytes, frame_count);
    true
}

fn repair_pack_directory_crc(bytes: &mut [u8], frame_count: usize) {
    let end = PACK_HEADER_LEN.saturating_add(frame_count.saturating_mul(PACK_DIRECTORY_ENTRY_LEN));
    if end.saturating_add(4) > bytes.len() {
        return;
    }
    let checksum = crc32c(&bytes[PACK_HEADER_LEN..end]);
    bytes[end..end + 4].copy_from_slice(&checksum.to_le_bytes());
}

/// Opens the selected image and forces node, encoding, CSR, and pack reads.
pub(crate) fn exercise(input: &[u8], scratch: &Path, fixture_root: &Path) -> Result<(), String> {
    if input.len() > MAX_CONTAINER_INPUT_BYTES {
        return Err(format!(
            "container input is {} bytes, over the {MAX_CONTAINER_INPUT_BYTES}-byte cap",
            input.len()
        ));
    }
    let kind = ImageKind::from_byte(input.first().copied().unwrap_or(0));
    let payload = input.get(1..).unwrap_or_default();
    let path = scratch.join("container.devondb");
    install_image(kind, payload, &path, fixture_root)?;
    let database = match Database::open_with(
        &path,
        Options {
            memory_limit: 8 * 1024 * 1024,
            ..Options::default()
        },
    ) {
        Ok(database) => database,
        Err(DevonError::Corrupt { context }) => {
            eprintln!("error: corrupt data: {context}");
            return Ok(());
        }
        Err(DevonError::BudgetExceeded { context }) => {
            eprintln!("error: bounded open refused: {context}");
            return Ok(());
        }
        Err(error) => {
            return Err(format!("container open returned unexpected error: {error}"));
        }
    };
    probe(database)
}

fn install_image(
    kind: ImageKind,
    payload: &[u8],
    path: &Path,
    fixture_root: &Path,
) -> Result<(), String> {
    match kind {
        ImageKind::Main | ImageKind::Pack => {
            fs::write(path, payload).map_err(|error| format!("write container image: {error}"))
        }
        ImageKind::Wal => {
            let base = read(&fixture_root.join("wal-base.devondb"))?;
            fs::write(path, base).map_err(|error| format!("write WAL base main: {error}"))?;
            fs::write(sidecar_path(path), payload)
                .map_err(|error| format!("write mutated WAL: {error}"))
        }
    }
}

fn probe(mut database: Database) -> Result<(), String> {
    // Corrupting the newest superblock can expose a valid older empty catalog.
    // Probe the recovered schema, never assume the original fixture survived.
    let schema = database.schema_summary();
    let nodes = schema
        .node_tables
        .iter()
        .map(|table| format!("nodes({}) as p", quote_identifier(&table.name)));
    let relationships = schema.rel_tables.iter().flat_map(|table| {
        [(&table.from, "out"), (&table.to, "in")].map(|(node, direction)| {
            format!(
                "nodes({}) as p | expand_rel {} {direction} as c via e",
                quote_identifier(node),
                quote_identifier(&table.name)
            )
        })
    });
    for text in nodes.chain(relationships) {
        let plan = match parse(&text) {
            Ok(Parsed::Query(plan)) => plan,
            Ok(Parsed::Statement(_)) => return Err("probe parsed as statement".to_owned()),
            Err(error) => return Err(format!("probe parse failed: {error}")),
        };
        match database.run(&plan) {
            Ok(_) => {}
            Err(DevonError::Corrupt { context }) => {
                eprintln!("error: corrupt data: {context}");
                return Ok(());
            }
            Err(DevonError::BudgetExceeded { context }) => {
                // Positive row counts larger than the writer's group capacity
                // are legal. Budget preflight can refuse before payload decode.
                eprintln!("error: bounded query refused: {context}");
            }
            Err(error) => {
                return Err(format!(
                    "container query returned unexpected error: {error}"
                ));
            }
        }
    }
    Ok(())
}

fn quote_identifier(value: &str) -> String {
    format!("`{}`", value.replace('\\', "\\\\").replace('`', "\\`"))
}

fn sidecar_path(path: &Path) -> std::path::PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push("-wal");
    value.into()
}

fn read(path: &Path) -> Result<Vec<u8>, String> {
    fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let raw: [u8; 4] = bytes.get(offset..offset + 4)?.try_into().ok()?;
    Some(u32::from_le_bytes(raw))
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let raw: [u8; 8] = bytes.get(offset..offset + 8)?.try_into().ok()?;
    Some(u64::from_le_bytes(raw))
}

fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0_u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0x82f6_3b78 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new() -> Self {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "devondb-container-fixture-{}-{stamp}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn captured_wal_recovers_an_insert_absent_from_its_checkpoint_base() {
        let scratch = Scratch::new();
        let fixtures = build_fixtures(&scratch.0.join("fixtures")).unwrap();
        assert!(!fixtures.wal.is_empty());
        let path = scratch.0.join("replay.devondb");
        fs::copy(fixtures.root.join("wal-base.devondb"), &path).unwrap();
        let Parsed::Query(plan) = parse("nodes(Person) as p").unwrap() else {
            panic!("query")
        };
        let mut base = Database::open(&path).unwrap();
        assert!(base.run(&plan).unwrap().rows.is_empty());
        drop(base);
        fs::write(sidecar_path(&path), &fixtures.wal).unwrap();
        let mut replay = Database::open(&path).unwrap();
        let rows = replay.run(&plan).unwrap().rows;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], devondb::Value::Int64(999));
    }

    #[test]
    fn probes_follow_empty_and_quoted_recovered_schemas() {
        let scratch = Scratch::new();
        let path = scratch.0.join("schema.devondb");
        probe(Database::create(&path, PAGE_SIZE as u32).unwrap()).unwrap();
        let mut database = Database::open(&path).unwrap();
        let name = quote_identifier("a`\\\n table");
        execute_text(
            &mut database,
            &format!("create node table {name} (id Int64 primary key, body String)"),
        )
        .unwrap();
        execute_text(
            &mut database,
            &format!("insert into {name} values (1, \"payload\")"),
        )
        .unwrap();
        database.checkpoint().unwrap();
        probe(database).unwrap();
    }
}
