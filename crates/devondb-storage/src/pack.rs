//! The DEVONPACK read-only distribution container (`docs/SCALE.md` §7,
//! BINDING), behind the `pack` cargo feature.
//!
//! A pack holds every main-file page of a checkpointed database,
//! zstd-compressed in fixed runs of `frame_pages` consecutive pages:
//!
//! ```text
//! magic "DEVONPACK" (9 B) · u8 container_version = 1 · u16 codec (1 = zstd)
//! u32 page_size · u64 page_count · u32 frame_pages · u64 frame_count
//! u32 crc32c(header bytes above)
//! frame directory: frame_count × { u64 offset, u32 compressed_len, u32 crc32c(compressed bytes) }
//! u32 crc32c(directory bytes)
//! frames: zstd-compressed runs of frame_pages consecutive pages (last frame short)
//! ```
//!
//! All integers little-endian. The pages inside are byte-identical to the
//! source's, so every reader above the pager seam is unchanged. Every
//! corruption class is a distinct [`DevonError::Corrupt`] naming its region;
//! the frame crc is verified BEFORE decode (`docs/SCALE.md` §7.2).

use std::fs::File;
use std::io;
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::Path;

use devondb_types::{DevonError, DevonResult};

use crate::backend::PagerBackend;
use crate::pager::Pager;

/// The 9-byte file magic recognized at facade open (`docs/SCALE.md` §7.1).
pub const PACK_MAGIC: [u8; 9] = *b"DEVONPACK";
/// The only container version this build reads and writes.
pub const CONTAINER_VERSION: u8 = 1;
/// Codec id 1: zstd at the library's default level (`docs/SCALE.md` §7.3).
pub const CODEC_ZSTD: u16 = 1;
/// Writer policy default: pages per frame (`docs/SCALE.md` §7.1).
pub const DEFAULT_FRAME_PAGES: u32 = 256;
/// Fixed header length: magic · version · codec · page_size · page_count ·
/// frame_pages · frame_count · header crc32c.
pub const HEADER_LEN: usize = 9 + 1 + 2 + 4 + 8 + 4 + 8 + 4;
/// One directory entry: u64 offset · u32 compressed_len · u32 crc32c.
pub const DIRECTORY_ENTRY_LEN: usize = 16;
/// The crc32c trailer appended to the directory region.
pub const DIRECTORY_CRC_LEN: usize = 4;

/// One decoded directory row: where a frame's compressed bytes live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FrameEntry {
    pub(crate) offset: u64,
    pub(crate) compressed_len: u32,
    pub(crate) crc32c: u32,
}

/// An open DEVONPACK container: parsed header and verified directory.
///
/// The reader verifies the directory crc at open and each frame's crc before
/// decode; truncation anywhere is [`DevonError::Corrupt`] with the region
/// named (`docs/SCALE.md` §7.2).
#[derive(Debug)]
pub struct PackFile {
    file: File,
    page_size: u32,
    page_count: u64,
    frame_pages: u32,
    directory: Vec<FrameEntry>,
}

/// Writes every main-file page of `source` — superblock slots included — as
/// one DEVONPACK container (`docs/SCALE.md` §7.1).
///
/// `frame_pages` is writer policy ([`DEFAULT_FRAME_PAGES`] when the caller
/// has no opinion). Compression runs twice per frame — once to size the
/// directory, once to stream the bytes — so peak memory stays at one frame
/// no matter how large the source; zstd single-shot encoding is
/// deterministic, and the second pass verifies it reproduced the first.
pub fn pack_pages(source: &Pager, mut out: impl Write, frame_pages: u32) -> DevonResult<()> {
    if frame_pages == 0 {
        return Err(invalid_argument("frame_pages must be at least 1"));
    }
    let page_size = source.superblock().page_size;
    let byte_len = source.backend().len()?;
    if !byte_len.is_multiple_of(u64::from(page_size)) {
        return Err(corrupt(format!(
            "source length {byte_len} is not page-aligned to page_size {page_size}"
        )));
    }
    let page_count = byte_len / u64::from(page_size);
    let frame_count = page_count.div_ceil(u64::from(frame_pages));
    let mut frame = vec![0_u8; frame_bytes(frame_pages, page_size)];

    // Pass 1: compress every frame to size and checksum the directory.
    let mut directory = Vec::with_capacity(frame_count as usize);
    let mut offset = frames_offset(frame_count);
    for (frame_index, frame_len) in FrameLengths::new(page_count, frame_pages, page_size) {
        read_frame(source, &mut frame, frame_index, frame_len)?;
        let compressed = compress_frame(&frame[..frame_len])?;
        directory.push(FrameEntry {
            offset,
            compressed_len: compressed.len() as u32,
            crc32c: crc32c::crc32c(&compressed),
        });
        offset += compressed.len() as u64;
    }

    write_header(&mut out, page_size, page_count, frame_pages, frame_count)?;
    write_directory(&mut out, &directory)?;

    // Pass 2: re-encode deterministically and stream the frames out.
    for (frame_index, frame_len) in FrameLengths::new(page_count, frame_pages, page_size) {
        read_frame(source, &mut frame, frame_index, frame_len)?;
        let compressed = compress_frame(&frame[..frame_len])?;
        let entry = directory[frame_index as usize];
        if compressed.len() as u32 != entry.compressed_len
            || crc32c::crc32c(&compressed) != entry.crc32c
        {
            return Err(corrupt(format!(
                "pack frame {frame_index} changed between the directory and write passes"
            )));
        }
        out.write_all(&compressed)?;
    }
    Ok(())
}

