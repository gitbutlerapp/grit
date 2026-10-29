//! Repository-scoped object database access via [`Objects`].
use crate::change_id::{set_change_id, ChangeIdPolicy};
use crate::error::{Error, Result};
use crate::fsck_standalone::{fsck_object, FsckObjectOptions};
use crate::objects::write::{
    canonical_tree_entries, validate_tree_entry_stored_kinds, WriteTreeOptions,
};
use crate::objects::{
    serialize_commit, serialize_tree, CommitData, NewCommit, Object, ObjectId, ObjectInfo,
    ObjectKind, TreeEntry,
};
use crate::porcelain::commit::stamp_change_id_on_new_commit;
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
    /// # Errors
    ///
    /// Propagates a backend failure when no object source has `oid` (see
    /// [`crate::odb::Odb::contains`]) instead of reporting it as missing.
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
    /// assert!(repo.objects().contains(&oid)?);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    pub fn contains(&self, oid: &ObjectId) -> Result<bool> {
        self.repo.odb().contains(oid)
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

    /// Read `oid` and parse it as a commit (annotated tags are not followed).
    ///
    /// # Errors
    ///
    /// See [`crate::objects::odb::Odb::read_commit`]: a missing object, an object that is not
    /// a commit ([`crate::error::Error::UnexpectedObjectKind`]), or a malformed commit.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> grit_lib::error::Result<()> {
    /// use grit_lib::objects::ObjectKind;
    /// use grit_lib::repo::{InitOptions, Repository};
    /// let dir = std::env::temp_dir().join("grit-objects-read-commit-doc");
    /// let repo = Repository::init(&dir, &InitOptions::default())?;
    /// let tree = repo.objects().write(ObjectKind::Tree, b"")?;
    /// let body = format!(
    ///     "tree {tree}\nauthor A <a@example.com> 0 +0000\ncommitter A <a@example.com> 0 +0000\n\nfirst\n"
    /// );
    /// let commit = repo.objects().write(ObjectKind::Commit, body.as_bytes())?;
    /// assert_eq!(repo.objects().read_commit(&commit)?.message, "first\n");
    /// assert_eq!(repo.objects().commit_tree(&commit)?, tree);
    /// assert!(repo.objects().read_commit(&tree).is_err());
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    pub fn read_commit(&self, oid: &ObjectId) -> Result<CommitData> {
        self.repo.odb().read_commit(oid)
    }

    /// The root tree of the commit `oid`, decoding only the commit headers.
    ///
    /// # Errors
    ///
    /// Same as [`Self::read_commit`].
    #[inline]
    pub fn commit_tree(&self, oid: &ObjectId) -> Result<ObjectId> {
        self.repo.odb().commit_tree(oid)
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

    /// Validate and store an object, returning its id.
    ///
    /// Tree, commit and tag payloads must pass the same checks as `git hash-object -t <kind>`
    /// (without `--literally`): sorted, duplicate-free tree entries with allowed modes and
    /// names, and well-formed `tree`/`parent`/`author`/`committer`/`tagger` headers. Blobs
    /// are stored as given. To build trees and commits from typed values instead of raw
    /// bytes, use [`Self::write_tree`] and [`Self::write_commit`].
    ///
    /// # Errors
    ///
    /// - [`crate::error::Error::InvalidObject`] when the payload would fail `git fsck`;
    ///   nothing is written.
    /// - [`crate::error::Error::HashCollision`] and zlib or filesystem errors from the ODB.
    ///
    /// # Examples
    ///
    /// See also the [`guide_objects`](https://grit-scm.com/docs/library/objects/) example program.
    pub fn write(&self, kind: ObjectKind, data: &[u8]) -> Result<ObjectId> {
        fsck_object(
            kind,
            data,
            FsckObjectOptions::new(self.repo.odb().hash_algo()),
        )
        .map_err(|check| Error::InvalidObject { kind, check })?;
        self.repo.odb().write(kind, data)
    }

    /// Store `data` as an object of `kind` without validating its structure.
    ///
    /// The equivalent of `git hash-object --literally`: use it only to reproduce or test
    /// corrupt objects. Repositories containing such objects fail `git fsck`.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::HashCollision`] and zlib or filesystem errors from the ODB.
    #[inline]
    pub fn write_literally(&self, kind: ObjectKind, data: &[u8]) -> Result<ObjectId> {
        self.repo.odb().write(kind, data)
    }

    /// Write a tree object from `entries`, given in any order.
    ///
    /// Entries are sorted into Git's tree order. Each entry's object must exist in this
    /// repository with the kind implied by its mode (tree or blob; gitlink `160000` entries
    /// may reference a commit that is not stored), matching `git mktree` without `--missing`.
    /// Use
    /// [`Self::write_tree_with`] with [`WriteTreeOptions::allow_missing`] when building
    /// trees bottom-up or referencing objects a partial clone has not fetched.
    ///
    /// # Errors
    ///
    /// - [`crate::error::Error::InvalidTreeEntry`] when an entry has an empty, `.`, `..`,
    ///   `.git`-alias or slash/NUL-containing name, a mode other than `100644`, `100755`,
    ///   `120000`, `40000` or `160000`, the null object id, an object id of the wrong hash
    ///   algorithm, a name over 4096 bytes, a name used by another entry, or a symlink to a
    ///   reserved metadata file name (`.gitmodules`, `.gitattributes`, and related spellings).
    /// - [`crate::error::Error::ObjectNotFound`] when a required entry object is missing.
    /// - [`crate::error::Error::UnexpectedObjectKind`] when a stored object has the wrong kind.
    /// - [`crate::error::Error::HashCollision`] and zlib or filesystem errors from the ODB.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> grit_lib::error::Result<()> {
    /// use grit_lib::objects::{ObjectKind, TreeEntry};
    /// use grit_lib::repo::{InitOptions, Repository};
    /// let dir = tempfile::tempdir()?;
    /// let repo = Repository::init(dir.path(), &InitOptions::new().with_bare(true))?;
    /// let blob = repo.objects().write(ObjectKind::Blob, b"hello\n")?;
    /// let entry = |name: &str| TreeEntry { mode: 0o100644, name: name.into(), oid: blob };
    /// let tree = repo.objects().write_tree([entry("b"), entry("a")])?;
    /// let names: Vec<_> = grit_lib::objects::parse_tree(&repo.objects().read(&tree)?.data)?
    ///     .into_iter()
    ///     .map(|e| e.name)
    ///     .collect();
    /// assert_eq!(names, [b"a".to_vec(), b"b".to_vec()]);
    /// # Ok(())
    /// # }
    /// ```
    pub fn write_tree(&self, entries: impl IntoIterator<Item = TreeEntry>) -> Result<ObjectId> {
        self.write_tree_with(entries, WriteTreeOptions::default())
    }

    /// Write a tree object from `entries` with `options`.
    ///
    /// Same validation and sorting as [`Self::write_tree`], except that missing blob and
    /// tree oids are accepted when [`WriteTreeOptions::allow_missing`] is set.
    ///
    /// # Errors
    ///
    /// Same as [`Self::write_tree`].
    pub fn write_tree_with(
        &self,
        entries: impl IntoIterator<Item = TreeEntry>,
        options: WriteTreeOptions,
    ) -> Result<ObjectId> {
        let odb = self.repo.odb();
        let algo = odb.hash_algo();
        let canonical = canonical_tree_entries(entries, algo)?;
        validate_tree_entry_stored_kinds(odb, &canonical, options.allow_missing)?;
        let data = serialize_tree(&canonical);
        debug_assert!(
            fsck_object(ObjectKind::Tree, &data, FsckObjectOptions::new(algo)).is_ok(),
            "canonical_tree_entries let through a tree fsck rejects"
        );
        odb.write(ObjectKind::Tree, &data)
    }

    /// Write the commit described by `commit` and return its id.
    ///
    /// The tree must exist and be a tree, and every parent must exist and be a commit
    /// (the checks `git commit-tree` makes). When `grit.changeIds` is enabled (the
    /// default) and no id was set with [`NewCommit::with_change_id`], the commit gets a
    /// fresh `change-id` header. The commit is not signed and no ref is updated; use
    /// [`crate::porcelain::commit::create_commit`] to commit the index onto a branch.
    ///
    /// # Errors
    ///
    /// - [`crate::error::Error::InvalidSignature`] when the author or committer has a
    ///   `<`, `>` or control character in its name or email, a time before the Unix
    ///   epoch, or a UTC offset with a seconds component.
    /// - [`crate::error::Error::ObjectNotFound`] when the tree or a parent is missing, and
    ///   [`crate::error::Error::UnexpectedObjectKind`] when one has the wrong kind.
    /// - Config loading errors from [`Repository::config`] (read for `grit.changeIds`).
    /// - The errors of [`Self::write`].
    ///
    /// # Examples
    ///
    /// See [`NewCommit`].
    pub fn write_commit(&self, commit: &NewCommit) -> Result<ObjectId> {
        for signature in commit.signatures() {
            signature.validate()?;
        }
        let (tree, parents) = commit.tree_and_parents();
        self.expect_kind(tree, ObjectKind::Tree)?;
        for parent in parents {
            self.expect_kind(parent, ObjectKind::Commit)?;
        }
        let mut data = commit.to_commit_data();
        match commit.change_id() {
            Some(id) => set_change_id(&mut data, id),
            None => {
                let policy = ChangeIdPolicy::from_config(self.repo.config()?.as_ref());
                stamp_change_id_on_new_commit(self.repo, &policy, &mut data)?;
            }
        }
        self.write(ObjectKind::Commit, &serialize_commit(&data))
    }

    fn expect_kind(&self, oid: &ObjectId, expected: ObjectKind) -> Result<()> {
        let actual = self.read_info(oid)?.kind;
        if actual != expected {
            return Err(Error::UnexpectedObjectKind {
                oid: *oid,
                expected,
                actual,
            });
        }
        Ok(())
    }
}
