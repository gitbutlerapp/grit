//! Typed inputs for building tree and commit objects without an index or work tree.
//!
//! [`Objects::write_tree`](super::Objects::write_tree) and
//! [`Objects::write_commit`](super::Objects::write_commit) take these types and
//! produce objects that pass `git fsck --strict`, including checks on referenced oids.

use std::collections::HashSet;

use crate::change_id::ChangeId;
use crate::error::Error;
use crate::fsck_standalone::{tree_entry_mode_allowed, MAX_TREE_ENTRY_LEN};
use crate::gitmodules::tree_entry_reserved_dot_symlink;
use crate::ident::Signature;
use crate::objects::{tree_entry_cmp, CommitData, HashAlgo, ObjectId, ObjectKind, TreeEntry};
use crate::odb::Odb;
use crate::path_validate::{audit_tree_entry_name, PathComponentError, ProtectOptions};

const MODE_TREE: u32 = 0o040000;
const MODE_GITLINK: u32 = 0o160000;

/// Why [`Objects::write_tree`](super::Objects::write_tree) refused an entry.
///
/// Each variant carries the offending entry name as raw bytes (tree entry names
/// are not required to be UTF-8).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TreeEntryError {
    /// The name is empty, `.`, `..`, a `.git` alias, or contains `/` or NUL.
    #[error("invalid tree entry name '{}': {reason}", String::from_utf8_lossy(.name))]
    InvalidName {
        /// Entry name as given.
        name: Vec<u8>,
        /// Which path-component rule the name breaks.
        reason: PathComponentError,
    },
    /// The name is longer than the 4096 bytes `git fsck --strict` accepts.
    #[error(
        "tree entry name of {} bytes exceeds the {max}-byte limit",
        .name.len()
    )]
    NameTooLong {
        /// Entry name.
        name: Vec<u8>,
        /// Longest accepted name, in bytes.
        max: usize,
    },
    /// The mode is not one of `100644`, `100755`, `120000`, `40000` or `160000`.
    #[error("tree entry '{}' has unsupported mode {mode:o}", String::from_utf8_lossy(.name))]
    InvalidMode {
        /// Entry name.
        name: Vec<u8>,
        /// Mode as given.
        mode: u32,
    },
    /// The entry points at the all-zero object id.
    #[error("tree entry '{}' points at the null object id", String::from_utf8_lossy(.name))]
    NullObjectId {
        /// Entry name.
        name: Vec<u8>,
    },
    /// The entry's object id uses a different hash algorithm than the repository.
    #[error(
        "tree entry '{}' has a {} object id in a {} repository",
        String::from_utf8_lossy(.name),
        .actual.name(),
        .expected.name()
    )]
    HashAlgoMismatch {
        /// Entry name.
        name: Vec<u8>,
        /// The repository's object format.
        expected: HashAlgo,
        /// The algorithm of the entry's object id.
        actual: HashAlgo,
    },
    /// Two entries share a name (including a file and a directory of the same name).
    #[error("tree lists '{}' more than once", String::from_utf8_lossy(.name))]
    DuplicateName {
        /// The repeated name.
        name: Vec<u8>,
    },
    /// A symlink entry uses a name reserved for Git metadata files.
    #[error(
        "tree entry '{}' must not be a symlink to {reserved_file}",
        String::from_utf8_lossy(.name)
    )]
    ForbiddenDotSymlink {
        /// Entry name as given.
        name: Vec<u8>,
        /// Which metadata file name the entry matched.
        reserved_file: &'static str,
    },
}

/// Options for [`Objects::write_tree_with`](super::Objects::write_tree_with).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WriteTreeOptions {
    /// When `true`, blob and tree entries may reference oids not stored locally (partial
    /// clone and bottom-up tree assembly). Wrong kinds are still rejected.
    pub allow_missing: bool,
}

