//! Write-ahead log: append, iterate, replay, torn-tail handling.
//!
//! Record layout is specified in `docs/FORMAT.md` § WAL sidecar and is
//! binding.

use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crc32c::{crc32c, crc32c_append};
use devondb_types::{DevonError, DevonResult};

const HEADER_LEN: usize = 16;

/// One intact WAL record as its assigned LSN and owned payload bytes.
pub type WalRecord = (u64, Vec<u8>);

/// Records parsed from a byte cursor and the following intact-prefix cursor.
pub type WalCursorReplay = (Vec<WalRecord>, u64);

/// An append-only writer for a devondb WAL sidecar.
pub struct WalWriter {
    file: File,
    log_end: u64,
    next_lsn: Option<u64>,
    poison_reason: Option<String>,
}

impl WalWriter {
    /// Opens or creates `path`, repairing its tail before appending records.
    ///
    /// The first assigned LSN is at least `next_lsn` and is also strictly
    /// greater than the last intact record already in the WAL.
    pub fn open(path: impl AsRef<Path>, next_lsn: u64) -> DevonResult<Self> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        let (mut file, log_end, last_lsn) = scan_intact_prefix(file)?;
        if file.metadata()?.len() != log_end {
            file.set_len(log_end)?;
        }
        file.seek(SeekFrom::Start(log_end))?;

        Ok(Self {
            file,
            log_end,
            next_lsn: next_lsn_after(last_lsn, next_lsn),
            poison_reason: None,
        })
    }

    /// Appends one record and returns its assigned log sequence number.
    pub fn append(&mut self, payload: &[u8]) -> DevonResult<u64> {
        self.ensure_usable()?;
        let lsn = self.next_lsn.ok_or_else(|| DevonError::InvalidArgument {
            context: "WAL LSN space is exhausted".to_owned(),
        })?;
        let header = encode_header(lsn, payload)?;
        let record_len = (HEADER_LEN as u64) + payload.len() as u64;
        let new_log_end =
            self.log_end
                .checked_add(record_len)
                .ok_or_else(|| DevonError::InvalidArgument {
                    context: "WAL file length exceeds u64::MAX".to_owned(),
                })?;

        self.file.seek(SeekFrom::Start(self.log_end))?;
        if let Err(error) = self
            .file
            .write_all(&header)
            .and_then(|()| self.file.write_all(payload))
        {
            return Err(self.recover_failed_append(error));
        }

        self.log_end = new_log_end;
        self.next_lsn = lsn.checked_add(1);
        Ok(lsn)
    }

    /// Flushes all WAL contents and file metadata to durable storage.
    pub fn sync(&mut self) -> DevonResult<()> {
        self.file.sync_all()?;
        Ok(())
    }

    fn ensure_usable(&self) -> DevonResult<()> {
        if let Some(context) = &self.poison_reason {
            return Err(DevonError::Corrupt {
                context: context.clone(),
            });
        }
        Ok(())
    }

    fn recover_failed_append(&mut self, write_error: io::Error) -> DevonError {
        match self.file.set_len(self.log_end) {
            Ok(()) => write_error.into(),
            Err(truncate_error) => {
                let context = format!(
                    "WAL writer poisoned: append failed ({write_error}) and truncating the torn tail to byte {} failed ({truncate_error})",
                    self.log_end
                );
                self.poison_reason = Some(context.clone());
                DevonError::Corrupt { context }
            }
        }
    }
}

/// A forward-only reader over intact records in a devondb WAL sidecar.
pub struct WalReader {
    file: File,
    last_lsn: Option<u64>,
    stopped: bool,
}

impl WalReader {
    /// Opens `path` and positions the reader at its first WAL record.
    pub fn open(path: impl AsRef<Path>) -> DevonResult<Self> {
        Ok(Self::from_file(File::open(path)?))
    }

    fn from_file(file: File) -> Self {
        Self {
            file,
            last_lsn: None,
            stopped: false,
        }
    }

