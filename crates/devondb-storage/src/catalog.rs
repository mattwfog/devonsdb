//! Catalog persistence: table schemas stored in a catalog page reached
//! from the superblock's `catalog_root`.
//!
//! Page format per `docs/FORMAT.md` § catalog (binding).

use std::collections::{BTreeMap, BTreeSet, HashSet};

use crc32c::crc32c;
use devondb_types::{
    DevonError, DevonResult,
    logical_type::LogicalType,
    schema::{NodeTableSchema, RelTableSchema, fold, suggestion_suffix},
};
use serde::{Deserialize, Serialize};

use crate::pager::Pager;
use crate::superblock::Superblock;

const HEADER_LEN: usize = 8;
/// Multi-page catalog directory header: `payload_len` (u32), `crc32c`
/// (u32), `cont_count` (u32); continuation page ids (u64 each) follow.
const MULTIPAGE_HEADER_LEN: usize = 12;

/// The persisted storage state of one node table.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableStorage {
    /// Directory page ids of the table's node groups, oldest first. Only
    /// the last group may hold fewer rows than the writer's group capacity.
    pub groups: Vec<u64>,
}

/// The persisted forward and backward CSR roots of one relationship table.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelStorage {
    /// CSR directory page ids grouped by source node group.
    pub fwd: Vec<u64>,
    /// CSR directory page ids grouped by destination node group.
    pub bwd: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
enum StorageState {
    Node(TableStorage),
    Rel(RelStorage),
}

/// The kind of a persistent index (`docs/HNSW.md` §3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IndexKind {
    /// A persistent HNSW graph over one vector column.
    Hnsw,
}

/// One persistent index entry. The canonical compact JSON field order is
/// this declaration order (`docs/HNSW.md` §3.1); metric and topology
/// parameters live in the index root page, not here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexEntry {
    /// Index name as created, unique among indexes under ASCII folding.
    pub name: String,
    /// Index kind discriminator.
    pub kind: IndexKind,
    /// The node table the index covers.
    pub table: String,
    /// The vector column the index covers.
    pub column: String,
    /// Page id of the immutable index root; never zero.
    pub root: u64,
}

/// One column of a declared ontology interface
/// (`docs/FORMAT.md` § Catalog `ontology`; `docs/ONTOLOGY.md` § 2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InterfaceColumn {
    /// Column name the implementing table must carry (fold-matched).
    pub name: String,
    /// Required logical type, compared for equality on the implementer.
    #[serde(rename = "type")]
    pub ty: devondb_types::logical_type::LogicalType,
}

/// One declared interface: a named property set node classes implement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InterfaceEntry {
    /// Interface name as declared, unique under ASCII folding.
    pub name: String,
    /// Required columns in declaration order; never empty.
    pub columns: Vec<InterfaceColumn>,
}

/// Ontology metadata for one node table. Every field except `table` is
/// optional; consumers derive defaults (`docs/ONTOLOGY.md` § 2).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeClassEntry {
    /// The node table this class annotates (fold-matched, must exist).
    pub table: String,
    /// Singular display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    /// Plural spelling for NL grounding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plural: Option<String>,
    /// Pinned label column (fold-matched against the table's columns).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Columns a card/grid leads with (each fold-matched).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub summary: Vec<String>,
    /// Display color, free text (e.g. `#7aa2ff`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Free-text description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Declared interfaces (each fold-matched against `interfaces`, and
    /// every interface column must exist on the table with equal type).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub implements: Vec<String>,
}

/// Ontology metadata for one relationship table.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelClassEntry {
    /// The relationship table this class annotates (fold-matched).
    pub table: String,
    /// Forward verb phrase ("knows") for NL grounding and entity pages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verb: Option<String>,
    /// Inverse verb phrase ("is known by").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inverse: Option<String>,
}

/// The catalog's ontology section: declared interfaces and classes
/// (`docs/FORMAT.md` § Catalog `ontology`). Present only when at least
/// one declaration exists, so pre-ontology catalog bytes are unchanged.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ontology {
    /// Declared interfaces in declaration order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub interfaces: Vec<InterfaceEntry>,
    /// Node-class annotations in declaration order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub node_classes: Vec<NodeClassEntry>,
    /// Relationship-class annotations in declaration order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rel_classes: Vec<RelClassEntry>,
}

impl Ontology {
    fn is_empty(&self) -> bool {
        self.interfaces.is_empty() && self.node_classes.is_empty() && self.rel_classes.is_empty()
    }
}

/// One persistently named query plan (`docs/FORMAT.md` § Catalog `pins`).
///
/// Storage deliberately treats `plan` as opaque JSON apart from its envelope
/// shape. Full DevonPlan decoding belongs to the facade, keeping the storage
/// crate independent of `devondb-plan`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinEntry {
    /// Pin name as declared, unique under ASCII folding.
    pub name: String,
    /// Original input text retained as provenance.
    pub text: String,
    /// Canonical DevonPlan JSON envelope.
    pub plan: serde_json::Value,
    /// Catalog-publication LSN that created this pin.
    pub created_lsn: u64,
}

impl PinEntry {
    /// Builds an entry from opaque canonical plan JSON, validating only the
    /// storage-level envelope shape.
    pub fn from_plan_json(
        name: String,
        text: String,
        plan_json: &str,
        created_lsn: u64,
    ) -> DevonResult<Self> {
        let plan = serde_json::from_str(plan_json)
            .map_err(|error| invalid_argument(format!("pin plan is not valid JSON: {error}")))?;
        let entry = Self {
            name,
            text,
            plan,
            created_lsn,
        };
        validate_pin_entry(&entry).map_err(corrupt_to_invalid)?;
        Ok(entry)
    }

    /// Encodes the stored opaque plan envelope as compact JSON.
    pub fn plan_json(&self) -> DevonResult<String> {
        serde_json::to_string(&self.plan).map_err(|error| {
            corrupt(format!(
                "pin `{}` plan cannot be encoded: {error}",
                self.name
            ))
        })
    }
}

/// The persisted collection of node-table and relationship-table schemas.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Catalog {
    node_tables: Vec<NodeTableSchema>,
    rel_tables: Vec<RelTableSchema>,
    /// Table name → shape-discriminated storage state; absent for tables
    /// with no persisted rows or edges. `serde(default)` keeps pre-storage
    /// catalog pages readable.
    #[serde(default)]
    storage: BTreeMap<String, StorageState>,
    /// Persistent index entries in creation order. Omitted when empty so
    /// pre-index catalog bytes are unchanged and the `HNSW_INDEX` feature
    /// bit is set iff this is non-empty (`docs/HNSW.md` §8.2).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    indexes: Vec<IndexEntry>,
    /// Ontology section; omitted when no declaration exists so
    /// pre-ontology catalog bytes are unchanged and the `ONTOLOGY`
    /// feature bit is set iff this is present
    /// (`docs/FORMAT.md` § Catalog `ontology`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ontology: Option<Ontology>,
    /// Pinned plans in declaration order. Omitted when empty so every
    /// pre-pin catalog byte remains unchanged and `PINNED_PLANS` is set iff
    /// this array is non-empty (`docs/FORMAT.md` § Catalog `pins`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pins: Vec<PinEntry>,
}

impl Catalog {
    /// Returns the node-table schemas in declaration order.
    #[must_use]
    pub fn node_tables(&self) -> &[NodeTableSchema] {
        &self.node_tables
    }

    /// Returns the relationship-table schemas in declaration order.
    #[must_use]
    pub fn rel_tables(&self) -> &[RelTableSchema] {
        &self.rel_tables
    }

    /// Returns the persistent index entries in creation order.
    #[must_use]
    pub fn indexes(&self) -> &[IndexEntry] {
        &self.indexes
    }

    /// Adds a persistent index entry after validating it against this
    /// catalog. One index per `(table, column)` — stricter than
    /// `docs/HNSW.md` §1 rule 10's `(table, column, metric)` tuple, which
    /// the entry cannot express because the metric lives in the root page;
    /// the edge-device write-cost rationale favors the stricter rule.
    pub fn add_index(&mut self, entry: IndexEntry) -> DevonResult<()> {
        if self
            .indexes
            .iter()
            .any(|index| fold(&index.name) == fold(&entry.name))
        {
            return Err(invalid_argument(format!(
                "index name `{}` is already in the catalog",
                entry.name
            )));
        }
        if let Some(existing) = self.indexes.iter().find(|index| {
            fold(&index.table) == fold(&entry.table) && fold(&index.column) == fold(&entry.column)
        }) {
            return Err(invalid_argument(format!(
                "column `{}.{}` is already indexed by `{}`",
                entry.table, entry.column, existing.name
            )));
        }
        self.validate_index_entry(&entry).map_err(|error| {
            // Decode-path validation reports Corrupt; at DDL time the same
            // failure is a caller error.
            match error {
                DevonError::Corrupt { context } => invalid_argument(context),
                other => other,
            }
        })?;
        self.indexes.push(entry);
        Ok(())
    }

    /// Replaces a persistent index's root page id (checkpoint publication).
    pub fn set_index_root(&mut self, name: &str, root: u64) -> DevonResult<()> {
        if root == 0 {
            return Err(invalid_argument(format!(
                "index `{name}` cannot use root page 0"
            )));
        }
        let entry = self
            .indexes
            .iter_mut()
            .find(|index| fold(&index.name) == fold(name))
            .ok_or_else(|| DevonError::NotFound {
                what: format!("index `{name}`"),
            })?;
        entry.root = root;
        Ok(())
    }

    /// Returns the ontology section, if any declaration exists.
    #[must_use]
    pub fn ontology(&self) -> Option<&Ontology> {
        self.ontology.as_ref()
    }

    /// Returns pinned plans in declaration order.
    #[must_use]
    pub fn pins(&self) -> &[PinEntry] {
        &self.pins
    }

    /// Adds a pinned plan after validating its storage-level envelope.
    pub fn pin(&mut self, entry: PinEntry) -> DevonResult<()> {
        if self
            .pins
            .iter()
            .any(|pin| fold(&pin.name) == fold(&entry.name))
        {
            return Err(invalid_argument(format!(
                "pin `{}` is already in the catalog",
                entry.name
            )));
        }
        validate_pin_entry(&entry).map_err(corrupt_to_invalid)?;
        self.pins.push(entry);
        Ok(())
    }