/// Validate `entries` for `algo` and return them in canonical Git tree order.
///
/// The checks cover every rule `git fsck --strict` applies to a tree, so the
/// serialized result needs no second fsck pass.
///
/// # Errors
///
/// The first [`TreeEntryError`] found, checking each entry in input order and
/// then duplicate names in sorted order.
pub(crate) fn canonical_tree_entries(
    entries: impl IntoIterator<Item = TreeEntry>,
    algo: HashAlgo,
) -> Result<Vec<TreeEntry>, TreeEntryError> {
    let mut entries: Vec<TreeEntry> = entries.into_iter().collect();
    for entry in &entries {
        validate_tree_entry(entry, algo)?;
    }
    entries
        .sort_by(|a, b| tree_entry_cmp(&a.name, a.mode == MODE_TREE, &b.name, b.mode == MODE_TREE));
    let mut seen = HashSet::with_capacity(entries.len());
    if let Some(dup) = entries.iter().find(|e| !seen.insert(e.name.as_slice())) {
        return Err(TreeEntryError::DuplicateName {
            name: dup.name.clone(),
        });
    }
    Ok(entries)
}

/// Check each entry's oid is stored with the kind implied by its mode.
///
/// # Errors
///
/// - [`Error::ObjectNotFound`] when an entry requires a stored object that is missing.
/// - [`Error::UnexpectedObjectKind`] when a stored object has the wrong kind.
pub(crate) fn validate_tree_entry_stored_kinds(
    odb: &Odb,
    entries: &[TreeEntry],
    allow_missing: bool,
) -> Result<(), Error> {
    for entry in entries {
        validate_one_tree_entry_stored_kind(odb, entry, allow_missing)?;
    }
    Ok(())
}

fn expected_kind_for_tree_mode(mode: u32) -> ObjectKind {
    match mode {
        MODE_TREE => ObjectKind::Tree,
        MODE_GITLINK => ObjectKind::Commit,
        _ => ObjectKind::Blob,
    }
}

fn validate_one_tree_entry_stored_kind(
    odb: &Odb,
    entry: &TreeEntry,
    allow_missing: bool,
) -> Result<(), Error> {
    let expected = expected_kind_for_tree_mode(entry.mode);
    let stored = odb.stored_object_kind(&entry.oid)?;
    if entry.mode == MODE_GITLINK {
        if let Some(actual) = stored {
            if actual != ObjectKind::Commit {
                return Err(Error::UnexpectedObjectKind {
                    oid: entry.oid,
                    expected: ObjectKind::Commit,
                    actual,
                });
            }
        }
        return Ok(());
    }
    let Some(actual) = stored else {
        if allow_missing {
            return Ok(());
        }
        return Err(Error::ObjectNotFound(entry.oid.to_hex()));
    };
    if actual != expected {
        return Err(Error::UnexpectedObjectKind {
            oid: entry.oid,
            expected,
            actual,
        });
    }
    Ok(())
}

fn validate_tree_entry(entry: &TreeEntry, algo: HashAlgo) -> Result<(), TreeEntryError> {
    audit_tree_entry_name(&entry.name, ProtectOptions::fsck())
        .first_error()
        .map_err(|reason| TreeEntryError::InvalidName {
            name: entry.name.clone(),
            reason,
        })?;
    if entry.name.len() > MAX_TREE_ENTRY_LEN {
        return Err(TreeEntryError::NameTooLong {
            name: entry.name.clone(),
            max: MAX_TREE_ENTRY_LEN,
        });
    }
    if !tree_entry_mode_allowed(entry.mode) {
        return Err(TreeEntryError::InvalidMode {
            name: entry.name.clone(),
            mode: entry.mode,
        });
    }
    if entry.oid.algo() != algo {
        return Err(TreeEntryError::HashAlgoMismatch {
            name: entry.name.clone(),
            expected: algo,
            actual: entry.oid.algo(),
        });
    }
    if entry.oid.is_zero() {
        return Err(TreeEntryError::NullObjectId {
            name: entry.name.clone(),
        });
    }
    if let Some(reserved_file) = tree_entry_reserved_dot_symlink(entry.mode, &entry.name) {
        return Err(TreeEntryError::ForbiddenDotSymlink {
            name: entry.name.clone(),
            reserved_file,
        });
    }
    Ok(())
}

