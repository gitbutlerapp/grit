//! On-disk object source for one `objects/` directory (MIDX, packs, loose).

use std::collections::HashSet;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::diagnostics::DiagnosticsHandle;
use crate::error::{Error, Result};
use crate::hash;
use crate::midx::midx_oid_listed_in_tip;
use crate::objects::{HashAlgo, Object, ObjectId, ObjectInfo, ObjectKind};
use crate::pack::{self, PackLookupOptions};
use crate::pack_store::PackStore;

use super::{
    LooseStore, MidxObjects, MidxObjectsStatus, ObjectStore, ObjectStream, PackedObjects,
    WritableObjectStore,
};
use crate::odb::WriteOptions;

/// Read/write object storage backed by one `objects/` directory tree.
pub struct FilesSource {
    objects_dir: PathBuf,
    loose: LooseStore,
    packs: PackedObjects,
    midx: MidxObjects,
    use_midx: bool,
    midx_tip_listing_for_exists: bool,
    _diagnostics: DiagnosticsHandle,
}

impl std::fmt::Debug for FilesSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilesSource")
            .field("objects_dir", &self.objects_dir)
            .field("use_midx", &self.use_midx)
            .finish()
    }
}

impl FilesSource {
    /// Open a files source for `pack_store`'s directory.
    ///
    /// `use_midx` enables multi-pack-index reads when a chain is present.
    /// `midx_tip_listing_for_exists` matches Git's MIDX membership probe for [`ObjectStore::contains`].
    ///
    /// # Errors
    ///
    /// Propagates [`MidxObjects::new`] failures other than a missing index.
    pub fn open(
        pack_store: Arc<PackStore>,
        loose: LooseStore,
        use_midx: bool,
        midx_tip_listing_for_exists: bool,
        diagnostics: DiagnosticsHandle,
    ) -> Result<Self> {
        let midx = MidxObjects::new(Arc::clone(&pack_store), use_midx)?;
        Ok(Self {
            objects_dir: pack_store.objects_dir().to_path_buf(),
            loose,
            packs: PackedObjects::new(pack_store),
            midx,
            use_midx,
            midx_tip_listing_for_exists,
            _diagnostics: diagnostics,
        })
    }

    /// `objects/` root for this source.
    #[must_use]
    pub fn objects_dir(&self) -> &Path {
        &self.objects_dir
    }

    /// Loose-object backend (writes target this store).
    #[must_use]
    pub fn loose_store(&self) -> &LooseStore {
        &self.loose
    }

    /// Shared pack read backend.
    #[must_use]
    pub fn packed_objects(&self) -> &PackedObjects {
        &self.packs
    }

    /// MIDX layer for this directory.
    #[must_use]
    pub fn midx_objects(&self) -> &MidxObjects {
        &self.midx
    }

    /// Whether `oid` is materialized under this `objects/` tree (local loose or non-promisor pack).
    #[must_use]
    pub fn exists_local_materialized(&self, oid: &ObjectId) -> bool {
        super::local_object_materialized(&self.objects_dir, oid)
    }

    /// Resolve duplicate-write short-circuit for this directory before creating a loose file.
    ///
    /// Returns `Some(oid)` when the object is already present locally and was freshened when
    /// appropriate. When `options.trust_new_loose` is set, only an existing loose file stops the
    /// write (callers may still consult alternates).
    pub fn try_finish_duplicate_write(
        &self,
        oid: &ObjectId,
        options: WriteOptions,
    ) -> Result<Option<ObjectId>> {
        if let Some(existing) = self.loose.try_finish_if_loose_present(oid, options)? {
            return Ok(Some(existing));
        }
        if options.trust_new_loose {
            return Ok(None);
        }
        if options.assume_loose_only_existence && self.exists_local_materialized(oid) {
            if self.freshen(oid)? {
                return Ok(Some(*oid));
            }
            // Cruft packs (and other unfreshenable local copies) must not block loose
            // materialization — matches `try_freshen_existing_local` / unpack-objects.
            return Ok(None);
        }
        Ok(None)
    }