    /// Removes a pinned plan resolved by ASCII-folded name.
    pub fn unpin(&mut self, name: &str) -> DevonResult<()> {
        let folded = fold(name);
        let Some(index) = self.pins.iter().position(|pin| fold(&pin.name) == folded) else {
            return Err(DevonError::NotFound {
                what: format!(
                    "pin `{name}`{}",
                    suggestion_suffix(name, self.pins.iter().map(|pin| pin.name.as_str()))
                ),
            });
        };
        self.pins.remove(index);
        Ok(())
    }

    /// Looks up a node class by its table's ASCII-folded name.
    #[must_use]
    pub fn node_class(&self, table: &str) -> Option<&NodeClassEntry> {
        let folded = fold(table);
        self.ontology
            .as_ref()?
            .node_classes
            .iter()
            .find(|class| fold(&class.table) == folded)
    }

    /// Looks up a relationship class by its table's ASCII-folded name.
    #[must_use]
    pub fn rel_class(&self, table: &str) -> Option<&RelClassEntry> {
        let folded = fold(table);
        self.ontology
            .as_ref()?
            .rel_classes
            .iter()
            .find(|class| fold(&class.table) == folded)
    }

    /// Looks up a declared interface by its ASCII-folded name.
    #[must_use]
    pub fn interface(&self, name: &str) -> Option<&InterfaceEntry> {
        let folded = fold(name);
        self.ontology
            .as_ref()?
            .interfaces
            .iter()
            .find(|interface| fold(&interface.name) == folded)
    }

    /// Declares an interface after validating it against this catalog.
    pub fn declare_interface(&mut self, entry: InterfaceEntry) -> DevonResult<()> {
        if self.interface(&entry.name).is_some() {
            return Err(invalid_argument(format!(
                "interface `{}` is already declared",
                entry.name
            )));
        }
        validate_interface_entry(&entry).map_err(corrupt_to_invalid)?;
        self.ontology
            .get_or_insert_with(Ontology::default)
            .interfaces
            .push(entry);
        Ok(())
    }

    /// Declares a node class after validating it against this catalog.
    pub fn declare_node_class(&mut self, entry: NodeClassEntry) -> DevonResult<()> {
        if self.node_class(&entry.table).is_some() {
            return Err(invalid_argument(format!(
                "node table `{}` already has a class declaration",
                entry.table
            )));
        }
        self.validate_node_class_entry(&entry)
            .map_err(corrupt_to_invalid)?;
        self.ontology
            .get_or_insert_with(Ontology::default)
            .node_classes
            .push(entry);
        Ok(())
    }

    /// Declares a relationship class after validating it against this catalog.
    pub fn declare_rel_class(&mut self, entry: RelClassEntry) -> DevonResult<()> {
        if self.rel_class(&entry.table).is_some() {
            return Err(invalid_argument(format!(
                "relationship table `{}` already has a class declaration",
                entry.table
            )));
        }
        self.validate_rel_class_entry(&entry)
            .map_err(corrupt_to_invalid)?;
        self.ontology
            .get_or_insert_with(Ontology::default)
            .rel_classes
            .push(entry);
        Ok(())
    }

    /// Adds a node-table schema if its name is unused by either table kind.
    pub fn add_node_table(&mut self, schema: NodeTableSchema) -> DevonResult<()> {
        if self.contains_table(schema.name()) {
            return Err(duplicate_table(schema.name()));
        }
        self.node_tables.push(schema);
        Ok(())
    }

    /// Adds a relationship-table schema after validating its name and endpoints.
    pub fn add_rel_table(&mut self, schema: RelTableSchema) -> DevonResult<()> {
        if self.contains_table(schema.name()) {
            return Err(duplicate_table(schema.name()));
        }
        self.require_node_table(schema.from())?;
        self.require_node_table(schema.to())?;
        reject_vector_encoded_relationship_columns(schema.name(), schema.columns())?;
        self.rel_tables.push(schema);
        Ok(())
    }

    /// Looks up a node-table schema by its ASCII-folded name.
    #[must_use]
    pub fn node_table(&self, name: &str) -> Option<&NodeTableSchema> {
        let folded = fold(name);
        self.node_tables
            .iter()
            .find(|schema| fold(schema.name()) == folded)
    }

    /// Looks up a relationship-table schema by its ASCII-folded name.
    #[must_use]
    pub fn rel_table(&self, name: &str) -> Option<&RelTableSchema> {
        let folded = fold(name);
        self.rel_tables
            .iter()
            .find(|schema| fold(schema.name()) == folded)
    }

    /// Returns a node table's persisted storage state, if any.
    #[must_use]
    pub fn table_storage(&self, name: &str) -> Option<&TableStorage> {
        match self.storage_state(name) {
            Some(StorageState::Node(storage)) => Some(storage),
            Some(StorageState::Rel(_)) | None => None,
        }
    }

    /// Replaces a node table's storage state; the table must exist.
    pub fn set_table_storage(&mut self, name: &str, storage: TableStorage) -> DevonResult<()> {
        let display_name = self
            .node_table(name)
            .map(|schema| schema.name().to_owned())
            .ok_or_else(|| DevonError::NotFound {
                what: format!("node table `{name}`"),
            })?;
        self.storage
            .insert(display_name, StorageState::Node(storage));
        Ok(())
    }

    /// Returns a relationship table's persisted CSR storage state, if any.
    #[must_use]
    pub fn rel_storage(&self, name: &str) -> Option<&RelStorage> {
        match self.storage_state(name) {
            Some(StorageState::Rel(storage)) => Some(storage),
            Some(StorageState::Node(_)) | None => None,
        }
    }

    /// Replaces a relationship table's CSR storage state; the table must exist.
    pub fn set_rel_storage(&mut self, name: &str, storage: RelStorage) -> DevonResult<()> {
        let display_name = self
            .rel_table(name)
            .map(|schema| schema.name().to_owned())
            .ok_or_else(|| DevonError::NotFound {
                what: format!("relationship table `{name}`"),
            })?;
        self.storage
            .insert(display_name, StorageState::Rel(storage));
        Ok(())
    }

    /// Persists the catalog and publishes its page and checkpoint LSN.
    /// A payload that fits one page keeps the v1 single-page encoding
    /// byte-for-byte; a larger payload spills into continuation pages
    /// behind `MULTIPAGE_CATALOG_FLAG` (`docs/FORMAT.md` § Multi-page
    /// catalog).
    pub fn save(&self, pager: &Pager, publish_lsn: u64) -> DevonResult<()> {
        let previous = pager.superblock();
        let prospective = crate::free_pages::prospective_pages(previous.db_id);
        let current_lsn = previous.checkpoint_lsn;
        if publish_lsn <= current_lsn {
            return Err(invalid_argument(format!(
                "catalog publish LSN {publish_lsn} must exceed current checkpoint LSN {current_lsn}"
            )));
        }
        let page_size = previous.page_size as usize;
        let payload = self.encode_payload()?;
        let capacity = page_size
            .checked_sub(HEADER_LEN)
            .ok_or_else(|| invalid_argument("catalog page is shorter than its header"))?;
        let multipage = payload.len() > capacity;
        let catalog_root = if multipage {
            Self::save_multipage(pager, &payload, page_size)?
        } else {
            let page = Self::encode_single(&payload, page_size)?;
            let root = pager.allocate_page()?;
            pager.write_page(root, &page)?;
            root
        };
        pager.sync()?;

        // Free-page retirement (`docs/FREE_PAGES.md` § Retirement and
        // reclamation sequence): every page the old catalog could
        // reach and the new one cannot enters the ledger, and consumption
        // rewrites flush, all durably BEFORE the superblock flip below.
        let mut superseded = self.superseded_pages(pager, &previous)?;
        if !prospective.is_empty() {
            let reachable = self.reachable_storage_pages(pager)?;
            superseded.extend(
                prospective
                    .iter()
                    .copied()
                    .filter(|page_id| !reachable.contains(page_id)),
            );
        }
        superseded.sort_unstable();
        superseded.dedup();
        pager.retire_pages(superseded, publish_lsn)?;

        let mut superblock = pager.superblock();
        superblock.checkpoint_lsn = publish_lsn;
        superblock.catalog_root = catalog_root;
        superblock.feature_flags = set_or_clear(
            superblock.feature_flags,
            crate::superblock::MULTIPAGE_CATALOG_FLAG,
            multipage,
        );
        // Feature bits are derived, never independently managed: each is
        // set iff the catalog state it marks is published, in the SAME
        // alternate-superblock publication as the catalog that carries it
        // (docs/FORMAT.md § Feature flag registry, docs/HNSW.md §8.2).
        superblock.feature_flags = set_or_clear(
            superblock.feature_flags,
            crate::superblock::HNSW_INDEX_FLAG,
            !self.indexes.is_empty(),
        );
        superblock.feature_flags = set_or_clear(
            superblock.feature_flags,
            crate::superblock::ONTOLOGY_FLAG,
            self.ontology.is_some(),
        );
        superblock.feature_flags = set_or_clear(
            superblock.feature_flags,
            crate::superblock::PINNED_PLANS_FLAG,
            !self.pins.is_empty(),
        );
        superblock.feature_flags = set_or_clear(
            superblock.feature_flags,
            crate::superblock::GEO_COLUMNS_FLAG,
            self.has_geo_columns(),
        );
        superblock.feature_flags = set_or_clear(
            superblock.feature_flags,
            crate::superblock::SCALAR_TYPES_V2_FLAG,
            self.has_scalar_v2_columns(),
        );
        // Zone maps are sticky. The pager records an in-flight stats-bearing
        // directory write, and the catalog publication that first makes such
        // a directory reachable claims the governing feature bit. Never clear
        // a bit already present on an older publication.
        if pager.has_written_zone_maps() {
            superblock.feature_flags |= crate::superblock::ZONE_MAPS_FLAG;
        }
        // COLUMN_ENCODINGS (bit 13) is likewise sticky, derived by sniffing
        // the flags word of every node-group directory this publication
        // reaches (docs/SCALE.md §8.1: set iff any node group carries a
        // non-plain payload; this feature is not read-safe). The pager's pending
        // note admits validated reads before this flip, but is deliberately
        // not the publication input: reachability remains what derives the
        // durable bit. The production writer runs the §8.4 adaptive selection,
        // so any selected non-plain encoding declares the bit.
        if self.has_non_plain_node_groups(pager) {
            superblock.feature_flags |= crate::superblock::COLUMN_ENCODINGS_FLAG;
        }
        // FREE_PAGES is sticky too: set by the first ledger-writing
        // publication, and
        // once a chain exists the extension must stay governed forever.
        if pager.has_free_pages_ledger() {
            superblock.feature_flags |= crate::superblock::FREE_PAGES_FLAG;
        }
        pager.commit_superblock(superblock)?;
        crate::free_pages::published_prospective_pages(previous.db_id, &prospective);
        Ok(())
    }