impl PackFile {
    /// Opens and validates a DEVONPACK container: header, header crc,
    /// directory, directory crc, and every frame's bounds
    /// (`docs/SCALE.md` §7.1/§7.2).
    pub fn open(path: impl AsRef<Path>) -> DevonResult<Self> {
        let file = File::open(path)?;
        let file_len = file.metadata()?.len();
        let header = read_header(&file, file_len)?;
        let directory = read_directory(&file, file_len, header.frame_count)?;
        check_frame_count(header.page_count, header.frame_pages, header.frame_count)?;
        check_frame_bounds(&directory, file_len)?;
        Ok(Self {
            file,
            page_size: header.page_size,
            page_count: header.page_count,
            frame_pages: header.frame_pages,
            directory,
        })
    }

    /// The database page size carried by the container.
    #[must_use]
    pub fn page_size(&self) -> u32 {
        self.page_size
    }

    /// The number of main-file pages in the container.
    #[must_use]
    pub fn page_count(&self) -> u64 {
        self.page_count
    }

    /// Pages per frame (the last frame is short when `page_count` does not
    /// divide evenly).
    #[must_use]
    pub fn frame_pages(&self) -> u32 {
        self.frame_pages
    }

    /// Bytes in one full decoded frame — the read side's fixed working set,
    /// charged to the memory budget once at open (`docs/SCALE.md` §7.2).
    #[must_use]
    pub fn frame_bytes(&self) -> usize {
        frame_bytes(self.frame_pages, self.page_size)
    }

    /// The logical length the pager sees: `page_count × page_size`.
    #[must_use]
    pub fn byte_len(&self) -> u64 {
        self.page_count * u64::from(self.page_size)
    }

    /// Reads, crc-verifies, and decodes one frame, returning exactly the
    /// frame's decoded bytes (short for the last frame).
    pub(crate) fn read_frame(&self, frame_index: u64) -> DevonResult<Vec<u8>> {
        let Some(entry) = self.directory.get(frame_index as usize) else {
            return Err(corrupt(format!(
                "pack frame {frame_index} is beyond the {}-frame directory",
                self.directory.len()
            )));
        };
        let mut compressed = vec![0_u8; entry.compressed_len as usize];
        read_exact_at(&self.file, &mut compressed, entry.offset).map_err(|error| {
            if error.kind() == io::ErrorKind::UnexpectedEof {
                corrupt(format!(
                    "pack frame {frame_index} truncated: short read of {} bytes at offset {}",
                    entry.compressed_len, entry.offset
                ))
            } else {
                DevonError::Io(error)
            }
        })?;
        let computed = crc32c::crc32c(&compressed);
        if computed != entry.crc32c {
            return Err(corrupt(format!(
                "pack frame {frame_index} crc32c mismatch: stored {:#010x}, computed {computed:#010x}",
                entry.crc32c
            )));
        }
        let expected = self.expected_frame_bytes(frame_index);
        let decoded = zstd::bulk::decompress(&compressed, expected).map_err(|error| {
            corrupt(format!(
                "pack frame {frame_index} fails zstd decode: {error}"
            ))
        })?;
        if decoded.len() != expected {
            return Err(corrupt(format!(
                "pack frame {frame_index} decoded to {} bytes, expected {expected}",
                decoded.len()
            )));
        }
        Ok(decoded)
    }

    /// The decoded length of one frame: full except for a short last frame.
    fn expected_frame_bytes(&self, frame_index: u64) -> usize {
        let first_page = frame_index * u64::from(self.frame_pages);
        let pages = u64::from(self.frame_pages).min(self.page_count - first_page);
        (pages * u64::from(self.page_size)) as usize
    }
}

