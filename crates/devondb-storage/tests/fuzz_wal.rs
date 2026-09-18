use std::fs;
use std::path::{Path, PathBuf};

use crc32c::{crc32c, crc32c_append};
use devondb_storage::wal::{WalReader, WalWriter, replay};
use proptest::collection;
use proptest::prelude::*;
use proptest::test_runner::RngSeed;
use tempfile::tempdir;

const PROPTEST_SEED: u64 = 0x4456_4e57_414c_3030;
const WAL_HEADER_LEN: usize = 16;
const WAL_CRC_OFFSET: usize = 4;
const WAL_CRC_LEN: usize = 4;
const MAX_ARBITRARY_WAL_BYTES: usize = 256 * 1024;

fn corrupt_envelope() -> BoxedStrategy<Vec<u8>> {
    (any::<u64>(), collection::vec(any::<u8>(), 0..=4096))
        .prop_map(|(lsn, payload)| {
            let lsn_bytes = lsn.to_le_bytes();
            let checksum = crc32c_append(crc32c(&lsn_bytes), &payload);
            let mut bytes = vec![0_u8; WAL_HEADER_LEN + payload.len()];
            bytes[..4].copy_from_slice(&(payload.len() as u32).to_le_bytes());
            bytes[4..8].copy_from_slice(&(checksum ^ 1).to_le_bytes());
            bytes[8..WAL_HEADER_LEN].copy_from_slice(&lsn_bytes);
            bytes[WAL_HEADER_LEN..].copy_from_slice(&payload);
            bytes
        })
        .boxed()
}

fn declared_giant_envelope() -> BoxedStrategy<Vec<u8>> {
    (
        any::<u32>(),
        any::<u64>(),
        collection::vec(any::<u8>(), 0..=1024),
    )
        .prop_map(|(crc, lsn, body)| {
            let mut bytes = Vec::with_capacity(WAL_HEADER_LEN + body.len());
            bytes.extend_from_slice(&u32::MAX.to_le_bytes());
            bytes.extend_from_slice(&crc.to_le_bytes());
            bytes.extend_from_slice(&lsn.to_le_bytes());
            bytes.extend_from_slice(&body);
            bytes
        })
        .boxed()
}

fn arbitrary_wal_contents() -> BoxedStrategy<Vec<u8>> {
    let empty = Just(Vec::new());
    let sub_header = collection::vec(any::<u8>(), 0..WAL_HEADER_LEN);
    let high_entropy = collection::vec(any::<u8>(), 0..=8192);
    let large = collection::vec(any::<u8>(), 64 * 1024..=MAX_ARBITRARY_WAL_BYTES);

    prop_oneof![
        1 => empty,
        2 => sub_header,
        4 => high_entropy,
        1 => large,
        2 => declared_giant_envelope(),
        2 => corrupt_envelope(),
    ]
    .boxed()
}

fn exercise_public_decode_paths(bytes: &[u8]) {
    let directory = tempdir().unwrap();
    let path = directory.path().join("arbitrary.devondb-wal");
    fs::write(&path, bytes).unwrap();

    let _replay_outcome = replay(&path);
    if let Ok(reader) = WalReader::open(&path) {
        let _reader_outcome = reader.collect::<Vec<_>>();
    }
    let _writer_open_outcome = WalWriter::open(&path, 0);
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 128,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(PROPTEST_SEED),
        ..ProptestConfig::default()
    })]

    #[test]
    fn arbitrary_wal_bytes_never_panic(bytes in arbitrary_wal_contents()) {
        exercise_public_decode_paths(&bytes);
    }
}

#[test]
fn megabyte_declared_giant_wal_never_panics_or_allocates_from_its_length() {
    let mut bytes = vec![0_u8; 1024 * 1024];
    let mut state = 0xd1b5_4a32_d192_ed03_u64;
    for byte in &mut bytes {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *byte = state as u8;
    }
    bytes[..4].copy_from_slice(&u32::MAX.to_le_bytes());
    exercise_public_decode_paths(&bytes);
}

#[derive(Clone, Debug)]
struct TailCase {
    prefix_payloads: Vec<Vec<u8>>,
    tail_payload: Vec<u8>,
    first_lsn: u64,
}

fn tail_case() -> BoxedStrategy<TailCase> {
    (
        collection::vec(collection::vec(any::<u8>(), 0..=128), 0..=4),
        collection::vec(any::<u8>(), 0..=256),
        0_u64..=1_000_000,
    )
        .prop_map(|(prefix_payloads, tail_payload, first_lsn)| TailCase {
            prefix_payloads,
            tail_payload,
            first_lsn,
        })
        .boxed()
}

#[derive(Debug)]
struct WrittenTail {
    bytes: Vec<u8>,
    prefix_len: usize,
    prefix_records: Vec<(u64, Vec<u8>)>,
    all_records: Vec<(u64, Vec<u8>)>,
}

impl WrittenTail {
    fn tail_record_len(&self) -> usize {
        self.bytes.len() - self.prefix_len
    }
}

fn write_tail_case(path: &Path, case: &TailCase) -> WrittenTail {
    let mut writer = WalWriter::open(path, case.first_lsn).unwrap();
    let mut prefix_records = Vec::with_capacity(case.prefix_payloads.len());
    for payload in &case.prefix_payloads {
        let lsn = writer.append(payload).unwrap();
        prefix_records.push((lsn, payload.clone()));
    }
    writer.sync().unwrap();
    let prefix_len = fs::metadata(path).unwrap().len() as usize;

    let tail_lsn = writer.append(&case.tail_payload).unwrap();
    writer.sync().unwrap();
    drop(writer);

    let mut all_records = prefix_records.clone();
    all_records.push((tail_lsn, case.tail_payload.clone()));
    WrittenTail {
        bytes: fs::read(path).unwrap(),
        prefix_len,
        prefix_records,
        all_records,
    }
}