    /// Enumerates pages reachable only from this unpublished prospective
    /// catalog relative to `baseline`.
    ///
    /// Detach checkpoint uses this after a measured group-budget failure so
    /// all groups written before the failure can enter the next publication's
    /// retirement ledger. Damaged inventories conservatively contribute the
    /// proven directory page and leave the remainder to the offline sweep.
    #[must_use]
    pub fn prospective_pages(&self, pager: &Pager, baseline: &Self) -> Vec<u64> {
        let baseline_direct = baseline.direct_page_ids();
        let mut pages = BTreeSet::new();
        for schema in self.node_tables() {
            let Some(storage) = self.table_storage(schema.name()) else {
                continue;
            };
            let types = node_schema_types(schema);
            for group in storage
                .groups
                .iter()
                .copied()
                .filter(|group| *group != 0 && !baseline_direct.contains(group))
            {
                match crate::node_group::group_page_inventory(pager, group, &types) {
                    Ok(inventory) => pages.extend(inventory),
                    Err(_) => {
                        pages.insert(group);
                    }
                }
            }
        }
        for schema in self.rel_tables() {
            let Some(storage) = self.rel_storage(schema.name()) else {
                continue;
            };
            let types = rel_schema_types(schema);
            for group in storage
                .fwd
                .iter()
                .chain(&storage.bwd)
                .copied()
                .filter(|group| *group != 0 && !baseline_direct.contains(group))
            {
                match crate::csr_group::csr_page_inventory(pager, group, &types) {
                    Ok(inventory) => pages.extend(inventory),
                    Err(_) => {
                        pages.insert(group);
                    }
                }
            }
        }
        // New roots can share CSR subtrees with published roots. Only pages
        // proven absent from every baseline subtree are prospective.
        let baseline_hnsw = baseline
            .indexes
            .iter()
            .try_fold(BTreeSet::new(), |mut all, index| {
                all.extend(crate::hnsw::index::index_page_inventory(pager, index.root)?);
                Ok::<_, DevonError>(all)
            });
        for root in self
            .indexes
            .iter()
            .map(|index| index.root)
            .filter(|root| !baseline_direct.contains(root))
        {
            if let (Ok(live), Ok(inventory)) = (
                &baseline_hnsw,
                crate::hnsw::index::index_page_inventory(pager, root),
            ) {
                pages.extend(inventory.difference(live).copied());
            } else {
                pages.insert(root);
            }
        }
        pages.into_iter().collect()
    }

    /// Computes the pages this publication makes unreachable: the diff of
    /// the old catalog's reachable direct page ids against the new one's,
    /// expanded through changed group directories, plus the old catalog
    /// chain itself (`docs/FREE_PAGES.md` § Retirement and reclamation
    /// sequence step 1).
    ///
    /// Directory expansion prunes on identity: a directory page id present
    /// in both catalogs names an immutable page, so its whole payload
    /// subtree is shared and cancels out of the set difference — per-
    /// publication I/O is proportional to churn, never to file size.
    /// Changed HNSW roots expand through their layer directories and CSR
    /// payloads; pages reachable from any new index are removed from the diff.
    fn superseded_pages(&self, pager: &Pager, previous: &Superblock) -> DevonResult<Vec<u64>> {
        if previous.catalog_root == 0 {
            return Ok(Vec::new());
        }
        // The authoritative superblock is still `previous` until the flip,
        // so this loads exactly the catalog recovery could reach today.
        let old = Self::load(pager)?;
        let new_direct = self.direct_page_ids();
        let mut superseded = catalog_chain_pages(pager, previous)?;

        for schema in old.node_tables() {
            let Some(storage) = old.table_storage(schema.name()) else {
                continue;
            };
            let types = node_schema_types(schema);
            for group in &storage.groups {
                if new_direct.contains(group) {
                    continue;
                }
                match crate::node_group::group_page_inventory(pager, *group, &types) {
                    Ok(pages) => superseded.extend(pages),
                    // The group is unreachable either way: reclaim the
                    // proven page and leave the rest to the sweep rather
                    // than let a damaged directory sink every checkpoint.
                    Err(_) => superseded.push(*group),
                }
            }
        }
        for schema in old.rel_tables() {
            let Some(storage) = old.rel_storage(schema.name()) else {
                continue;
            };
            let types = rel_schema_types(schema);
            for directory in storage.fwd.iter().chain(&storage.bwd) {
                if *directory == 0 {
                    continue;
                }
                if new_direct.contains(directory) {
                    continue;
                }
                match crate::csr_group::csr_page_inventory(pager, *directory, &types) {
                    Ok(pages) => superseded.extend(pages),
                    Err(_) => superseded.push(*directory),
                }
            }
        }
        let changed = old
            .indexes
            .iter()
            .filter(|index| !new_direct.contains(&index.root));
        let mut retired_hnsw = BTreeSet::new();
        for index in changed {
            retired_hnsw.extend(crate::hnsw::index::index_page_inventory(pager, index.root)?);
        }
        if !retired_hnsw.is_empty() {
            // Different roots may share entire immutable CSR cells and payloads.
            for index in &self.indexes {
                let live = crate::hnsw::index::index_page_inventory(pager, index.root)?;
                retired_hnsw.retain(|page| !live.contains(page));
            }
            superseded.extend(retired_hnsw);
        }
        Ok(superseded)
    }

    /// Every page id this catalog names directly: node-group directories,
    /// CSR directories, and index roots.
    fn direct_page_ids(&self) -> BTreeSet<u64> {
        let mut ids = BTreeSet::new();
        for state in self.storage.values() {
            match state {
                StorageState::Node(storage) => ids.extend(&storage.groups),
                StorageState::Rel(storage) => {
                    ids.extend(&storage.fwd);
                    ids.extend(&storage.bwd);
                }
            }
        }
        ids.extend(self.indexes.iter().map(|index| index.root));
        ids
    }

    fn reachable_storage_pages(&self, pager: &Pager) -> DevonResult<BTreeSet<u64>> {
        let empty = Self::default();
        let mut pages: BTreeSet<u64> = self.prospective_pages(pager, &empty).into_iter().collect();
        pages.extend(
            self.direct_page_ids()
                .into_iter()
                .filter(|page_id| *page_id != 0),
        );
        for index in &self.indexes {
            pages.extend(crate::hnsw::index::index_page_inventory(pager, index.root)?);
        }
        Ok(pages)
    }

    /// Whether any reachable node-group directory declares a
    /// `COLUMN_ENCODINGS` section — the structural derivation of
    /// `COLUMN_ENCODINGS_FLAG` (`docs/FORMAT.md` § Feature flag registry;
    /// `docs/SCALE.md` §8.1). Relationship groups (CSR) carry no column
    /// encodings. The sniff is tolerant of unreadable pages, exactly like
    /// the superseded-page expansion above: a damaged directory fails on
    /// the read paths, not here.
    fn has_non_plain_node_groups(&self, pager: &Pager) -> bool {
        self.storage.values().any(|state| match state {
            StorageState::Node(storage) => storage
                .groups
                .iter()
                .any(|group| crate::node_group::directory_declares_column_encodings(pager, *group)),
            StorageState::Rel(_) => false,
        })
    }

    /// Whether any node table carries a `GeoPoint` column — the structural
    /// derivation of `GEO_COLUMNS_FLAG` (`docs/FORMAT.md` § Feature flag
    /// registry). Relationship tables cannot: the schema floor rejects
    /// GeoPoint relationship properties.
    fn has_geo_columns(&self) -> bool {
        self.node_tables.iter().any(|table| {
            table
                .columns()
                .iter()
                .any(|column| matches!(column.ty, LogicalType::GeoPoint))
        })
    }

    /// Whether any table carries a scalar-v2 column — the structural
    /// derivation of `SCALAR_TYPES_V2_FLAG` (`docs/FORMAT.md` § Feature
    /// flag registry). Relationship DDL currently rejects these properties,
    /// but both table collections participate to keep the derivation equal
    /// to the format law for every decodable catalog.
    fn has_scalar_v2_columns(&self) -> bool {
        self.node_tables
            .iter()
            .map(NodeTableSchema::columns)
            .chain(self.rel_tables.iter().map(RelTableSchema::columns))
            .flatten()
            .any(|column| {
                matches!(
                    column.ty,
                    LogicalType::Timestamp
                        | LogicalType::Bytes
                        | LogicalType::Decimal { .. }
                        | LogicalType::Json
                )
            })
    }

    /// Loads the catalog referenced by the pager's authoritative superblock.
    /// `MULTIPAGE_CATALOG_FLAG` decides the root page's shape — the flag is
    /// derived in the same publication as the catalog it governs, so it is
    /// authoritative over sniffing the page bytes.
    pub fn load(pager: &Pager) -> DevonResult<Self> {
        let superblock = pager.superblock();
        let catalog_root = superblock.catalog_root;
        if catalog_root == 0 {
            return Ok(Self::default());
        }

        let page = read_catalog_page(pager, catalog_root)?;
        if superblock.feature_flags & crate::superblock::MULTIPAGE_CATALOG_FLAG != 0 {
            return Self::load_multipage(pager, &page);
        }
        Self::decode(&page)
    }