/// The parsed container header, before the directory.
struct PackHeader {
    page_size: u32,
    page_count: u64,
    frame_pages: u32,
    frame_count: u64,
}

/// Yields `(frame_index, decoded byte length)` for every frame, the last
/// one short when `page_count` does not divide by `frame_pages`.
struct FrameLengths {
    page_count: u64,
    frame_pages: u64,
    page_size: u64,
    next: u64,
}

impl FrameLengths {
    fn new(page_count: u64, frame_pages: u32, page_size: u32) -> Self {
        Self {
            page_count,
            frame_pages: u64::from(frame_pages),
            page_size: u64::from(page_size),
            next: 0,
        }
    }
}

impl Iterator for FrameLengths {
    type Item = (u64, usize);

    fn next(&mut self) -> Option<Self::Item> {
        let first_page = self.next * self.frame_pages;
        if first_page >= self.page_count {
            return None;
        }
        let pages = self.frame_pages.min(self.page_count - first_page);
        self.next += 1;
        Some((self.next - 1, (pages * self.page_size) as usize))
    }
}

fn frame_bytes(frame_pages: u32, page_size: u32) -> usize {
    frame_pages as usize * page_size as usize
}

/// Byte offset of the first frame: header + directory + directory crc.
fn frames_offset(frame_count: u64) -> u64 {
    HEADER_LEN as u64 + frame_count * DIRECTORY_ENTRY_LEN as u64 + DIRECTORY_CRC_LEN as u64
}

/// Reads one source frame's pages into `frame[..frame_len]`, superblock
/// pages included (the data-page gate does not apply to container export).
fn read_frame(
    source: &Pager,
    frame: &mut [u8],
    frame_index: u64,
    frame_len: usize,
) -> DevonResult<()> {
    let offset = frame_index * frame.len() as u64;
    source
        .backend()
        .read_exact_at(offset, &mut frame[..frame_len])
}

fn compress_frame(frame: &[u8]) -> DevonResult<Vec<u8>> {
    Ok(zstd::bulk::compress(
        frame,
        zstd::DEFAULT_COMPRESSION_LEVEL,
    )?)
}

fn write_header(
    out: &mut impl Write,
    page_size: u32,
    page_count: u64,
    frame_pages: u32,
    frame_count: u64,
) -> DevonResult<()> {
    let mut header = [0_u8; HEADER_LEN];
    header[0..9].copy_from_slice(&PACK_MAGIC);
    header[9] = CONTAINER_VERSION;
    header[10..12].copy_from_slice(&CODEC_ZSTD.to_le_bytes());
    header[12..16].copy_from_slice(&page_size.to_le_bytes());
    header[16..24].copy_from_slice(&page_count.to_le_bytes());
    header[24..28].copy_from_slice(&frame_pages.to_le_bytes());
    header[28..36].copy_from_slice(&frame_count.to_le_bytes());
    let crc = crc32c::crc32c(&header[..36]);
    header[36..40].copy_from_slice(&crc.to_le_bytes());
    out.write_all(&header)?;
    Ok(())
}

fn write_directory(out: &mut impl Write, directory: &[FrameEntry]) -> DevonResult<()> {
    let mut bytes = Vec::with_capacity(directory.len() * DIRECTORY_ENTRY_LEN);
    for entry in directory {
        bytes.extend_from_slice(&entry.offset.to_le_bytes());
        bytes.extend_from_slice(&entry.compressed_len.to_le_bytes());
        bytes.extend_from_slice(&entry.crc32c.to_le_bytes());
    }
    out.write_all(&bytes)?;
    out.write_all(&crc32c::crc32c(&bytes).to_le_bytes())?;
    Ok(())
}