    /// Write zlib-compressed store bytes for a precomputed `oid` into this source's loose store.
    ///
    /// # Errors
    ///
    /// Same as [`WritableObjectStore::write`].
    pub fn write_zlib_prehashed(
        &self,
        oid: &ObjectId,
        zlib_store: &[u8],
        options: WriteOptions,
    ) -> Result<ObjectId> {
        if let Some(existing) = self.try_finish_duplicate_write(oid, options)? {
            return Ok(existing);
        }
        self.loose.write_zlib_prehashed(oid, zlib_store, options)
    }

    /// Whether a loose object file exists but could not be parsed (caller may consult alternates).
    pub fn local_loose_exists_but_unreadable(&self, oid: &ObjectId, err: &Error) -> bool {
        if !self.loose.object_path(oid).is_file() {
            return false;
        }
        matches!(
            err,
            Error::CorruptObject(_) | Error::LooseHashMismatch { .. } | Error::Zlib(_)
        )
    }

    fn midx_pack_filter_active(&self) -> bool {
        self.use_midx && self.midx.status() == MidxObjectsStatus::Active
    }

    fn midx_read_should_fallback_to_packs(err: &Error) -> bool {
        matches!(err, Error::CorruptObject(_) | Error::Zlib(_))
    }

    fn try_read_object(&self, oid: &ObjectId) -> Result<Option<Object>> {
        if oid.is_well_known_empty_tree() {
            return Ok(Some(Object {
                kind: ObjectKind::Tree,
                data: Vec::new(),
            }));
        }

        let midx_pack_filter = self.midx_pack_filter_active();
        let pack_opts = PackLookupOptions {
            skip_midx_covered_packs: midx_pack_filter,
            only_midx_covered_packs: false,
        };

        if self.use_midx {
            match self.midx.read(oid) {
                Ok(Some(obj)) => return Ok(Some(obj)),
                Ok(None) => {}
                Err(Error::Midx(_)) => {}
                Err(err) if Self::midx_read_should_fallback_to_packs(&err) => {}
                Err(err) => return Err(err),
            }
        }

        match pack::try_read_object_from_packs_with_options(&self.objects_dir, oid, pack_opts) {
            Ok(obj) => return Ok(Some(obj)),
            Err(Error::ObjectNotFound(_)) => {}
            Err(err) => return Err(err),
        }

        if midx_pack_filter {
            match pack::try_read_object_from_packs_with_options(
                &self.objects_dir,
                oid,
                PackLookupOptions::MIDX_COVERED_PACKS,
            ) {
                Ok(obj) => return Ok(Some(obj)),
                Err(Error::ObjectNotFound(_)) => {}
                Err(err) => return Err(err),
            }
        }

        #[cfg(test)]
        crate::hot_path_test_metrics::record_loose_path_open_for_active_scope();
        self.loose.read(oid)
    }

    fn try_read_info(&self, oid: &ObjectId) -> Result<Option<ObjectInfo>> {
        if oid.is_well_known_empty_tree() {
            return Ok(Some(ObjectInfo {
                kind: ObjectKind::Tree,
                size: 0,
            }));
        }

        let midx_pack_filter = self.midx_pack_filter_active();
        let pack_opts = PackLookupOptions {
            skip_midx_covered_packs: midx_pack_filter,
            only_midx_covered_packs: false,
        };

        if self.use_midx {
            match self.midx.read_info(oid) {
                Ok(Some(info)) => return Ok(Some(info)),
                Ok(None) => {}
                Err(Error::Midx(_)) => {}
                Err(err) if Self::midx_read_should_fallback_to_packs(&err) => {}
                Err(err) => return Err(err),
            }
        }

        match pack::try_read_object_info_from_packs_with_options(&self.objects_dir, oid, pack_opts)
        {
            Ok(info) => return Ok(Some(info)),
            Err(Error::ObjectNotFound(_)) => {}
            Err(err) => return Err(err),
        }

        if midx_pack_filter {
            match pack::try_read_object_info_from_packs_with_options(
                &self.objects_dir,
                oid,
                PackLookupOptions::MIDX_COVERED_PACKS,
            ) {
                Ok(info) => return Ok(Some(info)),
                Err(Error::ObjectNotFound(_)) => {}
                Err(err) => return Err(err),
            }
        }

        #[cfg(test)]
        crate::hot_path_test_metrics::record_loose_path_open_for_active_scope();
        self.loose.read_info(oid)
    }