    fn read_next(&mut self) -> DevonResult<Option<(u64, Vec<u8>)>> {
        let mut header = [0_u8; HEADER_LEN];
        if !read_complete(&mut self.file, &mut header)? {
            return Ok(None);
        }

        let (payload_len_bytes, rest) = header.split_at(4);
        let (crc_bytes, lsn_slice) = rest.split_at(4);
        let payload_len = u32::from_le_bytes([
            payload_len_bytes[0],
            payload_len_bytes[1],
            payload_len_bytes[2],
            payload_len_bytes[3],
        ]) as usize;
        let expected_crc =
            u32::from_le_bytes([crc_bytes[0], crc_bytes[1], crc_bytes[2], crc_bytes[3]]);
        let lsn_bytes = [
            lsn_slice[0],
            lsn_slice[1],
            lsn_slice[2],
            lsn_slice[3],
            lsn_slice[4],
            lsn_slice[5],
            lsn_slice[6],
            lsn_slice[7],
        ];
        let lsn = u64::from_le_bytes(lsn_bytes);
        let Some(payload) = read_payload(&mut self.file, payload_len)? else {
            return Ok(None);
        };
        let actual_crc = crc32c_append(crc32c(&lsn_bytes), &payload);
        if actual_crc != expected_crc || self.last_lsn.is_some_and(|last| lsn <= last) {
            return Ok(None);
        }

        self.last_lsn = Some(lsn);
        Ok(Some((lsn, payload)))
    }
}

impl Iterator for WalReader {
    type Item = DevonResult<WalRecord>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.stopped {
            return None;
        }

        match self.read_next() {
            Ok(Some(record)) => Some(Ok(record)),
            Ok(None) => {
                self.stopped = true;
                None
            }
            Err(error) => {
                self.stopped = true;
                Some(Err(error))
            }
        }
    }
}

/// Replays every intact WAL record from `path` in file order.
pub fn replay(path: impl AsRef<Path>) -> DevonResult<Vec<WalRecord>> {
    WalReader::open(path)?.collect()
}

/// Replays intact WAL records beginning at the byte-aligned record cursor.
///
/// The returned cursor is the end of the last intact record, or `offset` when
/// no complete record follows it. A truncated record or invalid checksum is a
/// normal tail boundary and is therefore not returned as an error.
pub fn replay_from(path: impl AsRef<Path>, offset: u64) -> DevonResult<WalCursorReplay> {
    let mut file = File::open(path)?;
    let file_len = file.metadata()?.len();
    if offset > file_len {
        return Err(DevonError::InvalidArgument {
            context: format!("WAL replay offset {offset} is beyond the file length {file_len}"),
        });
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut reader = WalReader::from_file(file);
    let mut records = Vec::new();
    let mut new_offset = offset;
    while let Some(record) = reader.read_next()? {
        records.push(record);
        new_offset = reader.file.stream_position()?;
    }
    Ok((records, new_offset))
}

fn encode_header(lsn: u64, payload: &[u8]) -> DevonResult<[u8; HEADER_LEN]> {
    let payload_len = u32::try_from(payload.len()).map_err(|_| DevonError::InvalidArgument {
        context: "WAL payload length exceeds u32::MAX".to_owned(),
    })?;
    let lsn_bytes = lsn.to_le_bytes();
    let checksum = crc32c_append(crc32c(&lsn_bytes), payload);
    let mut header = [0_u8; HEADER_LEN];
    header[..4].copy_from_slice(&payload_len.to_le_bytes());
    header[4..8].copy_from_slice(&checksum.to_le_bytes());
    header[8..].copy_from_slice(&lsn_bytes);
    Ok(header)
}

fn next_lsn_after(last_lsn: Option<u64>, requested: u64) -> Option<u64> {
    match last_lsn {
        Some(last) => last.checked_add(1).map(|next| requested.max(next)),
        None => Some(requested),
    }
}

fn scan_intact_prefix(file: File) -> DevonResult<(File, u64, Option<u64>)> {
    let mut reader = WalReader::from_file(file);
    let mut log_end = 0;
    while reader.read_next()?.is_some() {
        log_end = reader.file.stream_position()?;
    }
    let last_lsn = reader.last_lsn;
    Ok((reader.file, log_end, last_lsn))
}

fn read_complete(file: &mut File, buffer: &mut [u8]) -> io::Result<bool> {
    match file.read_exact(buffer) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == ErrorKind::UnexpectedEof => Ok(false),
        Err(error) => Err(error),
    }
}

