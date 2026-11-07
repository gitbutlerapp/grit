//! [`Odb::read_store`]: the whole lookup chain of an [`Odb`] as one [`ObjectStore`].

use std::ops::ControlFlow;
use std::sync::Arc;

use crate::error::Result;
use crate::objects::{HashAlgo, Object, ObjectId, ObjectInfo};
use crate::odb::store::{ObjectStore, ObjectStream};

use super::Odb;

/// Read-only [`ObjectStore`] over a cloned [`Odb`] handle (clones share every cache).
///
/// Misses are `Ok(None)` and never start a promisor fetch: a store composed into another
/// repository answers for what this database holds, not for what a remote could send.
#[derive(Debug)]
struct OdbReadStore(Odb);

impl ObjectStore for OdbReadStore {
    fn hash_algo(&self) -> HashAlgo {
        self.0.hash_algo()
    }

    fn read(&self, oid: &ObjectId) -> Result<Option<Object>> {
        self.0.try_read_once(oid)
    }

    fn read_info(&self, oid: &ObjectId) -> Result<Option<ObjectInfo>> {
        self.0.try_read_info_once(oid)
    }

    fn contains(&self, oid: &ObjectId) -> Result<bool> {
        self.0.contains(oid)
    }

    fn contains_many(&self, oids: &[ObjectId]) -> Result<Vec<bool>> {
        self.0.contains_many(oids)
    }

    fn open_stream(&self, oid: &ObjectId) -> Result<Option<ObjectStream<'_>>> {
        self.0.open_stream(oid)
    }

    fn for_each_object(&self, f: &mut dyn FnMut(&ObjectId) -> ControlFlow<()>) -> Result<()> {
        self.0.for_each_object(f)
    }

    fn lookup_prefix(&self, prefix: &str, limit: usize, out: &mut Vec<ObjectId>) -> Result<()> {
        self.0.lookup_prefix(prefix, limit, out)
    }

    fn refresh(&self) -> Result<bool> {
        let odb = &self.0;
        let mut changed = odb.primary_writable()?.refresh()?;
        for source in &odb.extra_read_sources {
            changed |= source.refresh()?;
        }
        if odb.alternates_enabled {
            changed |= odb.sources()?.refresh()?;
        }
        Ok(changed)
    }
}

impl Odb {
    /// Everything this database can read, as one [`ObjectStore`].
    ///
    /// The store walks the same lookup order as [`Self::read`]: the in-memory overlay, the
    /// primary (files-backed or a custom [`RepositoryOptions::with_object_store`](crate::environment::RepositoryOptions::with_object_store)),
    /// each [`OdbBuilder::push_read_source`](super::OdbBuilder::push_read_source) layer, then
    /// alternate object directories when [`OdbBuilder::alternates`](super::OdbBuilder::alternates)
    /// is on. Pass it to another repository's `push_read_source` to layer this one under it
    /// (a read-through cache or mirror), or as the `bases` of
    /// [`ingest_pack_into`](crate::unpack_objects::ingest_pack_into).
    ///
    /// Unlike [`Self::read`], a missing object is `Ok(None)` and never triggers a promisor
    /// fetch. [`ObjectStore::open_stream`] buffers the payload like [`Self::open_stream`];
    /// call [`Self::with_stream`] on the `Odb` itself to stream large objects in bounded
    /// memory. [`ObjectStore::refresh`] refreshes every layer. The returned handle shares
    /// this `Odb`'s caches and stays valid after the `Odb` is dropped.
    ///
    /// [`Self::sources`] is narrower (alternates only) and [`Self::primary`] is files-only;
    /// use [`Self::primary_writable`] for the write side of a custom primary.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::path::Path;
    /// use std::sync::Arc;
    ///
    /// use grit_lib::config::environment::RepositoryOptions;
    /// use grit_lib::objects::odb::store::MemoryStore;
    /// use grit_lib::objects::odb::OdbBuilder;
    /// use grit_lib::objects::{HashAlgo, ObjectKind};
    /// use grit_lib::refs::store::MemoryRefStore;
    /// use grit_lib::repo::Repository;
    ///
    /// # fn main() -> grit_lib::error::Result<()> {
    /// let in_memory = || {
    ///     RepositoryOptions::empty()
    ///         .with_object_store(Arc::new(MemoryStore::new(HashAlgo::Sha1)))
    ///         .with_ref_store(Arc::new(MemoryRefStore::new()))
    /// };
    /// let upstream = Repository::open(Path::new("/nowhere/upstream.git"), &in_memory())?;
    /// let mirror = Repository::open(
    ///     Path::new("/nowhere/mirror.git"),
    ///     &in_memory().with_odb_builder(OdbBuilder::new().push_read_source(upstream.odb().read_store())),
    /// )?;
    /// let oid = upstream.odb().write(ObjectKind::Blob, b"shared")?;
    /// assert!(mirror.odb().contains(&oid)?);
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn read_store(&self) -> Arc<dyn ObjectStore> {
        Arc::new(OdbReadStore(self.clone()))
    }
}
