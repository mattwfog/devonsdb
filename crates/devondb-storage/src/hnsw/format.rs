//! HNSW root-page and layer-directory codecs.
//!
//! The byte layouts are specified by `docs/FORMAT.md` § HNSW index pages.

use crc32c::crc32c;
use devondb_types::{DevonError, DevonResult};

use super::types::{HnswConfig, HnswMetric, MAX_LEVEL, NavigationEncoding};

const ROOT_MAGIC: &[u8; 4] = b"HNSW";
const ROOT_LAYOUT_VERSION: u16 = 1;
const ROOT_HEADER_LEN: usize = 64;
const FIRST_DATA_PAGE: u64 = 2;
const LAYER_DIRECTORY_CELL_LEN: u64 = 8;

/// The decoded contents of one persistent HNSW root page.
///
/// `layer_dir_byte_len` and `layer_dir_crc32c` describe the exact payload
/// accepted by [`LayerDirectory::decode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HnswRoot {
    /// Topology-defining index configuration.
    pub config: HnswConfig,
    /// Contiguous table-offset prefix represented by this root.
    pub covered_rows: u64,
    /// Eligible entry node, or `None` for an index with no eligible nodes.
    pub entry_node: Option<u64>,
    /// Entry node's maximum level; zero when `entry_node` is `None`.
    pub entry_level: u8,
    /// Number of persisted layers; zero or exactly `entry_level + 1`.
    pub layer_count: u8,
    /// Number of base node groups intersecting `covered_rows`.
    pub group_count: u32,
    /// First page in the contiguous layer-directory run, or zero when empty.
    pub layer_dir_first_page: u64,
    /// Exact layer-directory payload length in bytes.
    pub layer_dir_byte_len: u32,
    /// CRC-32C over the exact layer-directory payload.
    pub layer_dir_crc32c: u32,
}

impl HnswRoot {
    /// Encodes this root into one page and zeroes every byte after the header.
    pub fn encode(&self, page: &mut [u8]) -> DevonResult<()> {
        if page.len() < ROOT_HEADER_LEN {
            return Err(invalid_argument("HNSW root page must be at least 64 bytes"));
        }
        self.validate()?;

        page.fill(0);
        page[..4].copy_from_slice(ROOT_MAGIC);
        page[4..6].copy_from_slice(&ROOT_LAYOUT_VERSION.to_le_bytes());
        page[6] = self.config.metric.as_byte();
        page[7] = self.config.navigation.as_byte();
        page[8..10].copy_from_slice(&self.config.m.to_le_bytes());
        page[10..12].copy_from_slice(&self.config.m0.to_le_bytes());
        page[12..16].copy_from_slice(&self.config.ef_construction.to_le_bytes());
        page[16..24].copy_from_slice(&self.config.level_seed.to_le_bytes());
        page[24..32].copy_from_slice(&self.covered_rows.to_le_bytes());
        page[32..40].copy_from_slice(&self.entry_node.unwrap_or(u64::MAX).to_le_bytes());
        page[40] = self.entry_level;
        page[41] = self.layer_count;
        page[44..48].copy_from_slice(&self.group_count.to_le_bytes());
        page[48..56].copy_from_slice(&self.layer_dir_first_page.to_le_bytes());
        page[56..60].copy_from_slice(&self.layer_dir_byte_len.to_le_bytes());
        page[60..64].copy_from_slice(&self.layer_dir_crc32c.to_le_bytes());
        Ok(())
    }

    /// Decodes and validates one root page.
    pub fn decode(page: &[u8]) -> DevonResult<Self> {
        validate_root_envelope(page)?;
        let config = HnswConfig {
            metric: HnswMetric::from_byte(page[6])?,
            navigation: NavigationEncoding::from_byte(page[7])?,
            m: read_u16(page, 8),
            m0: read_u16(page, 10),
            ef_construction: read_u32(page, 12),
            level_seed: read_u64(page, 16),
        };
        let entry_node = match read_u64(page, 32) {
            u64::MAX => None,
            node => Some(node),
        };
        let root = Self {
            config,
            covered_rows: read_u64(page, 24),
            entry_node,
            entry_level: page[40],
            layer_count: page[41],
            group_count: read_u32(page, 44),
            layer_dir_first_page: read_u64(page, 48),
            layer_dir_byte_len: read_u32(page, 56),
            layer_dir_crc32c: read_u32(page, 60),
        };
        root.validate()?;
        Ok(root)
    }