fn read_payload(file: &mut File, payload_len: usize) -> io::Result<Option<Vec<u8>>> {
    let position = file.stream_position()?;
    let remaining = file.metadata()?.len().saturating_sub(position);
    if remaining < payload_len as u64 {
        return Ok(None);
    }

    let mut payload = Vec::new();
    payload
        .try_reserve_exact(payload_len)
        .map_err(|error| io::Error::other(format!("cannot allocate WAL payload: {error}")))?;
    payload.resize(payload_len, 0);
    if read_complete(file, &mut payload)? {
        Ok(Some(payload))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::io::{Read, Seek, SeekFrom, Write};

    use tempfile::tempdir;

    use super::{HEADER_LEN, WalWriter, encode_header, replay, replay_from};

    #[test]
    fn append_then_replay_returns_all_records_in_order() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("database.devondb-wal");
        let payloads = [
            b"first".as_slice(),
            b"second".as_slice(),
            b"third".as_slice(),
        ];
        let mut writer = WalWriter::open(&path, 41).unwrap();

        for payload in payloads {
            writer.append(payload).unwrap();
        }
        writer.sync().unwrap();
        drop(writer);

        let records = replay(&path).unwrap();
        assert_eq!(
            records,
            vec![
                (41, b"first".to_vec()),
                (42, b"second".to_vec()),
                (43, b"third".to_vec()),
            ]
        );
    }

    #[test]
    fn truncated_final_record_is_silently_discarded() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("database.devondb-wal");
        let mut writer = WalWriter::open(&path, 7).unwrap();
        writer.append(b"complete one").unwrap();
        writer.append(b"complete two").unwrap();
        writer.append(b"torn tail").unwrap();
        writer.sync().unwrap();
        drop(writer);

        let file = OpenOptions::new().write(true).open(&path).unwrap();
        let truncated_len = file.metadata().unwrap().len() - 3;
        file.set_len(truncated_len).unwrap();
        drop(file);

        assert_eq!(
            replay(&path).unwrap(),
            vec![(7, b"complete one".to_vec()), (8, b"complete two".to_vec())]
        );
    }

    #[test]
    fn corrupt_payload_stops_replay_at_that_record() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("database.devondb-wal");
        let first = b"intact";
        let second = b"corrupt me";
        let mut writer = WalWriter::open(&path, 100).unwrap();
        writer.append(first).unwrap();
        writer.append(second).unwrap();
        writer.append(b"valid but unreachable").unwrap();
        writer.sync().unwrap();
        drop(writer);

        let second_payload_offset = (HEADER_LEN + first.len() + HEADER_LEN) as u64;
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        file.seek(SeekFrom::Start(second_payload_offset)).unwrap();
        let mut byte = [0_u8; 1];
        file.read_exact(&mut byte).unwrap();
        byte[0] ^= 0xff;
        file.seek(SeekFrom::Start(second_payload_offset)).unwrap();
        file.write_all(&byte).unwrap();
        file.sync_all().unwrap();
        drop(file);

        assert_eq!(replay(&path).unwrap(), vec![(100, first.to_vec())]);
    }

    #[test]
    fn append_assigns_strictly_increasing_lsns() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("database.devondb-wal");
        let mut writer = WalWriter::open(path, 500).unwrap();

        assert_eq!(writer.append(b"one").unwrap(), 500);
        assert_eq!(writer.append(b"two").unwrap(), 501);
        assert_eq!(writer.append(b"three").unwrap(), 502);
    }

    #[test]
    fn replay_from_returns_only_records_after_the_cursor() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("database.devondb-wal");
        let mut writer = WalWriter::open(&path, 50).unwrap();
        writer.append(b"first").unwrap();
        writer.append(b"second").unwrap();
        writer.append(b"third").unwrap();
        writer.sync().unwrap();
        drop(writer);

        let first_end = u64::try_from(HEADER_LEN + b"first".len()).unwrap();
        let (records, cursor) = replay_from(&path, first_end).unwrap();

        assert_eq!(
            records,
            vec![(51, b"second".to_vec()), (52, b"third".to_vec())]
        );
        assert_eq!(cursor, path.metadata().unwrap().len());
    }

    #[test]
    fn replay_from_leaves_cursor_before_a_torn_tail() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("database.devondb-wal");
        let mut writer = WalWriter::open(&path, 80).unwrap();
        writer.append(b"complete").unwrap();
        writer.sync().unwrap();
        drop(writer);
        let intact_end = path.metadata().unwrap().len();
        append_torn_record(&path, 81, b"incomplete", 3);

        let (records, cursor) = replay_from(&path, intact_end).unwrap();

        assert!(records.is_empty());
        assert_eq!(cursor, intact_end);
    }

    #[test]
    fn failed_append_then_retry_does_not_strand_retried_record() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("database.devondb-wal");
        let mut writer = WalWriter::open(&path, 10).unwrap();
        writer.append(b"before failure").unwrap();
        writer.sync().unwrap();
        drop(writer);
        append_torn_record(&path, 11, b"failed append", 4);

        let mut writer = WalWriter::open(&path, 11).unwrap();
        assert_eq!(writer.append(b"retried append").unwrap(), 11);
        assert_eq!(writer.append(b"later append").unwrap(), 12);
        writer.sync().unwrap();
        drop(writer);

        assert_eq!(
            replay(&path).unwrap(),
            vec![
                (10, b"before failure".to_vec()),
                (11, b"retried append".to_vec()),
                (12, b"later append".to_vec()),
            ]
        );
    }

    #[test]
    fn open_truncates_torn_tail_to_intact_prefix() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("database.devondb-wal");
        let mut writer = WalWriter::open(&path, 20).unwrap();
        writer.append(b"first").unwrap();
        writer.append(b"second").unwrap();
        writer.sync().unwrap();
        drop(writer);
        let intact_len = path.metadata().unwrap().len();
        append_torn_record(&path, 22, b"incomplete", 3);
        assert!(path.metadata().unwrap().len() > intact_len);

        let writer = WalWriter::open(&path, 22).unwrap();
        assert_eq!(path.metadata().unwrap().len(), intact_len);
        drop(writer);

        assert_eq!(
            replay(&path).unwrap(),
            vec![(20, b"first".to_vec()), (21, b"second".to_vec())]
        );
    }

    #[test]
    fn lsn_continuity_survives_reopen_and_torn_tail_repair() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("database.devondb-wal");
        let mut writer = WalWriter::open(&path, 30).unwrap();
        writer.append(b"thirty").unwrap();
        writer.append(b"thirty-one").unwrap();
        writer.sync().unwrap();
        drop(writer);
        append_torn_record(&path, 32, b"torn thirty-two", 5);

        let mut writer = WalWriter::open(&path, 0).unwrap();
        assert_eq!(writer.append(b"complete thirty-two").unwrap(), 32);
        writer.sync().unwrap();
        drop(writer);

        assert_eq!(
            replay(&path).unwrap(),
            vec![
                (30, b"thirty".to_vec()),
                (31, b"thirty-one".to_vec()),
                (32, b"complete thirty-two".to_vec()),
            ]
        );
    }

    fn append_torn_record(path: &std::path::Path, lsn: u64, payload: &[u8], written: usize) {
        assert!(written < payload.len());
        let header = encode_header(lsn, payload).unwrap();
        let mut file = OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(&header).unwrap();
        file.write_all(&payload[..written]).unwrap();
        file.sync_all().unwrap();
    }
}