fn read_header(file: &File, file_len: u64) -> DevonResult<PackHeader> {
    let mut header = [0_u8; HEADER_LEN];
    read_exact_at(file, &mut header, 0).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            corrupt(format!(
                "pack header truncated: file has {file_len} bytes, the header requires {HEADER_LEN}"
            ))
        } else {
            DevonError::Io(error)
        }
    })?;
    if header[0..9] != PACK_MAGIC {
        return Err(corrupt(
            "bad pack magic: the header does not start with DEVONPACK",
        ));
    }
    if header[9] != CONTAINER_VERSION {
        return Err(corrupt(format!(
            "unsupported pack container version {}: this build reads version {CONTAINER_VERSION}",
            header[9]
        )));
    }
    let codec = u16::from_le_bytes(header[10..12].try_into().unwrap_or([0, 0]));
    if codec != CODEC_ZSTD {
        return Err(corrupt(format!(
            "unsupported pack codec {codec}: this build reads codec {CODEC_ZSTD} (zstd)"
        )));
    }
    let stored_crc = u32::from_le_bytes(header[36..40].try_into().unwrap_or([0; 4]));
    let computed_crc = crc32c::crc32c(&header[..36]);
    if stored_crc != computed_crc {
        return Err(corrupt(format!(
            "pack header crc32c mismatch: stored {stored_crc:#010x}, computed {computed_crc:#010x}"
        )));
    }
    let page_size = u32::from_le_bytes(header[12..16].try_into().unwrap_or([0; 4]));
    if !(4096..=65536).contains(&page_size) || !page_size.is_power_of_two() {
        return Err(corrupt(format!(
            "pack header page_size {page_size} is not a power of two between 4096 and 65536"
        )));
    }
    Ok(PackHeader {
        page_size,
        page_count: u64::from_le_bytes(header[16..24].try_into().unwrap_or([0; 8])),
        frame_pages: u32::from_le_bytes(header[24..28].try_into().unwrap_or([0; 4])),
        frame_count: u64::from_le_bytes(header[28..36].try_into().unwrap_or([0; 8])),
    })
}

fn read_directory(file: &File, file_len: u64, frame_count: u64) -> DevonResult<Vec<FrameEntry>> {
    let byte_len = frame_count as usize * DIRECTORY_ENTRY_LEN;
    let mut bytes = vec![0_u8; byte_len];
    let mut crc_bytes = [0_u8; DIRECTORY_CRC_LEN];
    let region = || {
        format!(
            "pack directory truncated: {byte_len} bytes plus crc at offset {HEADER_LEN}, file has {file_len}"
        )
    };
    read_exact_at(file, &mut bytes, HEADER_LEN as u64).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            corrupt(region())
        } else {
            DevonError::Io(error)
        }
    })?;
    read_exact_at(file, &mut crc_bytes, (HEADER_LEN + byte_len) as u64).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            corrupt(region())
        } else {
            DevonError::Io(error)
        }
    })?;
    let stored_crc = u32::from_le_bytes(crc_bytes);
    let computed_crc = crc32c::crc32c(&bytes);
    if stored_crc != computed_crc {
        return Err(corrupt(format!(
            "pack directory crc32c mismatch: stored {stored_crc:#010x}, computed {computed_crc:#010x}"
        )));
    }
    Ok(bytes
        .as_chunks::<DIRECTORY_ENTRY_LEN>()
        .0
        .iter()
        .map(|chunk| FrameEntry {
            offset: u64::from_le_bytes(chunk[0..8].try_into().unwrap_or([0; 8])),
            compressed_len: u32::from_le_bytes(chunk[8..12].try_into().unwrap_or([0; 4])),
            crc32c: u32::from_le_bytes(chunk[12..16].try_into().unwrap_or([0; 4])),
        })
        .collect())
}

/// `page_count` and `frame_count` must agree through `frame_pages`
/// (`docs/SCALE.md` §7.2: a disagreeing `page_count` is `Corrupt`).
fn check_frame_count(page_count: u64, frame_pages: u32, frame_count: u64) -> DevonResult<()> {
    if frame_pages == 0 {
        return Err(corrupt("pack header frame_pages is zero"));
    }
    let expected = page_count.div_ceil(u64::from(frame_pages));
    if expected != frame_count {
        return Err(corrupt(format!(
            "pack page_count mismatch: {page_count} pages at {frame_pages} pages per frame \
             require {expected} frames, the header declares {frame_count}"
        )));
    }
    Ok(())
}

/// Every frame the directory names must lie within the file — truncation
/// at a frame is reported at open, not first touched at read.
fn check_frame_bounds(directory: &[FrameEntry], file_len: u64) -> DevonResult<()> {
    for (index, entry) in directory.iter().enumerate() {
        let end = entry.offset + u64::from(entry.compressed_len);
        if entry.offset < frames_offset(directory.len() as u64) || end > file_len {
            return Err(corrupt(format!(
                "pack frame {index} truncated: directory names {} bytes at offset {}, file has {file_len}",
                entry.compressed_len, entry.offset
            )));
        }
    }
    Ok(())
}

/// Positional read for the container file; the unix-only seam mirrors the
/// pager's `FileExt` law (a windows port swaps the trait, not the callers).
fn read_exact_at(file: &File, dst: &mut [u8], offset: u64) -> io::Result<()> {
    FileExt::read_exact_at(file, dst, offset)
}

fn corrupt(context: impl Into<String>) -> DevonError {
    DevonError::Corrupt {
        context: context.into(),
    }
}

fn invalid_argument(context: &str) -> DevonError {
    DevonError::InvalidArgument {
        context: context.to_owned(),
    }
}