    fn read_with_pack_retry(&self, oid: &ObjectId) -> Result<Option<Object>> {
        match self.try_read_object(oid) {
            Ok(Some(obj)) => Ok(Some(obj)),
            Ok(None) => {
                if pack::reprepare_pack_directory_on_miss(&self.objects_dir)? {
                    self.try_read_object(oid)
                } else {
                    Ok(None)
                }
            }
            Err(err) => Err(err),
        }
    }

    fn read_info_with_pack_retry(&self, oid: &ObjectId) -> Result<Option<ObjectInfo>> {
        match self.try_read_info(oid) {
            Ok(Some(info)) => Ok(Some(info)),
            Ok(None) => {
                if pack::reprepare_pack_directory_on_miss(&self.objects_dir)? {
                    self.try_read_info(oid)
                } else {
                    Ok(None)
                }
            }
            Err(err) => Err(err),
        }
    }
}

impl ObjectStore for FilesSource {
    fn hash_algo(&self) -> HashAlgo {
        ObjectStore::hash_algo(&self.loose)
    }

    fn read(&self, oid: &ObjectId) -> Result<Option<Object>> {
        PackStore::with_context(self.packs.pack_store().clone(), || {
            self.read_with_pack_retry(oid)
        })
    }

    fn read_info(&self, oid: &ObjectId) -> Result<Option<ObjectInfo>> {
        PackStore::with_context(self.packs.pack_store().clone(), || {
            self.read_info_with_pack_retry(oid)
        })
    }

    fn contains(&self, oid: &ObjectId) -> Result<bool> {
        if oid.is_canonical_empty_tree() {
            return Ok(true);
        }
        if self.loose.object_path(oid).is_file() {
            return Ok(true);
        }
        let indexes = pack::read_local_pack_indexes_cached(&self.objects_dir)?;
        if indexes.is_empty() && !pack_directory_has_index_files(&self.objects_dir) {
            if self.midx_tip_listing_for_exists && self.use_midx {
                match midx_oid_listed_in_tip(&self.objects_dir, oid) {
                    Ok(Some(true)) => return Ok(true),
                    Ok(Some(false)) | Ok(None) => {}
                    Err(_) => return Ok(false),
                }
            }
            return Ok(false);
        }
        PackStore::with_context(self.packs.pack_store().clone(), || {
            let filter = super::PackFilter {
                include_promisor: true,
                include_cruft: true,
            };
            if self.packs.contains_filtered(oid, filter)? {
                return Ok(true);
            }
            if pack::reprepare_pack_directory_on_miss(&self.objects_dir)?
                && self.packs.contains_filtered(oid, filter)?
            {
                return Ok(true);
            }
            if self.midx_tip_listing_for_exists && self.use_midx {
                match midx_oid_listed_in_tip(&self.objects_dir, oid) {
                    Ok(Some(true)) => return Ok(true),
                    Ok(Some(false)) | Ok(None) => {}
                    Err(_) => return Ok(false),
                }
            }
            Ok(false)
        })
    }