    /// Reassembles a multi-page catalog from its directory root page
    /// (format documented on [`Self::save_multipage`]).
    fn load_multipage(pager: &Pager, root: &[u8]) -> DevonResult<Self> {
        if root.len() < MULTIPAGE_HEADER_LEN {
            return Err(corrupt(
                "multi-page catalog directory is shorter than its header",
            ));
        }
        let page_size = root.len();
        let payload_len = read_u32(root, 0) as usize;
        let expected_checksum = read_u32(root, 4);
        let cont_count = read_u32(root, 8) as usize;
        if payload_len <= page_size.saturating_sub(HEADER_LEN) {
            return Err(corrupt(
                "multi-page catalog payload would fit a single v1 page",
            ));
        }
        if cont_count != payload_len.div_ceil(page_size) {
            return Err(corrupt(
                "multi-page catalog continuation count does not match its payload length",
            ));
        }
        let directory_len = MULTIPAGE_HEADER_LEN + cont_count * 8;
        if directory_len > page_size {
            return Err(corrupt(
                "multi-page catalog directory exceeds its page capacity",
            ));
        }
        if root[directory_len..].iter().any(|byte| *byte != 0) {
            return Err(corrupt("multi-page catalog directory padding is not zero"));
        }

        let mut payload = Vec::with_capacity(payload_len);
        for index in 0..cont_count {
            let id = read_u64(root, MULTIPAGE_HEADER_LEN + index * 8);
            if id == 0 {
                return Err(corrupt("multi-page catalog directory lists page id zero"));
            }
            let page = read_catalog_page(pager, id)?;
            if page.len() != page_size {
                return Err(corrupt(
                    "multi-page catalog continuation page size does not match its directory",
                ));
            }
            let taken = page_size.min(payload_len - index * page_size);
            payload.extend_from_slice(&page[..taken]);
            if page[taken..].iter().any(|byte| *byte != 0) {
                return Err(corrupt(
                    "multi-page catalog continuation padding is not zero",
                ));
            }
        }
        if crc32c(&payload) != expected_checksum {
            return Err(corrupt("catalog payload checksum does not match"));
        }
        Self::from_payload(&payload)
    }

    fn contains_table(&self, name: &str) -> bool {
        self.node_table(name).is_some() || self.rel_table(name).is_some()
    }

    fn storage_state(&self, name: &str) -> Option<&StorageState> {
        let folded = fold(name);
        self.storage
            .iter()
            .find(|(stored, _)| fold(stored) == folded)
            .map(|(_, storage)| storage)
    }

    fn require_node_table(&self, name: &str) -> DevonResult<()> {
        if self.node_table(name).is_none() {
            return Err(DevonError::NotFound {
                what: format!("node table `{name}`"),
            });
        }
        Ok(())
    }

    fn validate_storage(&self) -> DevonResult<()> {
        let mut names = HashSet::with_capacity(self.storage.len());
        for (name, storage) in &self.storage {
            if !names.insert(fold(name).into_owned()) {
                return Err(corrupt(format!(
                    "storage state contains fold-equal table name `{name}`"
                )));
            }
            match storage {
                StorageState::Node(_) if self.node_table(name).is_some() => {}
                StorageState::Rel(_) if self.rel_table(name).is_some() => {}
                StorageState::Node(_) if self.rel_table(name).is_some() => {
                    return Err(corrupt(format!(
                        "relationship table `{name}` has node-table storage"
                    )));
                }
                StorageState::Rel(_) if self.node_table(name).is_some() => {
                    return Err(corrupt(format!(
                        "node table `{name}` has relationship-table storage"
                    )));
                }
                StorageState::Node(_) | StorageState::Rel(_) => {
                    return Err(corrupt(format!(
                        "storage state names unknown table `{name}`"
                    )));
                }
            }
        }
        Ok(())
    }

    fn encode_payload(&self) -> DevonResult<Vec<u8>> {
        serde_json::to_vec(self).map_err(|error| {
            invalid_argument(format!("catalog cannot be encoded as JSON: {error}"))
        })
    }

    fn encode_single(payload: &[u8], page_size: usize) -> DevonResult<Vec<u8>> {
        let capacity = page_size
            .checked_sub(HEADER_LEN)
            .ok_or_else(|| invalid_argument("catalog page is shorter than its header"))?;
        if payload.len() > capacity {
            return Err(invalid_argument(format!(
                "catalog payload is {} bytes but page capacity is {capacity} bytes",
                payload.len()
            )));
        }

        let payload_len = u32::try_from(payload.len())
            .map_err(|_| invalid_argument("catalog payload length exceeds u32"))?;
        let mut page = vec![0_u8; page_size];
        page[..4].copy_from_slice(&payload_len.to_le_bytes());
        page[4..HEADER_LEN].copy_from_slice(&crc32c(payload).to_le_bytes());
        page[HEADER_LEN..HEADER_LEN + payload.len()].copy_from_slice(payload);
        Ok(page)
    }

    /// Writes an over-one-page payload as full-page continuation chunks
    /// plus a directory root page:
    /// `[payload_len u32][crc32c u32][cont_count u32][page id u64 × count]`,
    /// zero-padded. Payload bytes live only in the continuation pages.
    fn save_multipage(pager: &Pager, payload: &[u8], page_size: usize) -> DevonResult<u64> {
        let cont_count = payload.len().div_ceil(page_size);
        let directory_len = MULTIPAGE_HEADER_LEN + cont_count * 8;
        if directory_len > page_size {
            return Err(invalid_argument(format!(
                "catalog payload needs {cont_count} continuation pages but the directory page holds {}",
                (page_size.saturating_sub(MULTIPAGE_HEADER_LEN)) / 8
            )));
        }
        let payload_len = u32::try_from(payload.len())
            .map_err(|_| invalid_argument("catalog payload length exceeds u32"))?;
        let count = u32::try_from(cont_count)
            .map_err(|_| invalid_argument("catalog continuation count exceeds u32"))?;

        let mut root = vec![0_u8; page_size];
        root[..4].copy_from_slice(&payload_len.to_le_bytes());
        root[4..8].copy_from_slice(&crc32c(payload).to_le_bytes());
        root[8..MULTIPAGE_HEADER_LEN].copy_from_slice(&count.to_le_bytes());
        for (index, chunk) in payload.chunks(page_size).enumerate() {
            let id = pager.allocate_page()?;
            let mut page = vec![0_u8; page_size];
            page[..chunk.len()].copy_from_slice(chunk);
            pager.write_page(id, &page)?;
            let offset = MULTIPAGE_HEADER_LEN + index * 8;
            root[offset..offset + 8].copy_from_slice(&id.to_le_bytes());
        }
        let root_id = pager.allocate_page()?;
        pager.write_page(root_id, &root)?;
        Ok(root_id)
    }

    fn decode(page: &[u8]) -> DevonResult<Self> {
        if page.len() < HEADER_LEN {
            return Err(corrupt("catalog page is shorter than its header"));
        }

        let payload_len = read_u32(page, 0) as usize;
        if payload_len > page.len() - HEADER_LEN {
            return Err(corrupt("catalog payload length exceeds page capacity"));
        }
        let payload = &page[HEADER_LEN..HEADER_LEN + payload_len];
        let expected_checksum = read_u32(page, 4);
        if crc32c(payload) != expected_checksum {
            return Err(corrupt("catalog payload checksum does not match"));
        }
        if page[HEADER_LEN + payload_len..]
            .iter()
            .any(|byte| *byte != 0)
        {
            return Err(corrupt("catalog page padding is not zero"));
        }

        Self::from_payload(payload)
    }

    fn from_payload(payload: &[u8]) -> DevonResult<Self> {
        let catalog: Self = serde_json::from_slice(payload)
            .map_err(|error| corrupt(format!("catalog payload is not valid JSON: {error}")))?;
        catalog.validate_schemas()?;
        catalog.validate_storage()?;
        catalog.validate_indexes()?;
        catalog.validate_ontology()?;
        catalog.validate_pins()?;
        Ok(catalog)
    }

    fn validate_pins(&self) -> DevonResult<()> {
        let mut names = HashSet::with_capacity(self.pins.len());
        for pin in &self.pins {
            if !names.insert(fold(&pin.name).into_owned()) {
                return Err(corrupt(format!(
                    "pin `{}` has a fold-equal duplicate in the catalog",
                    pin.name
                )));
            }
            validate_pin_entry(pin)?;
        }
        Ok(())
    }

    fn validate_ontology(&self) -> DevonResult<()> {
        let Some(ontology) = &self.ontology else {
            return Ok(());
        };
        if ontology.is_empty() {
            return Err(corrupt(
                "catalog ontology section is present but declares nothing",
            ));
        }
        let mut interfaces = HashSet::with_capacity(ontology.interfaces.len());
        for interface in &ontology.interfaces {
            if !interfaces.insert(fold(&interface.name).into_owned()) {
                return Err(corrupt(format!(
                    "interface `{}` has a fold-equal duplicate in the catalog",
                    interface.name
                )));
            }
            validate_interface_entry(interface)?;
        }
        let mut node_classes = HashSet::with_capacity(ontology.node_classes.len());
        for class in &ontology.node_classes {
            if !node_classes.insert(fold(&class.table).into_owned()) {
                return Err(corrupt(format!(
                    "node class for `{}` has a fold-equal duplicate in the catalog",
                    class.table
                )));
            }
            self.validate_node_class_entry(class)?;
        }
        let mut rel_classes = HashSet::with_capacity(ontology.rel_classes.len());
        for class in &ontology.rel_classes {
            if !rel_classes.insert(fold(&class.table).into_owned()) {
                return Err(corrupt(format!(
                    "relationship class for `{}` has a fold-equal duplicate in the catalog",
                    class.table
                )));
            }
            self.validate_rel_class_entry(class)?;
        }
        Ok(())
    }

    fn validate_node_class_entry(&self, entry: &NodeClassEntry) -> DevonResult<()> {
        let Some(schema) = self.node_table(&entry.table) else {
            return Err(corrupt(format!(
                "node class names unknown node table `{}`",
                entry.table
            )));
        };
        for column in entry.label.iter().chain(&entry.summary) {
            if schema.column_index(column).is_none() {
                return Err(corrupt(format!(
                    "node class for `{}` names unknown column `{column}`",
                    entry.table
                )));
            }
        }
        for name in &entry.implements {
            let Some(interface) = self.interface(name) else {
                return Err(corrupt(format!(
                    "node class for `{}` implements unknown interface `{name}`",
                    entry.table
                )));
            };
            for required in &interface.columns {
                let matches = schema
                    .column_index(&required.name)
                    .and_then(|index| schema.columns().get(index))
                    .is_some_and(|column| column.ty == required.ty);
                if !matches {
                    return Err(corrupt(format!(
                        "node table `{}` does not satisfy interface `{}`: requires column `{}` of type {}",
                        entry.table, interface.name, required.name, required.ty
                    )));
                }
            }
        }
        Ok(())
    }

    fn validate_rel_class_entry(&self, entry: &RelClassEntry) -> DevonResult<()> {
        if self.rel_table(&entry.table).is_none() {
            return Err(corrupt(format!(
                "relationship class names unknown relationship table `{}`",
                entry.table
            )));
        }
        Ok(())
    }