    fn validate(&self) -> DevonResult<()> {
        self.config.validate()?;
        self.validate_entry()?;
        self.validate_coverage()?;
        self.validate_layer_directory_reference()
    }

    fn validate_entry(&self) -> DevonResult<()> {
        if self.entry_level > MAX_LEVEL {
            return Err(corrupt(format!(
                "HNSW entry_level {} exceeds {MAX_LEVEL}",
                self.entry_level
            )));
        }
        match self.entry_node {
            None if self.entry_level != 0 || self.layer_count != 0 => Err(corrupt(
                "HNSW root without an entry node must have entry_level and layer_count zero",
            )),
            None => Ok(()),
            Some(node) if node >= self.covered_rows => Err(corrupt(format!(
                "HNSW entry node {node} is outside covered_rows {}",
                self.covered_rows
            ))),
            Some(_) if self.layer_count != self.entry_level + 1 => Err(corrupt(format!(
                "HNSW layer_count is {}, expected entry_level + 1 = {}",
                self.layer_count,
                self.entry_level + 1
            ))),
            Some(_) => Ok(()),
        }
    }

    fn validate_coverage(&self) -> DevonResult<()> {
        if (self.covered_rows == 0) != (self.group_count == 0) {
            return Err(corrupt(
                "HNSW group_count is zero exactly when covered_rows is zero",
            ));
        }
        if u64::from(self.group_count) > self.covered_rows {
            return Err(corrupt(format!(
                "HNSW group_count {} exceeds covered_rows {}",
                self.group_count, self.covered_rows
            )));
        }
        Ok(())
    }

    fn validate_layer_directory_reference(&self) -> DevonResult<()> {
        let expected = matrix_byte_len(self.layer_count, self.group_count)
            .and_then(|len| u32::try_from(len).ok())
            .ok_or_else(|| corrupt("HNSW layer-directory byte length exceeds u32"))?;
        if self.layer_dir_byte_len != expected {
            return Err(corrupt(format!(
                "HNSW layer_dir_byte_len is {}, expected exactly {expected}",
                self.layer_dir_byte_len
            )));
        }
        validate_first_page(self.layer_count, self.layer_dir_first_page)?;
        if expected == 0 && self.layer_dir_crc32c != crc32c(&[]) {
            return Err(corrupt(
                "empty HNSW layer directory must have the CRC-32C of an empty payload",
            ));
        }
        Ok(())
    }
}

/// A layer-major matrix of HNSW CSR directory page ids.
///
/// Raw cell value zero is the only spelling of an empty `(layer, group)`
/// adjacency. Every nonzero cell names a data page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerDirectory {
    layer_count: u8,
    group_count: u32,
    byte_len: u32,
    page_ids: Vec<u64>,
}

impl LayerDirectory {
    /// Constructs a directory from a layer-major page-id matrix.
    pub fn new(layer_count: u8, group_count: u32, page_ids: Vec<u64>) -> DevonResult<Self> {
        validate_layer_count(layer_count).map_err(invalid_from_context)?;
        let expected = matrix_element_count(layer_count, group_count)
            .ok_or_else(|| invalid_argument("HNSW layer-directory dimensions overflow usize"))?;
        let byte_len = matrix_byte_len(layer_count, group_count)
            .and_then(|len| u32::try_from(len).ok())
            .ok_or_else(|| invalid_argument("HNSW layer-directory byte length exceeds u32"))?;
        if page_ids.len() != expected {
            return Err(invalid_argument(format!(
                "HNSW layer-directory matrix has {} cells, expected {expected}",
                page_ids.len()
            )));
        }
        validate_page_ids(&page_ids).map_err(invalid_from_context)?;
        Ok(Self {
            layer_count,
            group_count,
            byte_len,
            page_ids,
        })
    }

    /// Decodes an exact payload using dimensions and checksum from `root`.
    pub fn decode(payload: &[u8], root: &HnswRoot) -> DevonResult<Self> {
        root.validate()?;
        let expected = usize::try_from(root.layer_dir_byte_len)
            .map_err(|_| corrupt("HNSW layer-directory byte length exceeds usize"))?;
        if payload.len() != expected {
            return Err(corrupt(format!(
                "HNSW layer-directory payload length is {}, expected exactly {expected}",
                payload.len()
            )));
        }
        if crc32c(payload) != root.layer_dir_crc32c {
            return Err(corrupt(
                "HNSW layer-directory payload CRC-32C does not match",
            ));
        }
        let page_ids = payload
            .as_chunks::<8>()
            .0
            .iter()
            .map(|bytes| read_u64(bytes, 0))
            .collect::<Vec<_>>();
        validate_page_ids(&page_ids).map_err(corrupt_from_context)?;
        Ok(Self {
            layer_count: root.layer_count,
            group_count: root.group_count,
            byte_len: root.layer_dir_byte_len,
            page_ids,
        })
    }