fn replay_and_repair(path: &Path, bytes: &[u8], next_lsn: u64) -> (Vec<(u64, Vec<u8>)>, u64) {
    fs::write(path, bytes).unwrap();
    let replayed = replay(path).unwrap();
    let writer = WalWriter::open(path, next_lsn).unwrap();
    drop(writer);
    let repaired_len = fs::metadata(path).unwrap().len();
    (replayed, repaired_len)
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 48,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(PROPTEST_SEED ^ 0x544f_524e_5441_494c),
        ..ProptestConfig::default()
    })]

    #[test]
    fn every_tail_byte_boundary_recovers_exactly_the_known_good_prefix(case in tail_case()) {
        let directory = tempdir().unwrap();
        let path = directory.path().join("torn-tail.devondb-wal");
        let written = write_tail_case(&path, &case);

        for retained_tail_bytes in 0..=written.tail_record_len() {
            let retained_len = written.prefix_len + retained_tail_bytes;
            let is_complete = retained_tail_bytes == written.tail_record_len();
            let expected = if is_complete {
                &written.all_records
            } else {
                &written.prefix_records
            };
            let expected_repaired_len = if is_complete {
                written.bytes.len()
            } else {
                written.prefix_len
            };
            let (replayed, repaired_len) = replay_and_repair(
                &path,
                &written.bytes[..retained_len],
                case.first_lsn,
            );

            prop_assert_eq!(
                &replayed,
                expected,
                "retaining {} of {} tail bytes",
                retained_tail_bytes,
                written.tail_record_len(),
            );
            prop_assert_eq!(replayed.len(), expected.len());
            prop_assert_eq!(repaired_len as usize, expected_repaired_len);
            prop_assert_eq!(replay(&path).unwrap(), expected.clone());
        }

        for crc_byte in 0..WAL_CRC_LEN {
            let mut corrupted = written.bytes.clone();
            corrupted[written.prefix_len + WAL_CRC_OFFSET + crc_byte] ^= 0xff;
            let (replayed, repaired_len) =
                replay_and_repair(&path, &corrupted, case.first_lsn);

            prop_assert_eq!(
                &replayed,
                &written.prefix_records,
                "CRC byte {}",
                crc_byte,
            );
            prop_assert_eq!(replayed.len(), written.prefix_records.len());
            prop_assert_eq!(repaired_len as usize, written.prefix_len);
            prop_assert_eq!(replay(&path).unwrap(), written.prefix_records.clone());
        }
    }
}

#[derive(Clone, Debug)]
struct MidFileCase {
    before: Vec<Vec<u8>>,
    victim: Vec<u8>,
    after: Vec<Vec<u8>>,
    victim_byte: usize,
    flip_mask: u8,
    first_lsn: u64,
}

fn mid_file_case() -> BoxedStrategy<MidFileCase> {
    (
        collection::vec(collection::vec(any::<u8>(), 1..=128), 1..=3),
        collection::vec(any::<u8>(), 1..=128),
        collection::vec(collection::vec(any::<u8>(), 1..=128), 1..=3),
        any::<usize>(),
        1_u8..=u8::MAX,
        0_u64..=1_000_000,
    )
        .prop_map(
            |(before, victim, after, victim_byte, flip_mask, first_lsn)| MidFileCase {
                before,
                victim,
                after,
                victim_byte,
                flip_mask,
                first_lsn,
            },
        )
        .boxed()
}

fn append_payloads(
    writer: &mut WalWriter,
    payloads: &[Vec<u8>],
    records: &mut Vec<(u64, Vec<u8>)>,
) {
    for payload in payloads {
        let lsn = writer.append(payload).unwrap();
        records.push((lsn, payload.clone()));
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 96,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(PROPTEST_SEED ^ 0x4d49_4446_494c_4500),
        ..ProptestConfig::default()
    })]

    #[test]
    fn mid_file_crc_corruption_truncates_to_the_known_good_prefix(case in mid_file_case()) {
        let directory = tempdir().unwrap();
        let path: PathBuf = directory.path().join("mid-file.devondb-wal");
        let mut writer = WalWriter::open(&path, case.first_lsn).unwrap();
        let mut before_records = Vec::with_capacity(case.before.len());
        append_payloads(&mut writer, &case.before, &mut before_records);
        writer.sync().unwrap();
        let known_good_len = fs::metadata(&path).unwrap().len() as usize;

        let victim_lsn = writer.append(&case.victim).unwrap();
        let mut all_records = before_records.clone();
        all_records.push((victim_lsn, case.victim.clone()));
        append_payloads(&mut writer, &case.after, &mut all_records);
        writer.sync().unwrap();
        drop(writer);
        prop_assert_eq!(replay(&path).unwrap(), all_records);

        let mut corrupted = fs::read(&path).unwrap();
        let victim_byte = case.victim_byte % case.victim.len();
        corrupted[known_good_len + WAL_HEADER_LEN + victim_byte] ^= case.flip_mask;
        fs::write(&path, corrupted).unwrap();

        prop_assert_eq!(replay(&path).unwrap(), before_records.clone());
        let mut repaired = WalWriter::open(&path, case.first_lsn).unwrap();
        prop_assert_eq!(fs::metadata(&path).unwrap().len() as usize, known_good_len);

        let replacement = b"replacement after corruption";
        let replacement_lsn = repaired.append(replacement).unwrap();
        prop_assert_eq!(replacement_lsn, case.first_lsn + case.before.len() as u64);
        repaired.sync().unwrap();
        drop(repaired);
        before_records.push((replacement_lsn, replacement.to_vec()));
        prop_assert_eq!(replay(&path).unwrap(), before_records);
    }
}