    fn validate_indexes(&self) -> DevonResult<()> {
        let mut names = HashSet::with_capacity(self.indexes.len());
        let mut columns = HashSet::with_capacity(self.indexes.len());
        for index in &self.indexes {
            if !names.insert(fold(&index.name).into_owned()) {
                return Err(corrupt(format!(
                    "index name `{}` has a fold-equal duplicate in the catalog",
                    index.name
                )));
            }
            if !columns.insert((
                fold(&index.table).into_owned(),
                fold(&index.column).into_owned(),
            )) {
                return Err(corrupt(format!(
                    "index column `{}.{}` has a fold-equal duplicate in the catalog",
                    index.table, index.column
                )));
            }
            self.validate_index_entry(index)?;
        }
        Ok(())
    }

    fn validate_schemas(&self) -> DevonResult<()> {
        let mut tables = HashSet::with_capacity(self.node_tables.len() + self.rel_tables.len());
        for schema in &self.node_tables {
            validate_decoded_table(&mut tables, schema.name(), schema.columns())?;
        }
        for schema in &self.rel_tables {
            validate_decoded_table(&mut tables, schema.name(), schema.columns())?;
        }
        Ok(())
    }

    fn validate_index_entry(&self, index: &IndexEntry) -> DevonResult<()> {
        if index.root == 0 {
            return Err(corrupt(format!("index `{}` has root page 0", index.name)));
        }
        let Some(schema) = self.node_table(&index.table) else {
            return Err(corrupt(format!(
                "index `{}` covers unknown node table `{}`",
                index.name, index.table
            )));
        };
        if schema.column_index(&index.column).is_none() {
            return Err(corrupt(format!(
                "index `{}` covers unknown column `{}.{}`",
                index.name, index.table, index.column
            )));
        }
        Ok(())
    }
}

fn validate_decoded_table(
    tables: &mut HashSet<String>,
    table_name: &str,
    columns: &[devondb_types::schema::Column],
) -> DevonResult<()> {
    if !tables.insert(fold(table_name).into_owned()) {
        return Err(corrupt(format!(
            "table name `{table_name}` has a fold-equal duplicate in the catalog"
        )));
    }
    let mut column_names = HashSet::with_capacity(columns.len());
    for column in columns {
        if !column_names.insert(fold(&column.name).into_owned()) {
            return Err(corrupt(format!(
                "column `{}` has a fold-equal duplicate in table `{table_name}`",
                column.name
            )));
        }
    }
    Ok(())
}

fn validate_pin_entry(entry: &PinEntry) -> DevonResult<()> {
    let Some(envelope) = entry.plan.as_object() else {
        return Err(corrupt(format!(
            "pin `{}` plan is not a JSON object",
            entry.name
        )));
    };
    if !envelope.get("v").is_some_and(serde_json::Value::is_number) {
        return Err(corrupt(format!(
            "pin `{}` plan has no numeric `v`",
            entry.name
        )));
    }
    if !envelope
        .get("plan")
        .is_some_and(serde_json::Value::is_object)
    {
        return Err(corrupt(format!(
            "pin `{}` plan has no object `plan`",
            entry.name
        )));
    }
    Ok(())
}

// Pager arguments come from persisted metadata here. Keep its direct-call
// contract intact while reporting invalid stored references as corruption.
fn read_catalog_page(pager: &Pager, page_id: u64) -> DevonResult<Vec<u8>> {
    pager.read_page(page_id).map_err(|error| match error {
        DevonError::InvalidArgument { context } => corrupt(format!(
            "catalog references invalid page {page_id}: {context}"
        )),
        error => error,
    })
}

/// The pages of one catalog chain: the root plus, under
/// `MULTIPAGE_CATALOG`, every continuation page its directory lists.
fn catalog_chain_pages(pager: &Pager, previous: &Superblock) -> DevonResult<Vec<u64>> {
    let mut pages = vec![previous.catalog_root];
    if previous.feature_flags & crate::superblock::MULTIPAGE_CATALOG_FLAG != 0 {
        let root = read_catalog_page(pager, previous.catalog_root)?;
        let cont_count = read_u32(&root, 8) as usize;
        let directory_len = MULTIPAGE_HEADER_LEN + cont_count * 8;
        if directory_len > root.len() {
            return Err(corrupt(
                "multi-page catalog directory exceeds its page capacity",
            ));
        }
        for index in 0..cont_count {
            let id = read_u64(&root, MULTIPAGE_HEADER_LEN + index * 8);
            if id == 0 {
                return Err(corrupt("multi-page catalog directory lists page id zero"));
            }
            pages.push(id);
        }
    }
    Ok(pages)
}

fn node_schema_types(schema: &NodeTableSchema) -> Vec<LogicalType> {
    schema.columns().iter().map(|column| column.ty).collect()
}