    /// Encodes the matrix as contiguous little-endian u64 cells.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        self.page_ids
            .iter()
            .flat_map(|page_id| page_id.to_le_bytes())
            .collect()
    }

    /// Returns the number of layers in the matrix.
    #[must_use]
    pub fn layer_count(&self) -> u8 {
        self.layer_count
    }

    /// Returns the number of base node groups in each layer.
    #[must_use]
    pub fn group_count(&self) -> u32 {
        self.group_count
    }

    /// Returns the exact encoded byte length.
    #[must_use]
    pub fn byte_len(&self) -> u32 {
        self.byte_len
    }

    /// Returns the CRC-32C of the exact encoded payload.
    #[must_use]
    pub fn crc32c(&self) -> u32 {
        crc32c(&self.encode())
    }

    /// Returns a CSR directory page id, with an empty cell represented by `None`.
    pub fn page_id(&self, layer: u8, group: u32) -> DevonResult<Option<u64>> {
        if layer >= self.layer_count || group >= self.group_count {
            return Err(invalid_argument(format!(
                "HNSW layer-directory cell ({layer}, {group}) is outside {}x{} dimensions",
                self.layer_count, self.group_count
            )));
        }
        let index = usize::from(layer) * self.group_count as usize + group as usize;
        Ok((self.page_ids[index] != 0).then_some(self.page_ids[index]))
    }

    /// Returns the layer-major raw cells; zero cells are empty groups.
    #[must_use]
    pub fn page_ids(&self) -> &[u64] {
        &self.page_ids
    }
}

fn validate_root_envelope(page: &[u8]) -> DevonResult<()> {
    if page.len() < ROOT_HEADER_LEN {
        return Err(corrupt("HNSW root page is shorter than 64 bytes"));
    }
    if &page[..4] != ROOT_MAGIC {
        return Err(corrupt("HNSW root magic is not HNSW"));
    }
    let version = read_u16(page, 4);
    if version != ROOT_LAYOUT_VERSION {
        return Err(corrupt(format!(
            "HNSW root layout_version is {version}, expected {ROOT_LAYOUT_VERSION}"
        )));
    }
    if page[42..44] != [0, 0] {
        return Err(corrupt("HNSW root reserved bytes are not zero"));
    }
    if page[ROOT_HEADER_LEN..].iter().any(|byte| *byte != 0) {
        return Err(corrupt("HNSW root page tail is not zero"));
    }
    Ok(())
}

fn validate_first_page(layer_count: u8, first_page: u64) -> DevonResult<()> {
    if layer_count == 0 && first_page != 0 {
        return Err(corrupt(
            "empty HNSW layer directory must have first page zero",
        ));
    }
    if layer_count != 0 && first_page < FIRST_DATA_PAGE {
        return Err(corrupt(
            "nonempty HNSW layer directory must start at a data page",
        ));
    }
    Ok(())
}

fn validate_layer_count(layer_count: u8) -> Result<(), String> {
    if layer_count > MAX_LEVEL + 1 {
        return Err(format!(
            "HNSW layer_count {layer_count} exceeds {}",
            MAX_LEVEL + 1
        ));
    }
    Ok(())
}

fn validate_page_ids(page_ids: &[u64]) -> Result<(), String> {
    if let Some((index, page_id)) = page_ids
        .iter()
        .enumerate()
        .find(|(_, page_id)| **page_id != 0 && **page_id < FIRST_DATA_PAGE)
    {
        return Err(format!(
            "HNSW layer-directory cell {index} names reserved page {page_id}"
        ));
    }
    Ok(())
}

fn matrix_element_count(layer_count: u8, group_count: u32) -> Option<usize> {
    usize::from(layer_count).checked_mul(group_count as usize)
}

fn matrix_byte_len(layer_count: u8, group_count: u32) -> Option<u64> {
    u64::from(layer_count)
        .checked_mul(u64::from(group_count))?
        .checked_mul(LAYER_DIRECTORY_CELL_LEN)
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ])
}

fn invalid_from_context(context: String) -> DevonError {
    invalid_argument(context)
}

fn corrupt_from_context(context: String) -> DevonError {
    corrupt(context)
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