/// A commit to create with [`Objects::write_commit`](super::Objects::write_commit).
///
/// Identities are typed [`Signature`]s, so the `author` and `committer` header
/// lines always have Git's `Name <email> <unix> <±HHMM>` shape.
///
/// # Examples
///
/// ```
/// # fn main() -> grit_lib::error::Result<()> {
/// use grit_lib::ident::Signature;
/// use grit_lib::objects::{NewCommit, ObjectKind, TreeEntry};
/// use grit_lib::repo::{InitOptions, Repository};
/// use time::OffsetDateTime;
///
/// let dir = tempfile::tempdir()?;
/// let repo = Repository::init(dir.path(), &InitOptions::new().with_bare(true))?;
/// let blob = repo.objects().write(ObjectKind::Blob, b"hello\n")?;
/// let tree = repo.objects().write_tree([TreeEntry {
///     mode: 0o100644,
///     name: b"README".to_vec(),
///     oid: blob,
/// }])?;
/// let when = OffsetDateTime::from_unix_timestamp(1_700_000_000).map_err(|e| {
///     grit_lib::error::Error::Message(e.to_string())
/// })?;
/// let ada = Signature::new("Ada Lovelace", "ada@example.com", when)?;
/// let commit = repo
///     .objects()
///     .write_commit(&NewCommit::new(tree, ada.clone(), ada, "Initial commit"))?;
/// assert_eq!(repo.objects().read_commit(&commit)?.message, "Initial commit\n");
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewCommit {
    tree: ObjectId,
    parents: Vec<ObjectId>,
    author: Signature,
    committer: Signature,
    message: String,
    change_id: Option<ChangeId>,
}

impl NewCommit {
    /// A root commit of `tree` with no parents.
    ///
    /// # Parameters
    ///
    /// - `tree`: the commit's root tree (must exist in the repository when written).
    /// - `author`, `committer`: identities and timestamps for the two header lines.
    /// - `message`: commit message; a trailing newline is added when missing.
    #[must_use]
    pub fn new(
        tree: ObjectId,
        author: Signature,
        committer: Signature,
        message: impl Into<String>,
    ) -> Self {
        Self {
            tree,
            parents: Vec::new(),
            author,
            committer,
            message: message.into(),
            change_id: None,
        }
    }

    /// Replace the parent list (first parent first; each must name a commit when written).
    #[must_use]
    pub fn with_parents(mut self, parents: impl IntoIterator<Item = ObjectId>) -> Self {
        self.parents = parents.into_iter().collect();
        self
    }

    /// Record `id` as the commit's `change-id` header instead of assigning a fresh one.
    ///
    /// Use this when the new commit rewrites an existing change (amend, rebase) so the
    /// change keeps its identity. Without it, [`Objects::write_commit`](super::Objects::write_commit)
    /// assigns a new id when `grit.changeIds` is enabled (the default).
    #[must_use]
    pub fn with_change_id(mut self, id: ChangeId) -> Self {
        self.change_id = Some(id);
        self
    }

    /// Explicit change id set with [`Self::with_change_id`].
    pub(crate) fn change_id(&self) -> Option<&ChangeId> {
        self.change_id.as_ref()
    }

    /// The tree and parents, for existence checks before writing.
    pub(crate) fn tree_and_parents(&self) -> (&ObjectId, &[ObjectId]) {
        (&self.tree, &self.parents)
    }

    /// The two identities, for validation before writing.
    pub(crate) fn signatures(&self) -> [&Signature; 2] {
        [&self.author, &self.committer]
    }

