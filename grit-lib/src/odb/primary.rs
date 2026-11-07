//! The store behind an [`Odb`](super::Odb)'s primary object database.

use std::sync::{Arc, OnceLock};

use crate::error::{Error, Result};
use crate::objects::{Object, ObjectId, ObjectInfo, ObjectKind};
use crate::repo::error::FilesOnlyOperation;
use crate::repo::RepositoryError;

use super::store::{FilesSource, ObjectStore, ObjectStream, WritableObjectStore};

/// Which store an [`Odb`](super::Odb) reads and writes first.
///
/// The files arm holds the lazily opened source for `objects/` inline, so reads through it
/// are statically dispatched; only an injected store goes through a trait object.
#[derive(Clone)]
pub(crate) enum PrimaryStore {
    /// The on-disk `objects/` layout. The source is opened on first use and shared by every
    /// clone of the [`Odb`](super::Odb).
    Files(Arc<OnceLock<Arc<FilesSource>>>),
    /// A store injected with [`OdbBuilder::primary`](super::OdbBuilder::primary).
    Custom(Arc<dyn WritableObjectStore>),
}

impl PrimaryStore {
    /// A files primary whose source has not been opened yet.
    pub(crate) fn files() -> Self {
        Self::Files(Arc::new(OnceLock::new()))
    }

    /// Whether an injected store replaced the files layout.
    pub(crate) fn is_custom(&self) -> bool {
        matches!(self, Self::Custom(_))
    }

    /// Refuse `operation` unless the primary is the files layout.
    ///
    /// # Errors
    ///
    /// [`RepositoryError::UnsupportedObjectStore`] naming `operation` for a custom primary.
    pub(crate) fn require_files_primary(&self, operation: FilesOnlyOperation) -> Result<()> {
        match self {
            Self::Files(_) => Ok(()),
            Self::Custom(_) => Err(RepositoryError::UnsupportedObjectStore { operation }.into()),
        }
    }
}