    fn open_stream(&self, oid: &ObjectId) -> Result<Option<ObjectStream<'_>>> {
        if oid.is_well_known_empty_tree() {
            return Ok(Some(ObjectStream {
                kind: ObjectKind::Tree,
                size: 0,
                reader: Box::new(std::io::Cursor::new(Vec::<u8>::new())),
            }));
        }
        PackStore::with_context(self.packs.pack_store().clone(), || {
            if self.use_midx {
                if let Ok(Some(stream)) = self.midx.open_stream(oid) {
                    return Ok(Some(stream));
                }
            }
            if let Ok(Some(stream)) = self.packs.open_stream(oid) {
                return Ok(Some(stream));
            }
            self.loose.open_stream(oid)
        })
    }

    fn for_each_object(&self, f: &mut dyn FnMut(&ObjectId) -> ControlFlow<()>) -> Result<()> {
        let mut seen = HashSet::new();
        PackStore::with_context(self.packs.pack_store().clone(), || {
            let mut layer = |store: &dyn ObjectStore| -> Result<bool> {
                super::for_each_propagate_break(store, &mut |oid| {
                    if !seen.insert(*oid) {
                        return ControlFlow::Continue(());
                    }
                    f(oid)
                })
            };
            if self.use_midx && layer(&self.midx)? {
                return Ok(());
            }
            if layer(&self.packs)? {
                return Ok(());
            }
            let _ = layer(&self.loose)?;
            Ok(())
        })
    }

    fn lookup_prefix(&self, prefix: &str, limit: usize, out: &mut Vec<ObjectId>) -> Result<()> {
        let prefix_norm = super::normalize_oid_prefix(prefix, self.hash_algo())?;
        let mut seen: HashSet<ObjectId> = out.iter().copied().collect();
        PackStore::with_context(self.packs.pack_store().clone(), || {
            if self.use_midx {
                merge_prefix_matches(&self.midx, prefix_norm.as_str(), limit, out, &mut seen)?;
                if limit != 0 && out.len() >= limit {
                    return Ok(());
                }
            }
            merge_prefix_matches(&self.packs, prefix_norm.as_str(), limit, out, &mut seen)?;
            if limit != 0 && out.len() >= limit {
                return Ok(());
            }
            merge_prefix_matches(&self.loose, prefix_norm.as_str(), limit, out, &mut seen)
        })
    }

    fn refresh(&self) -> Result<bool> {
        PackStore::with_context(self.packs.pack_store().clone(), || {
            let mut changed = self.packs.refresh()?;
            if self.midx.refresh()? {
                changed = true;
            }
            Ok(changed)
        })
    }
}

impl WritableObjectStore for FilesSource {
    fn write(&self, kind: ObjectKind, data: &[u8], options: WriteOptions) -> Result<ObjectId> {
        let oid = hash::hash_object(self.hash_algo(), kind, data);
        if let Some(existing) = self.try_finish_duplicate_write(&oid, options)? {
            return Ok(existing);
        }
        self.loose.write(kind, data, options)
    }

    fn freshen(&self, oid: &ObjectId) -> Result<bool> {
        if self.loose.freshen(oid)? {
            return Ok(true);
        }
        PackStore::with_context(self.packs.pack_store().clone(), || {
            self.packs.freshen_pack_containing(oid)
        })
    }
}

fn pack_directory_has_index_files(objects_dir: &Path) -> bool {
    let pack_dir = objects_dir.join("pack");
    let Ok(entries) = std::fs::read_dir(pack_dir) else {
        return false;
    };
    entries
        .filter_map(|e| e.ok())
        .any(|ent| ent.path().extension().is_some_and(|ext| ext == "idx"))
}

fn merge_prefix_matches(
    store: &dyn ObjectStore,
    prefix: &str,
    limit: usize,
    out: &mut Vec<ObjectId>,
    seen: &mut HashSet<ObjectId>,
) -> Result<()> {
    store.for_each_object(&mut |oid| {
        if super::oid_hex_has_prefix(oid, prefix) && seen.insert(*oid) {
            out.push(*oid);
            if limit != 0 && out.len() >= limit {
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    })
}
