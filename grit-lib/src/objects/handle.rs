//! Repository-scoped object database access via [`Objects`].
use crate::error::Result;
use crate::objects::{Object, ObjectId, ObjectInfo, ObjectKind};
use crate::repo::Repository;

/// Read and write Git objects through a [`Repository`] handle.
#[derive(Debug, Copy, Clone)]
pub struct Objects<'repo> {
    repo: &'repo Repository,
}

impl<'repo> Objects<'repo> {
    /// Bind object operations to `repo`.
    #[inline]
    #[must_use]
    pub fn new(repo: &'repo Repository) -> Self {
        Self { repo }
    }

    /// Underlying repository.
    #[inline]
    #[must_use]
    pub fn repository(&self) -> &'repo Repository {
        self.repo
    }

    /// Returns whether `oid` is present in this repository's object database.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> grit_lib::error::Result<()> {
    /// use grit_lib::objects::ObjectKind;
    /// use grit_lib::repo::{InitOptions, Repository};
    /// let dir = std::env::temp_dir().join("grit-objects-handle-doc");
    /// let repo = Repository::init(&dir, &InitOptions::default())?;
    /// let oid = repo.objects().write(ObjectKind::Blob, b"hello")?;
    /// assert!(repo.objects().contains(&oid));
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    pub fn contains(&self, oid: &ObjectId) -> bool {
        self.repo.odb().exists(oid)
    }

    /// Read the object named `oid`.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::Error::ObjectNotFound`] when the object is missing.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> grit_lib::error::Result<()> {
    /// use grit_lib::objects::ObjectKind;
    /// use grit_lib::repo::{InitOptions, Repository};
    /// let dir = std::env::temp_dir().join("grit-objects-read-doc");
    /// let repo = Repository::init(&dir, &InitOptions::default())?;
    /// let oid = repo.objects().write(ObjectKind::Blob, b"payload")?;
    /// let obj = repo.objects().read(&oid)?;
    /// assert_eq!(obj.data, b"payload");
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    pub fn read(&self, oid: &ObjectId) -> Result<Object> {
        self.repo.odb().read(oid)
    }

    /// Read object type and size without inflating full blob bodies when possible.
    ///
    /// # Errors
    ///
    /// Propagates object lookup failures from the ODB.
    #[inline]
    pub fn read_info(&self, oid: &ObjectId) -> Result<ObjectInfo> {
        self.repo.odb().read_info(oid)
    }

    /// Hash `data` as a Git object of `kind` without writing to disk.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::Error::HashCollision`] when sha1dc detects a chosen-prefix collision.
    #[inline]
    pub fn hash(&self, kind: ObjectKind, data: &[u8]) -> Result<ObjectId> {
        self.repo.odb().hash(kind, data)
    }

    /// Store a loose object and return its id.
    ///
    /// # Errors
    ///
    /// Propagates zlib or filesystem errors from the ODB.
    ///
    /// # Examples
    ///
    /// See also the [`guide_objects`](https://grit-scm.com/docs/library/objects/) example program.
    #[inline]
    pub fn write(&self, kind: ObjectKind, data: &[u8]) -> Result<ObjectId> {
        self.repo.odb().write(kind, data)
    }
}
