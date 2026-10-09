//! Multi-pack-index object backend ([`MidxObjects`]).
//!
//! OID membership and prefix search use the prepared MIDX chain; object bytes are read through
//! [`PackedObjects`] restricted to MIDX-listed pack indexes.

use std::collections::HashSet;
use std::io::Cursor;
use std::ops::ControlFlow;
use std::sync::{Arc, RwLock};

use crate::error::{Error, Result};
use crate::midx::{prepared_midx_chain, PreparedMidxChain};
use crate::objects::{HashAlgo, Object, ObjectId, ObjectInfo};
use crate::pack::clear_pack_cache;
use crate::pack_store::PackStore;

use super::{normalize_oid_prefix, ObjectStore, ObjectStream, PackedObjects};

/// Whether [`MidxObjects`] can serve reads for this repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MidxObjectsStatus {
    /// `core.multiPackIndex` was off at construction; this backend is inert.
    Disabled,
    /// A usable MIDX chain is loaded.
    Active,
    /// MIDX was requested but no layer could be loaded (missing or corrupt); fall back to packs.
    Unusable,
}

/// MIDX-backed read-only [`ObjectStore`] for one `objects/` directory.
pub struct MidxObjects {
    packs: PackedObjects,
    multi_pack_index: bool,
    status: RwLock<MidxObjectsStatus>,
}

impl std::fmt::Debug for MidxObjects {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MidxObjects")
            .field("objects_dir", &self.packs.pack_store().objects_dir())
            .field("multi_pack_index", &self.multi_pack_index)
            .field("status", &self.status())
            .finish()
    }
}

impl MidxObjects {
    /// Open MIDX reads for `store`'s `objects/` directory.
    ///
    /// `multi_pack_index` mirrors `core.multiPackIndex` at construction time; the store does not
    /// read config itself.
    ///
    /// # Errors
    ///
    /// Propagates MIDX chain preparation failures other than a missing or skipped index.
    pub fn new(store: Arc<PackStore>, multi_pack_index: bool) -> Result<Self> {
        let packs = PackedObjects::new(Arc::clone(&store));
        let status = if multi_pack_index {
            classify_midx_status(store.objects_dir())?
        } else {
            MidxObjectsStatus::Disabled
        };
        Ok(Self {
            packs,
            multi_pack_index,
            status: RwLock::new(status),
        })
    }

    /// Whether MIDX-backed reads are active for this store.
    #[must_use]
    pub fn status(&self) -> MidxObjectsStatus {
        self.status
            .read()
            .map(|g| *g)
            .unwrap_or(MidxObjectsStatus::Unusable)
    }

    /// Whether `core.multiPackIndex` was enabled when this store was built.
    #[must_use]
    pub fn multi_pack_index_enabled(&self) -> bool {
        self.multi_pack_index
    }

    /// Pack index basenames covered by the active MIDX chain (empty when inactive).
    ///
    /// Callers skip these indexes in [`PackedObjects`] so MIDX-covered OIDs are not scanned twice.
    ///
    /// # Errors
    ///
    /// Propagates MIDX chain load failures.
    pub fn covered_pack_basenames(&self) -> Result<HashSet<String>> {
        let Some(chain) = self.active_chain()? else {
            return Ok(HashSet::new());
        };
        Ok(chain.covered_pack_basenames())
    }

    /// Underlying pack backend used for object payload reads.
    #[must_use]
    pub fn packed_objects(&self) -> &PackedObjects {
        &self.packs
    }

    fn active_chain(&self) -> Result<Option<Arc<PreparedMidxChain>>> {
        if self.status() != MidxObjectsStatus::Active {
            return Ok(None);
        }
        prepared_midx_chain(self.packs.pack_store().objects_dir())
    }

    fn oid_listed(&self, oid: &ObjectId) -> Result<bool> {
        let Some(chain) = self.active_chain()? else {
            return Ok(false);
        };
        Ok(chain.oid_listed_in_tip(oid))
    }
}

impl ObjectStore for MidxObjects {
    fn hash_algo(&self) -> HashAlgo {
        self.packs.hash_algo()
    }

    fn read(&self, oid: &ObjectId) -> Result<Option<Object>> {
        if !self.oid_listed(oid)? {
            return Ok(None);
        }
        self.packs.read_midx_covered(oid)
    }

    fn read_info(&self, oid: &ObjectId) -> Result<Option<ObjectInfo>> {
        if !self.oid_listed(oid)? {
            return Ok(None);
        }
        self.packs.read_info_midx_covered(oid)
    }

    fn open_stream(&self, oid: &ObjectId) -> Result<Option<ObjectStream<'_>>> {
        let Some(info) = self.read_info(oid)? else {
            return Ok(None);
        };
        let Some(obj) = self.read(oid)? else {
            return Ok(None);
        };
        let size = u64::try_from(obj.data.len()).map_err(|_| {
            Error::CorruptObject(format!("object size overflow for {}", oid.to_hex()))
        })?;
        Ok(Some(ObjectStream {
            kind: info.kind,
            size,
            reader: Box::new(Cursor::new(obj.data)),
        }))
    }

    fn for_each_object(&self, f: &mut dyn FnMut(&ObjectId) -> ControlFlow<()>) -> Result<()> {
        let Some(chain) = self.active_chain()? else {
            return Ok(());
        };
        chain.for_each_listed_object(f)
    }

    fn lookup_prefix(&self, prefix: &str, limit: usize, out: &mut Vec<ObjectId>) -> Result<()> {
        let Some(chain) = self.active_chain()? else {
            return Ok(());
        };
        let prefix = normalize_oid_prefix(prefix, self.hash_algo())?;
        chain.lookup_prefix(prefix.as_str(), limit, out)
    }

    fn refresh(&self) -> Result<bool> {
        if !self.multi_pack_index {
            return Ok(false);
        }
        let objects_dir = self.packs.pack_store().objects_dir();
        clear_pack_cache();
        crate::midx::evict_midx_read_cache_for_pack_dir(&objects_dir.join("pack"));
        let new_status = classify_midx_status(objects_dir)?;
        let mut guard = self
            .status
            .write()
            .map_err(|e| Error::Io(std::io::Error::other(e.to_string())))?;
        let changed = *guard != new_status;
        *guard = new_status;
        Ok(changed)
    }
}

fn classify_midx_status(objects_dir: &std::path::Path) -> Result<MidxObjectsStatus> {
    match prepared_midx_chain(objects_dir)? {
        Some(_) => Ok(MidxObjectsStatus::Active),
        None => Ok(MidxObjectsStatus::Unusable),
    }
}