    /// Commit fields with header lines rendered from the signatures and no extra headers.
    pub(crate) fn to_commit_data(&self) -> CommitData {
        let mut message = self.message.clone();
        if !message.is_empty() && !message.ends_with('\n') {
            message.push('\n');
        }
        CommitData {
            tree: self.tree,
            parents: self.parents.clone(),
            author: self.author.header_line(),
            committer: self.committer.header_line(),
            author_raw: Vec::new(),
            committer_raw: Vec::new(),
            encoding: None,
            message,
            raw_message: None,
            extra_headers: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::HashAlgo;
    use time::OffsetDateTime;

    fn oid(byte: u8) -> ObjectId {
        ObjectId::from_bytes(&[byte; 20]).unwrap()
    }

    fn entry(mode: u32, name: &[u8], byte: u8) -> TreeEntry {
        TreeEntry {
            mode,
            name: name.to_vec(),
            oid: oid(byte),
        }
    }

    #[test]
    fn sorts_into_git_tree_order() {
        let sorted = canonical_tree_entries(
            [
                entry(0o100644, b"foo.c", 1),
                entry(MODE_TREE, b"foo", 2),
                entry(0o100644, b"foo-bar", 3),
                entry(0o100644, b"a", 4),
            ],
            HashAlgo::Sha1,
        )
        .unwrap();
        let names: Vec<&[u8]> = sorted.iter().map(|e| e.name.as_slice()).collect();
        assert_eq!(
            names,
            [&b"a"[..], &b"foo-bar"[..], &b"foo.c"[..], &b"foo"[..]]
        );
    }

    #[test]
    fn rejects_file_and_directory_with_the_same_name() {
        let err = canonical_tree_entries(
            [
                entry(0o100644, b"foo", 1),
                entry(0o100644, b"foo.c", 2),
                entry(MODE_TREE, b"foo", 3),
            ],
            HashAlgo::Sha1,
        )
        .unwrap_err();
        assert_eq!(
            err,
            TreeEntryError::DuplicateName {
                name: b"foo".to_vec()
            }
        );
    }

    #[test]
    fn rejects_bad_names_modes_and_ids() {
        let cases: [(TreeEntry, TreeEntryError); 7] = [
            (
                entry(0o100644, &[b'x'; MAX_TREE_ENTRY_LEN + 1], 1),
                TreeEntryError::NameTooLong {
                    name: vec![b'x'; MAX_TREE_ENTRY_LEN + 1],
                    max: MAX_TREE_ENTRY_LEN,
                },
            ),
            (
                entry(0o100644, b"", 1),
                TreeEntryError::InvalidName {
                    name: Vec::new(),
                    reason: PathComponentError::Empty,
                },
            ),
            (
                entry(0o100644, b"a/b", 1),
                TreeEntryError::InvalidName {
                    name: b"a/b".to_vec(),
                    reason: PathComponentError::ContainsSlash,
                },
            ),
            (
                entry(0o100644, b"..", 1),
                TreeEntryError::InvalidName {
                    name: b"..".to_vec(),
                    reason: PathComponentError::DotDot,
                },
            ),
            (
                entry(MODE_TREE, b".GIT", 1),
                TreeEntryError::InvalidName {
                    name: b".GIT".to_vec(),
                    reason: PathComponentError::DotGit,
                },
            ),
            (
                entry(0o100664, b"a", 1),
                TreeEntryError::InvalidMode {
                    name: b"a".to_vec(),
                    mode: 0o100664,
                },
            ),
            (
                entry(0o100644, b"a", 0),
                TreeEntryError::NullObjectId {
                    name: b"a".to_vec(),
                },
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(
                canonical_tree_entries([input], HashAlgo::Sha1).unwrap_err(),
                expected
            );
        }
    }

    #[test]
    fn rejects_object_ids_of_the_other_hash_algorithm() {
        let err = canonical_tree_entries([entry(0o100644, b"a", 1)], HashAlgo::Sha256).unwrap_err();
        assert!(matches!(
            err,
            TreeEntryError::HashAlgoMismatch {
                expected: HashAlgo::Sha256,
                actual: HashAlgo::Sha1,
                ..
            }
        ));
    }

    #[test]
    fn commit_data_terminates_message_and_renders_signatures() {
        let when = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let sig = Signature::new("A U Thor", "a@example.com", when).unwrap();
        let data = NewCommit::new(oid(1), sig.clone(), sig, "subject")
            .with_parents([oid(2)])
            .to_commit_data();
        assert_eq!(data.message, "subject\n");
        assert_eq!(data.parents, vec![oid(2)]);
        assert_eq!(data.author, "A U Thor <a@example.com> 1700000000 +0000");
        assert!(data.extra_headers.is_empty());
    }
}