/// The primary store, opened and borrowed for one operation.
///
/// Borrowing instead of cloning the `Arc` keeps per-object reads free of reference-count
/// traffic, and the files arm calls [`FilesSource`] directly rather than through `dyn`.
#[derive(Clone, Copy)]
pub(crate) enum OpenPrimary<'a> {
    /// The opened files source for `objects/`.
    Files(&'a FilesSource),
    /// The injected store.
    Custom(&'a dyn WritableObjectStore),
}

impl<'a> OpenPrimary<'a> {
    /// The primary as a writable trait object, for code that is generic over stores.
    pub(crate) fn as_writable(self) -> &'a dyn WritableObjectStore {
        match self {
            Self::Files(files) => files,
            Self::Custom(store) => store,
        }
    }

    /// [`ObjectStore::read`] on the primary.
    pub(crate) fn read(self, oid: &ObjectId) -> Result<Option<Object>> {
        match self {
            Self::Files(files) => files.read(oid),
            Self::Custom(store) => store.read(oid),
        }
    }

    /// [`ObjectStore::read_into`] on the primary.
    pub(crate) fn read_into(self, oid: &ObjectId, buf: &mut Vec<u8>) -> Result<Option<ObjectKind>> {
        match self {
            Self::Files(files) => files.read_into(oid, buf),
            Self::Custom(store) => store.read_into(oid, buf),
        }
    }

    /// [`ObjectStore::read_info`] on the primary.
    pub(crate) fn read_info(self, oid: &ObjectId) -> Result<Option<ObjectInfo>> {
        match self {
            Self::Files(files) => files.read_info(oid),
            Self::Custom(store) => store.read_info(oid),
        }
    }

    /// [`ObjectStore::contains`] on the primary.
    pub(crate) fn contains(self, oid: &ObjectId) -> Result<bool> {
        match self {
            Self::Files(files) => files.contains(oid),
            Self::Custom(store) => store.contains(oid),
        }
    }

    /// [`ObjectStore::contains_many`] on the primary.
    pub(crate) fn contains_many(self, oids: &[ObjectId]) -> Result<Vec<bool>> {
        match self {
            Self::Files(files) => files.contains_many(oids),
            Self::Custom(store) => store.contains_many(oids),
        }
    }

    /// [`ObjectStore::open_stream`] on the primary.
    pub(crate) fn open_stream(self, oid: &ObjectId) -> Result<Option<ObjectStream<'a>>> {
        match self {
            Self::Files(files) => files.open_stream(oid),
            Self::Custom(store) => store.open_stream(oid),
        }
    }

    /// Whether `err` came from a loose file in `objects/` that exists but cannot be parsed,
    /// so a later source may still hold a good copy. Always `false` for a custom primary.
    pub(crate) fn local_loose_exists_but_unreadable(self, oid: &ObjectId, err: &Error) -> bool {
        match self {
            Self::Files(files) => files.local_loose_exists_but_unreadable(oid, err),
            Self::Custom(_) => false,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::Arc;

    use tempfile::TempDir;

    use super::{OpenPrimary, PrimaryStore};
    use crate::error::{Error, ObjectError};
    use crate::objects::{HashAlgo, ObjectKind};
    use crate::odb::store::{MemoryStore, WritableObjectStore};
    use crate::odb::{Odb, OdbBuilder, WriteOptions};
    use crate::repo::error::FilesOnlyOperation;
    use crate::repo::RepositoryError;

    fn objects_dir() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let objects = dir.path().join("objects");
        std::fs::create_dir_all(&objects).unwrap();
        (dir, objects)
    }

    fn custom_odb(objects: &std::path::Path) -> (Arc<MemoryStore>, Odb) {
        let store = Arc::new(MemoryStore::new(HashAlgo::Sha1));
        let odb = OdbBuilder::new()
            .alternates(false)
            .build(objects, Some(store.clone()));
        (store, odb)
    }

    fn same_store(a: &Arc<dyn WritableObjectStore>, b: &Arc<MemoryStore>) -> bool {
        std::ptr::addr_eq(Arc::as_ptr(a), Arc::as_ptr(b))
    }

    #[test]
    fn files_source_is_opened_once_and_shared_by_clones() {
        let (_dir, objects) = objects_dir();
        let odb = Odb::new(&objects);
        assert!(!odb.primary_store().is_custom());
        let first = odb.primary().unwrap();
        assert!(Arc::ptr_eq(&first, &odb.primary().unwrap()));
        let clone = odb.clone();
        assert!(Arc::ptr_eq(&first, &clone.primary().unwrap()));
    }

    #[test]
    fn bounded_view_reopens_the_files_source_but_keeps_a_custom_store() {
        let (_dir, objects) = objects_dir();
        let odb = Odb::new(&objects);
        let shared = odb.primary().unwrap();
        let bounded = odb.with_bounded_pack_cache();
        assert!(!Arc::ptr_eq(&shared, &bounded.primary().unwrap()));
        assert!(Arc::ptr_eq(&shared, &odb.primary().unwrap()));

        let (store, odb) = custom_odb(&objects);
        let bounded = odb.with_bounded_pack_cache();
        assert!(bounded.primary_store().is_custom());
        assert!(same_store(&bounded.primary_writable().unwrap(), &store));
    }

    #[test]
    fn custom_primary_refuses_files_only_operations() {
        let (_dir, objects) = objects_dir();
        let (_store, odb) = custom_odb(&objects);
        let primary = odb.primary_store();
        assert!(primary.is_custom());
        let err = primary
            .require_files_primary(FilesOnlyOperation::Repack)
            .unwrap_err();
        assert!(matches!(
            err,
            Error::Repository(RepositoryError::UnsupportedObjectStore {
                operation: FilesOnlyOperation::Repack
            })
        ));
        assert!(matches!(
            odb.primary(),
            Err(Error::Repository(RepositoryError::UnsupportedObjectStore {
                operation: FilesOnlyOperation::FilesSource
            }))
        ));
        assert!(odb.files_objects_dir().is_none());
        assert!(PrimaryStore::files()
            .require_files_primary(FilesOnlyOperation::Repack)
            .is_ok());
    }

    #[test]
    fn open_primary_dispatches_to_the_store_behind_each_arm() {
        let (_dir, objects) = objects_dir();
        let (store, odb) = custom_odb(&objects);
        let oid = WritableObjectStore::write(
            store.as_ref(),
            ObjectKind::Blob,
            b"custom",
            WriteOptions::default(),
        )
        .unwrap();
        let open = odb.open_primary().unwrap();
        assert!(matches!(open, OpenPrimary::Custom(_)));
        assert_eq!(open.read(&oid).unwrap().unwrap().data, b"custom");
        assert!(open.contains(&oid).unwrap());
        assert_eq!(open.contains_many(&[oid]).unwrap(), vec![true]);
        assert_eq!(open.read_info(&oid).unwrap().unwrap().size, 6);
        let mut buf = Vec::new();
        assert_eq!(
            open.read_into(&oid, &mut buf).unwrap(),
            Some(ObjectKind::Blob)
        );
        assert!(open.open_stream(&oid).unwrap().is_some());
        let corrupt = Error::Object(ObjectError::Zlib {
            detail: "x".to_owned(),
        });
        assert!(!open.local_loose_exists_but_unreadable(&oid, &corrupt));
        assert!(!odb.object_path(&oid).exists());

        let files_odb = Odb::new(&objects);
        let oid = files_odb.write(ObjectKind::Blob, b"files").unwrap();
        let open = files_odb.open_primary().unwrap();
        assert!(matches!(open, OpenPrimary::Files(_)));
        assert_eq!(open.read(&oid).unwrap().unwrap().data, b"files");
        assert!(open.as_writable().contains(&oid).unwrap());
        assert!(open.local_loose_exists_but_unreadable(&oid, &corrupt));
    }
}