fn rel_schema_types(schema: &RelTableSchema) -> Vec<LogicalType> {
    schema.columns().iter().map(|column| column.ty).collect()
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

fn duplicate_table(name: &str) -> DevonError {
    invalid_argument(format!("table name `{name}` is already in the catalog"))
}

/// Relationship property persistence does not yet support `VectorEncoded`;
/// node-table columns are supported by node groups.
fn reject_vector_encoded_relationship_columns(
    table_name: &str,
    columns: &[devondb_types::schema::Column],
) -> DevonResult<()> {
    for column in columns {
        if let devondb_types::logical_type::LogicalType::VectorEncoded { .. } = column.ty {
            return Err(invalid_argument(format!(
                "column `{}` in relationship table `{table_name}` has type {}; \
                 VectorEncoded relationship properties are not supported",
                column.name, column.ty
            )));
        }
    }
    Ok(())
}

fn validate_interface_entry(entry: &InterfaceEntry) -> DevonResult<()> {
    if entry.name.is_empty() {
        return Err(corrupt("interface has an empty name"));
    }
    if entry.columns.is_empty() {
        return Err(corrupt(format!(
            "interface `{}` declares no columns",
            entry.name
        )));
    }
    let mut names = HashSet::with_capacity(entry.columns.len());
    for column in &entry.columns {
        if column.name.is_empty() {
            return Err(corrupt(format!(
                "interface `{}` has a column with an empty name",
                entry.name
            )));
        }
        if !names.insert(fold(&column.name).into_owned()) {
            return Err(corrupt(format!(
                "interface `{}` column `{}` has a fold-equal duplicate",
                entry.name, column.name
            )));
        }
    }
    Ok(())
}

/// Maps decode-path Corrupt errors to DDL-time invalid arguments — the
/// same failure is a caller error when reached through a declare method.
fn corrupt_to_invalid(error: DevonError) -> DevonError {
    match error {
        DevonError::Corrupt { context } => invalid_argument(context),
        other => other,
    }
}

fn set_or_clear(flags: u64, bit: u64, on: bool) -> u64 {
    if on { flags | bit } else { flags & !bit }
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

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::io::{Read, Seek, SeekFrom, Write};

    use devondb_types::{
        DevonError,
        logical_type::LogicalType,
        schema::{Column, NodeTableSchema, RelTableSchema},
    };
    use tempfile::tempdir;

    use super::{
        Catalog, HEADER_LEN, IndexEntry, IndexKind, MULTIPAGE_HEADER_LEN, Pager, RelStorage,
        TableStorage, read_u64,
    };

    const PAGE_SIZE: u32 = 4096;
    const DB_ID: [u8; 16] = *b"catalog-test-id!";

    fn column(name: &str, ty: LogicalType, primary_key: bool) -> Column {
        Column {
            name: name.to_owned(),
            ty,
            primary_key,
        }
    }

    fn node(name: &str) -> NodeTableSchema {
        NodeTableSchema::new(
            name.to_owned(),
            vec![column("id", LogicalType::Int64, true)],
        )
        .unwrap()
    }

    fn vector_table(name: &str) -> NodeTableSchema {
        NodeTableSchema::new(
            name.to_owned(),
            vec![
                column("id", LogicalType::Int64, true),
                column("embedding", LogicalType::Vector { dim: 4 }, false),
            ],
        )
        .unwrap()
    }

    fn rel(name: &str, from: &str, to: &str) -> RelTableSchema {
        RelTableSchema::new(
            name.to_owned(),
            from.to_owned(),
            to.to_owned(),
            vec![column("since", LogicalType::Int64, false)],
        )
        .unwrap()
    }

    #[test]
    fn fresh_database_loads_an_empty_catalog() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("empty.devondb");
        let pager = Pager::create(path, PAGE_SIZE, DB_ID).unwrap();

        let catalog = Catalog::load(&pager).unwrap();

        assert!(catalog.node_tables().is_empty());
        assert!(catalog.rel_tables().is_empty());
    }

    #[test]
    fn invalid_persisted_catalog_roots_are_corruption() {
        for root in [1, 2, u64::MAX / u64::from(PAGE_SIZE), u64::MAX] {
            let directory = tempdir().unwrap();
            let path = directory.path().join("invalid-root.devondb");
            let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
            let mut header = pager.superblock();
            header.checkpoint_lsn = 1;
            header.catalog_root = root;
            pager.commit_superblock(header).unwrap();
            drop(pager);

            let reopened = Pager::open(&path).unwrap();
            let error = Catalog::load(&reopened).unwrap_err();
            assert!(
                matches!(error, DevonError::Corrupt { .. }),
                "root {root}: {error}"
            );
            // The pager's direct caller contract still treats invalid IDs as
            // arguments; only interpretation of a persisted pointer changes.
            assert!(matches!(
                reopened.read_page(root),
                Err(DevonError::InvalidArgument { .. })
            ));
        }
    }

    #[test]
    fn invalid_persisted_catalog_continuations_are_corruption() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("invalid-continuation.devondb");
        let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
        let catalog = catalog_larger_than(PAGE_SIZE as usize);
        catalog.save(&pager, 1).unwrap();
        let root_id = pager.superblock().catalog_root;
        let root = pager.read_page(root_id).unwrap();
        for id in [0, 1, 1 << 32, u64::MAX / u64::from(PAGE_SIZE), u64::MAX] {
            let mut changed = root.clone();
            changed[MULTIPAGE_HEADER_LEN..MULTIPAGE_HEADER_LEN + 8]
                .copy_from_slice(&id.to_le_bytes());
            pager.write_page(root_id, &changed).unwrap();
            let error = Catalog::load(&pager).unwrap_err();
            assert!(
                matches!(error, DevonError::Corrupt { .. }),
                "continuation {id}: {error}"
            );
        }
        pager.write_page(root_id, &root).unwrap();
        assert_eq!(Catalog::load(&pager).unwrap(), catalog);
    }

    #[test]
    fn save_and_load_round_trip_across_reopen() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("round-trip.devondb");
        let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
        let mut catalog = Catalog::default();
        catalog.add_node_table(node("Person")).unwrap();
        catalog.add_node_table(node("Company")).unwrap();
        catalog
            .add_rel_table(rel("WorksAt", "Person", "Company"))
            .unwrap();

        catalog.save(&pager, 1).unwrap();
        drop(pager);
        let reopened = Pager::open(path).unwrap();

        assert_eq!(Catalog::load(&reopened).unwrap(), catalog);
        assert_eq!(catalog.node_table("Person").unwrap().name(), "Person");
        assert_eq!(catalog.rel_table("WorksAt").unwrap().to(), "Company");
    }

    #[test]
    fn save_rejects_non_increasing_publish_lsn() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("publish-lsn.devondb");
        let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
        let mut catalog = Catalog::default();
        catalog.add_node_table(node("Person")).unwrap();

        assert_invalid_argument_mentions(catalog.save(&pager, 0), "must exceed");
        catalog.save(&pager, 7).unwrap();
        let published_root = pager.superblock().catalog_root;
        assert_invalid_argument_mentions(catalog.save(&pager, 7), "must exceed");
        assert_invalid_argument_mentions(catalog.save(&pager, 6), "must exceed");

        assert_eq!(pager.superblock().checkpoint_lsn, 7);
        assert_eq!(pager.superblock().catalog_root, published_root);
    }

    /// Grows a catalog until its JSON payload exceeds `target` bytes.
    fn catalog_larger_than(target: usize) -> Catalog {
        let mut catalog = Catalog::default();
        let mut index = 0_usize;
        while catalog.encode_payload().unwrap().len() <= target {
            catalog
                .add_node_table(node(&format!("Table{index:03}")))
                .unwrap();
            index += 1;
        }
        catalog
    }

    #[test]
    fn multipage_catalog_round_trips_and_derives_its_flag() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("multipage.devondb");
        let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
        // Three-plus pages of payload exercises full middle chunks, not
        // just the partial tail page.
        let catalog = catalog_larger_than(3 * PAGE_SIZE as usize);

        catalog.save(&pager, 1).unwrap();
        assert_ne!(
            pager.superblock().feature_flags & crate::superblock::MULTIPAGE_CATALOG_FLAG,
            0
        );
        drop(pager);

        let reopened = Pager::open(&path).unwrap();
        assert_eq!(Catalog::load(&reopened).unwrap(), catalog);

        // Shrinking back under one page clears the derived bit in the same
        // publication and returns to the v1 single-page encoding.
        let mut small = Catalog::default();
        small.add_node_table(node("Person")).unwrap();
        small.save(&reopened, 2).unwrap();
        assert_eq!(
            reopened.superblock().feature_flags & crate::superblock::MULTIPAGE_CATALOG_FLAG,
            0
        );
        assert_eq!(Catalog::load(&reopened).unwrap(), small);
    }

    #[test]
    fn multipage_catalog_rejects_tampered_continuation_and_directory() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("multipage-corrupt.devondb");
        let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
        let catalog = catalog_larger_than(PAGE_SIZE as usize);
        catalog.save(&pager, 1).unwrap();

        let root_id = pager.superblock().catalog_root;
        let root = pager.read_page(root_id).unwrap();
        let first_cont = read_u64(&root, MULTIPAGE_HEADER_LEN);

        // A flipped payload byte in a continuation page fails the checksum.
        let mut tampered = pager.read_page(first_cont).unwrap();
        tampered[0] ^= 0xFF;
        pager.write_page(first_cont, &tampered).unwrap();
        let checksum = Catalog::load(&pager).unwrap_err();
        assert!(matches!(checksum, DevonError::Corrupt { .. }), "{checksum}");
        assert!(checksum.to_string().contains("checksum"), "{checksum}");

        // Restore the payload, then break the directory's continuation count.
        tampered[0] ^= 0xFF;
        pager.write_page(first_cont, &tampered).unwrap();
        assert_eq!(Catalog::load(&pager).unwrap(), catalog);
        let mut bad_root = root.clone();
        bad_root[8..MULTIPAGE_HEADER_LEN].copy_from_slice(&u32::MAX.to_le_bytes());
        pager.write_page(root_id, &bad_root).unwrap();
        let count = Catalog::load(&pager).unwrap_err();
        assert!(matches!(count, DevonError::Corrupt { .. }), "{count}");
        assert!(count.to_string().contains("continuation count"), "{count}");
    }

    #[test]
    fn table_storage_round_trips_and_requires_an_existing_table() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("storage-map.devondb");
        let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
        let mut catalog = Catalog::default();
        catalog.add_node_table(node("Person")).unwrap();

        let missing =
            catalog.set_table_storage("Ghost", super::TableStorage { groups: vec![2, 7] });
        assert!(matches!(missing, Err(DevonError::NotFound { .. })));
        assert_eq!(catalog.table_storage("Person"), None);

        catalog
            .set_table_storage("pErSoN", super::TableStorage { groups: vec![2, 7] })
            .unwrap();
        catalog.save(&pager, 1).unwrap();
        drop(pager);

        let reopened = Pager::open(path).unwrap();
        let loaded = Catalog::load(&reopened).unwrap();
        assert_eq!(loaded, catalog);
        assert_eq!(loaded.table_storage("PERSON").unwrap().groups, vec![2, 7]);
    }

    #[test]
    fn relationship_storage_round_trips_and_requires_an_existing_rel() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("rel-storage-map.devondb");
        let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
        let mut catalog = Catalog::default();
        catalog.add_node_table(node("Person")).unwrap();
        catalog
            .add_rel_table(rel("Knows", "Person", "Person"))
            .unwrap();
        let storage = RelStorage {
            fwd: vec![0, 11],
            bwd: vec![12],
        };

        assert!(matches!(
            catalog.set_rel_storage("Ghost", storage.clone()),
            Err(DevonError::NotFound { .. })
        ));
        catalog.set_rel_storage("kNoWs", storage.clone()).unwrap();
        catalog.save(&pager, 1).unwrap();
        drop(pager);

        let reopened = Pager::open(path).unwrap();
        let loaded = Catalog::load(&reopened).unwrap();
        assert_eq!(loaded.rel_storage("KNOWS"), Some(&storage));
        assert_eq!(loaded.table_storage("Knows"), None);
        assert_eq!(loaded.rel_storage("Person"), None);
    }

    #[test]
    fn node_storage_api_and_json_shape_remain_unchanged() {
        let mut catalog = Catalog::default();
        catalog.add_node_table(node("Person")).unwrap();
        catalog
            .set_table_storage("Person", TableStorage { groups: vec![2, 7] })
            .unwrap();

        let json = serde_json::to_string(&catalog).unwrap();

        assert_eq!(
            json,
            r#"{"node_tables":[{"name":"Person","columns":[{"name":"id","ty":"Int64","primary_key":true}]}],"rel_tables":[],"storage":{"Person":{"groups":[2,7]}}}"#
        );
    }

    #[test]
    fn index_entries_round_trip_with_canonical_spelling() {
        let mut catalog = Catalog::default();
        catalog.add_node_table(vector_table("Corpus")).unwrap();
        catalog.indexes.push(IndexEntry {
            name: "embedding_cos".to_owned(),
            kind: IndexKind::Hnsw,
            table: "Corpus".to_owned(),
            column: "embedding".to_owned(),
            root: 42,
        });

        let json = serde_json::to_string(&catalog).unwrap();
        assert!(
            json.ends_with(
                r#""indexes":[{"name":"embedding_cos","kind":"hnsw","table":"Corpus","column":"embedding","root":42}]}"#
            ),
            "canonical index spelling changed: {json}"
        );

        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let decoded = Catalog::decode(&catalog_page(&value)).unwrap();
        assert_eq!(decoded, catalog);
        assert_eq!(decoded.indexes(), catalog.indexes());
    }

    #[test]
    fn add_index_validates_and_save_derives_the_feature_flag() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("db.devondb");
        let pager = Pager::create(&path, 4096, *b"0123456789abcdef").unwrap();

        let mut catalog = Catalog::default();
        catalog.add_node_table(vector_table("Corpus")).unwrap();
        let entry = |name: &str, column: &str, root: u64| IndexEntry {
            name: name.to_owned(),
            kind: IndexKind::Hnsw,
            table: "Corpus".to_owned(),
            column: column.to_owned(),
            root,
        };

        // Pre-index save leaves the flag clear.
        catalog.save(&pager, 1).unwrap();
        assert_eq!(
            pager.superblock().feature_flags & crate::superblock::HNSW_INDEX_FLAG,
            0
        );

        catalog.add_index(entry("a", "embedding", 40)).unwrap();
        // Duplicate name, duplicate column, zero root, unknown column all
        // fail as caller errors and leave the catalog unchanged.
        for (bad, needle) in [
            (entry("a", "embedding", 41), "already in the catalog"),
            (entry("b", "embedding", 41), "already indexed by `a`"),
            (entry("b", "id", 0), "root page 0"),
            (entry("b", "ghost", 41), "unknown column"),
        ] {
            let error = catalog.add_index(bad).unwrap_err();
            assert!(
                matches!(
                    &error,
                    DevonError::InvalidArgument { context } if context.contains(needle)
                ),
                "expected InvalidArgument containing `{needle}`, got {error}"
            );
        }
        assert_eq!(catalog.indexes().len(), 1);

        // Publishing one index sets the flag in the same superblock write.
        catalog.save(&pager, 2).unwrap();
        assert_ne!(
            pager.superblock().feature_flags & crate::superblock::HNSW_INDEX_FLAG,
            0
        );
        let reloaded = Catalog::load(&pager).unwrap();
        assert_eq!(reloaded.indexes(), catalog.indexes());

        // Root replacement round-trips; unknown index is NotFound.
        catalog.set_index_root("a", 99).unwrap();
        assert_eq!(catalog.indexes()[0].root, 99);
        assert!(matches!(
            catalog.set_index_root("ghost", 7),
            Err(DevonError::NotFound { .. })
        ));
        assert!(matches!(
            catalog.set_index_root("a", 0),
            Err(DevonError::InvalidArgument { .. })
        ));
    }

    #[test]
    fn invalid_index_entries_are_corruption() {
        let entry = |name: &str, table: &str, column: &str, root: u64| {
            serde_json::json!({
                "name": name, "kind": "hnsw", "table": table,
                "column": column, "root": root,
            })
        };
        let base = serde_json::to_value(&{
            let mut catalog = Catalog::default();
            catalog.add_node_table(vector_table("Corpus")).unwrap();
            catalog
        })
        .unwrap();
        let with_indexes = |indexes: serde_json::Value| {
            let mut value = base.clone();
            value["indexes"] = indexes;
            value
        };

        let cases = [
            // Fold-equal index name.
            with_indexes(serde_json::json!([
                entry("A", "Corpus", "embedding", 4),
                entry("a", "Corpus", "other", 5),
            ])),
            // Fold-equal indexed tuple.
            with_indexes(serde_json::json!([
                entry("a", "Corpus", "embedding", 4),
                entry("b", "CORPUS", "EMBEDDING", 5),
            ])),
            // Root page zero.
            with_indexes(serde_json::json!([entry("a", "Corpus", "embedding", 0)])),
            // Unknown table.
            with_indexes(serde_json::json!([entry("a", "Ghost", "embedding", 4)])),
            // Unknown column.
            with_indexes(serde_json::json!([entry("a", "Corpus", "ghost", 4)])),
            // Unknown entry field.
            with_indexes(serde_json::json!([{
                "name": "a", "kind": "hnsw", "table": "Corpus",
                "column": "embedding", "root": 4, "metric": "cosine",
            }])),
        ];
        for case in cases {
            assert!(
                matches!(
                    Catalog::decode(&catalog_page(&case)),
                    Err(DevonError::Corrupt { .. })
                ),
                "accepted invalid catalog: {case}"
            );
        }
    }

    fn named_person() -> NodeTableSchema {
        NodeTableSchema::new(
            "Person".to_owned(),
            vec![
                column("id", LogicalType::Int64, true),
                column("name", LogicalType::String, false),
                column("age", LogicalType::Int64, false),
            ],
        )
        .unwrap()
    }

    fn nameable() -> super::InterfaceEntry {
        super::InterfaceEntry {
            name: "Nameable".to_owned(),
            columns: vec![super::InterfaceColumn {
                name: "name".to_owned(),
                ty: LogicalType::String,
            }],
        }
    }

    fn person_class() -> super::NodeClassEntry {
        super::NodeClassEntry {
            table: "Person".to_owned(),
            display: Some("Person".to_owned()),
            plural: Some("people".to_owned()),
            label: Some("name".to_owned()),
            summary: vec!["name".to_owned(), "age".to_owned()],
            color: Some("#7aa2ff".to_owned()),
            description: None,
            implements: vec!["Nameable".to_owned()],
        }
    }

    #[test]
    fn ontology_round_trips_and_save_derives_the_feature_flag() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("ontology.devondb");
        let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
        let mut catalog = Catalog::default();
        catalog.add_node_table(named_person()).unwrap();
        catalog
            .add_rel_table(rel("Knows", "Person", "Person"))
            .unwrap();

        catalog.save(&pager, 1).unwrap();
        assert_eq!(
            pager.superblock().feature_flags & crate::superblock::ONTOLOGY_FLAG,
            0,
            "no ontology declared, so the bit stays clear"
        );

        catalog.declare_interface(nameable()).unwrap();
        catalog.declare_node_class(person_class()).unwrap();
        catalog
            .declare_rel_class(super::RelClassEntry {
                table: "Knows".to_owned(),
                verb: Some("knows".to_owned()),
                inverse: Some("is known by".to_owned()),
            })
            .unwrap();
        catalog.save(&pager, 2).unwrap();
        assert_ne!(
            pager.superblock().feature_flags & crate::superblock::ONTOLOGY_FLAG,
            0,
            "declared ontology derives the bit"
        );
        drop(pager);

        let reopened = Pager::open(path).unwrap();
        let loaded = Catalog::load(&reopened).unwrap();
        assert_eq!(loaded, catalog);
        assert_eq!(
            loaded.node_class("pErSoN").unwrap().plural.as_deref(),
            Some("people")
        );
        assert_eq!(
            loaded.rel_class("KNOWS").unwrap().verb.as_deref(),
            Some("knows")
        );
        assert_eq!(loaded.interface("nameable").unwrap().columns.len(), 1);
    }

    #[test]
    fn catalog_without_ontology_keeps_byte_identical_json() {
        let mut catalog = Catalog::default();
        catalog.add_node_table(named_person()).unwrap();

        let json = serde_json::to_string(&catalog).unwrap();

        assert!(
            !json.contains("ontology"),
            "no declaration must add no key: {json}"
        );
    }

    #[test]
    fn pins_round_trip_and_save_derives_the_feature_flag() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("pins.devondb");
        let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
        let mut catalog = Catalog::default();

        catalog.save(&pager, 1).unwrap();
        assert_eq!(
            pager.superblock().feature_flags & crate::superblock::PINNED_PLANS_FLAG,
            0
        );
        catalog
            .pin(super::PinEntry {
                name: "all people".to_owned(),
                text: "show all people".to_owned(),
                plan: serde_json::json!({
                    "v": 0,
                    "plan": {"op": "ScanNodes", "table": "Person", "binding": "person"}
                }),
                created_lsn: 2,
            })
            .unwrap();
        catalog.save(&pager, 2).unwrap();
        assert_ne!(
            pager.superblock().feature_flags & crate::superblock::PINNED_PLANS_FLAG,
            0
        );
        drop(pager);

        let reopened = Pager::open(&path).unwrap();
        let loaded = Catalog::load(&reopened).unwrap();
        assert_eq!(loaded, catalog);
        assert_eq!(loaded.pins()[0].name, "all people");
    }

    #[test]
    fn catalog_without_pins_keeps_byte_identical_json() {
        let mut catalog = Catalog::default();
        catalog.add_node_table(named_person()).unwrap();

        let json = serde_json::to_string(&catalog).unwrap();

        assert!(!json.contains("pins"), "no pins must add no key: {json}");
    }

    #[test]
    fn pin_methods_validate_duplicates_shape_and_suggestions() {
        let mut catalog = Catalog::default();
        let entry = super::PinEntry {
            name: "Ada Friends".to_owned(),
            text: "who does ada know".to_owned(),
            plan: serde_json::json!({"v": 0, "plan": {}}),
            created_lsn: 7,
        };
        catalog.pin(entry.clone()).unwrap();
        assert_invalid_argument_mentions(catalog.pin(entry), "already in the catalog");

        let malformed = super::PinEntry {
            name: "broken".to_owned(),
            text: String::new(),
            plan: serde_json::json!({"v": "zero", "plan": {}}),
            created_lsn: 8,
        };
        assert_invalid_argument_mentions(catalog.pin(malformed), "numeric `v`");

        let error = catalog.unpin("Ada Frends").unwrap_err();
        assert_eq!(
            error.to_string(),
            "not found: pin `Ada Frends` (did you mean `Ada Friends`?)"
        );
        catalog.unpin("ada friends").unwrap();
        assert!(catalog.pins().is_empty());
    }

    #[test]
    fn invalid_pin_sections_are_corruption() {
        let cases = [
            serde_json::json!({
                "node_tables": [], "rel_tables": [],
                "pins": [{"name": "x", "text": "x", "plan": [], "created_lsn": 1}]
            }),
            serde_json::json!({
                "node_tables": [], "rel_tables": [],
                "pins": [{"name": "x", "text": "x", "plan": {"v": 0}, "created_lsn": 1}]
            }),
            serde_json::json!({
                "node_tables": [], "rel_tables": [],
                "pins": [
                    {"name": "Pin", "text": "x", "plan": {"v": 0, "plan": {}}, "created_lsn": 1},
                    {"name": "pin", "text": "y", "plan": {"v": 0, "plan": {}}, "created_lsn": 2}
                ]
            }),
            serde_json::json!({
                "node_tables": [], "rel_tables": [],
                "pins": [{
                    "name": "x", "text": "x", "plan": {"v": 0, "plan": {}},
                    "created_lsn": 1, "surprise": true
                }]
            }),
        ];

        for value in cases {
            assert!(matches!(
                Catalog::decode(&catalog_page(&value)),
                Err(DevonError::Corrupt { .. })
            ));
        }
    }

    #[test]
    fn ontology_json_shape_matches_the_format_spec() {
        let mut catalog = Catalog::default();
        catalog.add_node_table(named_person()).unwrap();
        catalog.declare_interface(nameable()).unwrap();

        let json = serde_json::to_string(&catalog).unwrap();

        assert!(
            json.contains(
                r#""ontology":{"interfaces":[{"name":"Nameable","columns":[{"name":"name","type":"String"}]}]}"#
            ),
            "canonical ontology shape drifted: {json}"
        );
    }

    #[test]
    fn declare_methods_reject_unknown_references_and_duplicates() {
        let mut catalog = Catalog::default();
        catalog.add_node_table(named_person()).unwrap();
        catalog.declare_interface(nameable()).unwrap();

        assert_invalid_argument_mentions(catalog.declare_interface(nameable()), "already declared");
        assert_invalid_argument_mentions(
            catalog.declare_node_class(super::NodeClassEntry {
                table: "Ghost".to_owned(),
                ..super::NodeClassEntry::default()
            }),
            "unknown node table",
        );
        assert_invalid_argument_mentions(
            catalog.declare_node_class(super::NodeClassEntry {
                table: "Person".to_owned(),
                label: Some("missing".to_owned()),
                ..super::NodeClassEntry::default()
            }),
            "unknown column",
        );
        assert_invalid_argument_mentions(
            catalog.declare_node_class(super::NodeClassEntry {
                table: "Person".to_owned(),
                implements: vec!["Ghostly".to_owned()],
                ..super::NodeClassEntry::default()
            }),
            "unknown interface",
        );
        assert_invalid_argument_mentions(
            catalog.declare_rel_class(super::RelClassEntry {
                table: "Person".to_owned(),
                ..super::RelClassEntry::default()
            }),
            "unknown relationship table",
        );

        let mut mismatched = Catalog::default();
        mismatched.add_node_table(node("Untyped")).unwrap();
        mismatched.declare_interface(nameable()).unwrap();
        assert_invalid_argument_mentions(
            mismatched.declare_node_class(super::NodeClassEntry {
                table: "Untyped".to_owned(),
                implements: vec!["Nameable".to_owned()],
                ..super::NodeClassEntry::default()
            }),
            "does not satisfy interface",
        );

        catalog.declare_node_class(person_class()).unwrap();
        assert_invalid_argument_mentions(
            catalog.declare_node_class(super::NodeClassEntry {
                table: "PERSON".to_owned(),
                ..super::NodeClassEntry::default()
            }),
            "already has a class declaration",
        );
    }

    #[test]
    fn invalid_ontology_sections_are_corruption() {
        let empty_section = catalog_page(&serde_json::json!({
            "node_tables": [],
            "rel_tables": [],
            "ontology": {}
        }));
        let empty = Catalog::decode(&empty_section).unwrap_err();
        assert!(matches!(empty, DevonError::Corrupt { .. }), "{empty}");

        let unknown_table = catalog_page(&serde_json::json!({
            "node_tables": [],
            "rel_tables": [],
            "ontology": {"node_classes": [{"table": "Ghost"}]}
        }));
        let unknown = Catalog::decode(&unknown_table).unwrap_err();
        assert!(matches!(unknown, DevonError::Corrupt { .. }), "{unknown}");

        let unknown_field = catalog_page(&serde_json::json!({
            "node_tables": [],
            "rel_tables": [],
            "ontology": {"surprise": true}
        }));
        let field = Catalog::decode(&unknown_field).unwrap_err();
        assert!(matches!(field, DevonError::Corrupt { .. }), "{field}");
    }

    #[test]
    fn fold_equal_decoded_tables_columns_and_storage_keys_are_corruption() {
        let duplicate_tables = serde_json::json!({
            "node_tables": [node("Person")],
            "rel_tables": [{
                "name": "person", "from": "Person", "to": "Person", "columns": []
            }],
            "storage": {}
        });
        let duplicate_columns = serde_json::json!({
            "node_tables": [{
                "name": "Person",
                "columns": [
                    {"name": "Name", "ty": "Int64", "primary_key": true},
                    {"name": "name", "ty": "String", "primary_key": false}
                ]
            }],
            "rel_tables": [],
            "storage": {}
        });
        let duplicate_storage = serde_json::json!({
            "node_tables": [node("Person")],
            "rel_tables": [],
            "storage": {
                "Person": {"groups": []},
                "person": {"groups": []}
            }
        });

        for value in [duplicate_tables, duplicate_columns, duplicate_storage] {
            assert!(
                matches!(
                    Catalog::decode(&catalog_page(&value)),
                    Err(DevonError::Corrupt { .. })
                ),
                "accepted fold-equal catalog entries: {value}"
            );
        }
    }

    #[test]
    fn storage_object_with_neither_or_both_shapes_is_corruption() {
        let node_schema = node("Person");
        let rel_schema = rel("Knows", "Person", "Person");
        let neither = serde_json::json!({
            "node_tables": [node_schema.clone()],
            "rel_tables": [rel_schema.clone()],
            "storage": {"Knows": {}}
        });
        let both = serde_json::json!({
            "node_tables": [node_schema],
            "rel_tables": [rel_schema],
            "storage": {"Knows": {"groups": [], "fwd": [], "bwd": []}}
        });

        assert!(matches!(
            Catalog::decode(&catalog_page(&neither)),
            Err(DevonError::Corrupt { .. })
        ));
        assert!(matches!(
            Catalog::decode(&catalog_page(&both)),
            Err(DevonError::Corrupt { .. })
        ));
    }

    #[test]
    fn storage_shape_must_match_the_catalog_table_kind() {
        let payload = serde_json::json!({
            "node_tables": [node("Person")],
            "rel_tables": [],
            "storage": {"Person": {"fwd": [], "bwd": []}}
        });

        assert!(matches!(
            Catalog::decode(&catalog_page(&payload)),
            Err(DevonError::Corrupt { .. })
        ));
    }

    #[test]
    fn duplicate_names_are_rejected_across_table_kinds() {
        let mut catalog = Catalog::default();
        catalog.add_node_table(node("Person")).unwrap();

        assert_invalid_argument_mentions(
            catalog.add_rel_table(rel("pErSoN", "Person", "Person")),
            "pErSoN",
        );

        catalog.add_node_table(node("Company")).unwrap();
        catalog
            .add_rel_table(rel("WorksAt", "Person", "Company"))
            .unwrap();
        assert_invalid_argument_mentions(catalog.add_node_table(node("WORKSAT")), "WORKSAT");
    }

    #[test]
    fn relationship_with_missing_endpoint_is_rejected() {
        let mut catalog = Catalog::default();
        catalog.add_node_table(node("Person")).unwrap();

        let error = catalog
            .add_rel_table(rel("WorksAt", "Person", "Company"))
            .unwrap_err();

        let DevonError::NotFound { what } = error else {
            panic!("expected NotFound, got {error}");
        };
        assert!(what.contains("Company"));
    }

    #[test]
    fn corrupted_catalog_payload_is_rejected() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("corrupt.devondb");
        let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
        let mut catalog = Catalog::default();
        catalog.add_node_table(node("Person")).unwrap();
        catalog.save(&pager, 1).unwrap();
        let catalog_root = pager.superblock().catalog_root;
        drop(pager);

        let payload_offset = catalog_root * u64::from(PAGE_SIZE) + HEADER_LEN as u64;
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        file.seek(SeekFrom::Start(payload_offset)).unwrap();
        let mut byte = [0_u8; 1];
        file.read_exact(&mut byte).unwrap();
        byte[0] ^= 0xff;
        file.seek(SeekFrom::Start(payload_offset)).unwrap();
        file.write_all(&byte).unwrap();
        file.sync_all().unwrap();
        drop(file);

        let reopened = Pager::open(path).unwrap();
        assert!(matches!(
            Catalog::load(&reopened),
            Err(DevonError::Corrupt { .. })
        ));
    }

    #[test]
    fn catalog_with_nonzero_padding_is_corruption() {
        let payload = serde_json::json!({
            "node_tables": [node("Person")],
            "rel_tables": [],
            "storage": {}
        });
        let mut page = catalog_page(&payload);
        page[PAGE_SIZE as usize - 1] = 1;

        assert!(matches!(
            Catalog::decode(&page),
            Err(DevonError::Corrupt { .. })
        ));
    }

    #[test]
    fn repeated_saves_use_fresh_pages_and_preserve_the_old_catalog() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("repeated-save.devondb");
        let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
        let mut catalog = Catalog::default();
        catalog.add_node_table(node("Person")).unwrap();

        catalog.save(&pager, 1).unwrap();
        let first_catalog = catalog.clone();
        let first_root = pager.superblock().catalog_root;
        let first_lsn = pager.superblock().checkpoint_lsn;
        let first_file_len = fs::metadata(&path).unwrap().len();
        let first_page = pager.read_page(first_root).unwrap();

        catalog.add_node_table(node("Company")).unwrap();
        catalog
            .add_rel_table(rel("WorksAt", "Person", "Company"))
            .unwrap();
        catalog.save(&pager, first_lsn + 1).unwrap();

        assert_ne!(pager.superblock().catalog_root, first_root);
        assert_eq!(pager.superblock().checkpoint_lsn, first_lsn + 1);
        // The second save writes the new catalog page plus one ledger page
        // recording the old root's retirement.
        assert_eq!(
            fs::metadata(&path).unwrap().len(),
            first_file_len + u64::from(PAGE_SIZE) * 2
        );
        let ledger = pager.free_pages_ledger_pages().unwrap();
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger[0].1.entries.len(), 1);
        assert_eq!(ledger[0].1.entries[0].page_id, first_root);
        assert_eq!(ledger[0].1.entries[0].retired_lsn, first_lsn + 1);
        let preserved_page = pager.read_page(first_root).unwrap();
        assert_eq!(preserved_page, first_page);
        assert_eq!(Catalog::decode(&preserved_page).unwrap(), first_catalog);
        drop(pager);

        let reopened = Pager::open(path).unwrap();
        assert_eq!(Catalog::load(&reopened).unwrap(), catalog);
        assert_eq!(
            reopened.free_pages_extension().unwrap().retired_total,
            1,
            "the reopened extension carries the retirement history"
        );
    }

    #[test]
    fn oversized_catalog_spills_into_a_multipage_save() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("oversized.devondb");
        let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
        let mut catalog = Catalog::default();
        let mut columns = vec![column("id", LogicalType::Int64, true)];
        for index in 0..200 {
            columns.push(column(
                &format!("property_with_a_long_name_{index}"),
                LogicalType::String,
                false,
            ));
        }
        catalog
            .add_node_table(NodeTableSchema::new("Wide".to_owned(), columns).unwrap())
            .unwrap();

        catalog.save(&pager, 1).unwrap();
        assert_ne!(
            pager.superblock().feature_flags & crate::superblock::MULTIPAGE_CATALOG_FLAG,
            0
        );
        assert_eq!(Catalog::load(&pager).unwrap(), catalog);
    }

    #[test]
    fn catalog_directory_overflow_is_rejected_before_page_allocation() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("directory-overflow.devondb");
        let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
        let page_size = PAGE_SIZE as usize;
        // One more continuation page than the directory can list.
        let directory_capacity = (page_size - MULTIPAGE_HEADER_LEN) / 8;
        let payload = vec![7_u8; directory_capacity * page_size + 1];

        assert_invalid_argument_mentions(
            Catalog::save_multipage(&pager, &payload, page_size),
            "continuation pages",
        );
        assert_eq!(fs::metadata(path).unwrap().len(), u64::from(PAGE_SIZE) * 2);
    }

    fn assert_invalid_argument_mentions<T>(result: Result<T, DevonError>, expected: &str) {
        let error = result.err().unwrap();
        let DevonError::InvalidArgument { context } = error else {
            panic!("expected InvalidArgument, got {error}");
        };
        assert!(context.contains(expected));
    }

    fn catalog_page(value: &serde_json::Value) -> Vec<u8> {
        let payload = serde_json::to_vec(value).unwrap();
        let mut page = vec![0_u8; PAGE_SIZE as usize];
        page[..4].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        page[4..8].copy_from_slice(&crc32c::crc32c(&payload).to_le_bytes());
        page[8..8 + payload.len()].copy_from_slice(&payload);
        page
    }
}
