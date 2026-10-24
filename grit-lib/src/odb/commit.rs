//! Reading commit objects by id.
//!
//! Callers that need "the commit at this id" or "the root tree of this commit" share these
//! [`Odb`] methods instead of reading the object, checking its kind and parsing it by hand.

use crate::error::{Error, Result};
use crate::objects::{parse_commit, CommitData, CommitRef, ObjectId, ObjectKind};

use super::Odb;

impl Odb {
    /// Read `oid` and parse it as a commit.
    ///
    /// Annotated tags are not followed; peel first with [`Self::peel_to_commit`] when the id
    /// may name a tag.
    ///
    /// # Parameters
    ///
    /// - `oid`: the commit to read.
    ///
    /// # Returns
    ///
    /// The owned, parsed commit headers and message.
    ///
    /// # Errors
    ///
    /// - [`Error::ObjectNotFound`] when `oid` is missing.
    /// - [`Error::UnexpectedObjectKind`] when `oid` names a tree, blob or tag.
    /// - [`Error::CorruptObject`] when the commit's required headers are missing or malformed.
    /// - Any other error [`Self::read`] reports.
    pub fn read_commit(&self, oid: &ObjectId) -> Result<CommitData> {
        let object = self.read(oid)?;
        expect_commit(oid, object.kind)?;
        parse_commit(&object.data)
    }

    /// The root tree of the commit `oid`.
    ///
    /// Only the commit headers are decoded; the author, committer and message are not copied.
    ///
    /// # Parameters
    ///
    /// - `oid`: the commit whose tree to return.
    ///
    /// # Errors
    ///
    /// Same as [`Self::read_commit`].
    pub fn commit_tree(&self, oid: &ObjectId) -> Result<ObjectId> {
        let object = self.read(oid)?;
        expect_commit(oid, object.kind)?;
        Ok(CommitRef::parse(&object.data)?.tree())
    }
}

fn expect_commit(oid: &ObjectId, actual: ObjectKind) -> Result<()> {
    if actual == ObjectKind::Commit {
        return Ok(());
    }
    Err(Error::UnexpectedObjectKind {
        oid: *oid,
        expected: ObjectKind::Commit,
        actual,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use tempfile::TempDir;

    fn commit_with_parent(odb: &Odb, tree: ObjectId, parent: Option<ObjectId>) -> ObjectId {
        let parent = parent.map(|p| format!("parent {p}\n")).unwrap_or_default();
        let body = format!(
            "tree {tree}\n{parent}author A <a@example.com> 1 +0000\ncommitter C <c@example.com> 2 +0100\n\nsubject\n\nbody\n"
        );
        odb.write(ObjectKind::Commit, body.as_bytes()).unwrap()
    }

    #[test]
    fn read_commit_matches_parse_commit() {
        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());
        let tree = odb.write(ObjectKind::Tree, b"").unwrap();
        let root = commit_with_parent(&odb, tree, None);
        let child = commit_with_parent(&odb, tree, Some(root));

        let read = odb.read_commit(&child).unwrap();
        let parsed = parse_commit(&odb.read(&child).unwrap().data).unwrap();
        assert_eq!(read, parsed);
        assert_eq!(read.tree, tree);
        assert_eq!(read.parents, vec![root]);
        assert_eq!(odb.commit_tree(&child).unwrap(), tree);
    }

    #[test]
    fn non_commits_are_unexpected_kinds() {
        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());
        let tree = odb.write(ObjectKind::Tree, b"").unwrap();
        let blob = odb.write(ObjectKind::Blob, b"data").unwrap();
        let commit = commit_with_parent(&odb, tree, None);
        let tag = odb
            .write(
                ObjectKind::Tag,
                format!("object {commit}\ntype commit\ntag t\n\nmsg\n").as_bytes(),
            )
            .unwrap();

        for (oid, kind) in [
            (tree, ObjectKind::Tree),
            (blob, ObjectKind::Blob),
            (tag, ObjectKind::Tag),
        ] {
            for err in [
                odb.read_commit(&oid).unwrap_err(),
                odb.commit_tree(&oid).unwrap_err(),
            ] {
                assert!(
                    matches!(
                        err,
                        Error::UnexpectedObjectKind {
                            oid: o,
                            expected: ObjectKind::Commit,
                            actual,
                        } if o == oid && actual == kind
                    ),
                    "{err:?}"
                );
                assert_eq!(
                    err.to_string(),
                    format!("object {oid} is a {kind}, not a commit")
                );
            }
        }
    }

    #[test]
    fn missing_and_malformed_commits_are_errors() {
        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());
        let missing = ObjectId::from_hex("1111111111111111111111111111111111111111").unwrap();
        assert!(matches!(
            odb.read_commit(&missing),
            Err(Error::ObjectNotFound(_))
        ));
        assert!(matches!(
            odb.commit_tree(&missing),
            Err(Error::ObjectNotFound(_))
        ));

        let no_tree = odb
            .write(
                ObjectKind::Commit,
                b"author A <a@example.com> 0 +0000\n\nno tree\n",
            )
            .unwrap();
        assert!(matches!(
            odb.read_commit(&no_tree),
            Err(Error::CorruptObject(_))
        ));
        assert!(matches!(
            odb.commit_tree(&no_tree),
            Err(Error::CorruptObject(_))
        ));
    }
}
