//! Following annotated tags to the object they ultimately name.
//!
//! Every caller that turns a ref tip into "the commit it means" (revision walks, fetch
//! negotiation, bundles, bitmaps, commit-graph tips, `packed-refs` and `ls-remote` peel lines,
//! `--with-tree`) shares these [`Odb`] methods instead of its own tag loop.

use std::collections::HashSet;

use crate::error::{Error, Result};
use crate::objects::{tag_object_line_oid, CommitRef, ObjectId, ObjectKind};

use super::Odb;

impl Odb {
    /// Follow annotated tags starting at `oid` until reaching an object that is not a tag.
    ///
    /// A non-tag `oid` is returned unchanged. Only tag objects are fully read, and only their
    /// `object` header is parsed (other headers and the message may be absent or not UTF-8);
    /// the final object's kind comes from [`Self::read_info`], so peeling to a large blob or
    /// tree does not inflate it.
    ///
    /// # Parameters
    ///
    /// - `oid`: the object to start from, usually a ref tip.
    ///
    /// # Returns
    ///
    /// The id and kind of the first non-tag object in the chain.
    ///
    /// # Errors
    ///
    /// - [`Error::ObjectNotFound`] when `oid` or any object the chain names is missing.
    /// - [`Error::CorruptObject`] when a tag has no valid `object` header, or the chain names
    ///   a tag it already passed through. Chains of any length are followed; content-addressed
    ///   ids make a cycle impossible in a real repository, but a pluggable store can serve one.
    /// - Any other error [`Self::read`] / [`Self::read_info`] reports for the objects read.
    pub fn peel_tags(&self, oid: ObjectId) -> Result<(ObjectId, ObjectKind)> {
        let mut current = oid;
        let mut visited: Option<HashSet<ObjectId>> = None;
        loop {
            let kind = self.read_info(&current)?.kind;
            if kind != ObjectKind::Tag {
                return Ok((current, kind));
            }
            if !visited.get_or_insert_with(HashSet::new).insert(current) {
                return Err(Error::CorruptObject(format!(
                    "tag chain from {oid} loops back to {current}"
                )));
            }
            let tag = self.read(&current)?;
            current = tag_object_line_oid(&tag.data).ok_or_else(|| {
                Error::CorruptObject(format!("tag {current} has no valid object header"))
            })?;
        }
    }

    /// Peel `oid` through annotated tags to a commit.
    ///
    /// # Parameters
    ///
    /// - `oid`: a commit, or an annotated tag (possibly nested) naming one.
    ///
    /// # Returns
    ///
    /// `Some(commit)` when the chain ends at a commit, `None` when it ends at a tree or blob.
    ///
    /// # Errors
    ///
    /// Same as [`Self::peel_tags`]: missing objects, unparsable tags and overlong chains are
    /// errors rather than `None`, so callers can tell "not a commit" from "broken repository".
    pub fn peel_to_commit(&self, oid: ObjectId) -> Result<Option<ObjectId>> {
        let (peeled, kind) = self.peel_tags(oid)?;
        Ok((kind == ObjectKind::Commit).then_some(peeled))
    }

    /// The object an annotated tag ultimately names: the `^{}` value of a ref advertisement or
    /// a `packed-refs` peel line.
    ///
    /// Returns `None` when `oid` is not a tag. Unlike [`Self::peel_tags`], each tag's `type`
    /// header decides whether the chain continues, so the final object is never read and may
    /// be missing. Git computes these values the same way, and listing many tags pays for one
    /// object read per tag instead of two.
    ///
    /// # Errors
    ///
    /// - [`Error::ObjectNotFound`] when `oid` or an intermediate tag is missing.
    /// - [`Error::CorruptObject`] when a tag has no valid `object` header, a `type tag` header
    ///   names an object that is not a tag, or the chain loops.
    pub(crate) fn peel_tag_target(&self, oid: ObjectId) -> Result<Option<ObjectId>> {
        if self.read_info(&oid)?.kind != ObjectKind::Tag {
            return Ok(None);
        }
        let mut current = oid;
        let mut visited: Option<HashSet<ObjectId>> = None;
        loop {
            let tag = self.read(&current)?;
            if tag.kind != ObjectKind::Tag {
                return Err(Error::CorruptObject(format!(
                    "{current} is named as a tag but is a {}",
                    tag.kind
                )));
            }
            let (target, names_tag) = tag_target_header(&tag.data);
            let target = target.ok_or_else(|| {
                Error::CorruptObject(format!("tag {current} has no valid object header"))
            })?;
            if !names_tag {
                return Ok(Some(target));
            }
            if !visited.get_or_insert_with(HashSet::new).insert(current) {
                return Err(Error::CorruptObject(format!(
                    "tag chain from {oid} loops back to {current}"
                )));
            }
            current = target;
        }
    }

    /// Peel `oid` through annotated tags and a commit to a tree.
    ///
    /// Returns `None` when the chain ends at a blob. Errors are those of [`Self::peel_tags`],
    /// plus [`Error::CorruptObject`] for a commit without a valid `tree` header.
    pub(crate) fn peel_to_tree(&self, oid: ObjectId) -> Result<Option<ObjectId>> {
        match self.peel_tags(oid)? {
            (tree, ObjectKind::Tree) => Ok(Some(tree)),
            (commit, ObjectKind::Commit) => {
                let obj = self.read(&commit)?;
                Ok(Some(CommitRef::parse(&obj.data)?.tree()))
            }
            _ => Ok(None),
        }
    }
}

/// The first `object` header of a tag and whether its first `type` header is `tag`, read
/// without allocating.
fn tag_target_header(data: &[u8]) -> (Option<ObjectId>, bool) {
    let mut target: Option<Option<ObjectId>> = None;
    let mut names_tag = None;
    for line in data.split(|&b| b == b'\n') {
        if line.is_empty() || (target.is_some() && names_tag.is_some()) {
            break;
        }
        if let Some(hex) = line.strip_prefix(b"object ") {
            target.get_or_insert_with(|| {
                std::str::from_utf8(hex)
                    .ok()
                    .and_then(|s| s.trim().parse().ok())
            });
        } else if let Some(kind) = line.strip_prefix(b"type ") {
            names_tag.get_or_insert(kind.trim_ascii() == b"tag");
        }
    }
    (target.flatten(), names_tag.unwrap_or(false))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use crate::objects::HashAlgo;
    use crate::odb::{MemoryStore, OdbBuilder};
    use std::sync::Arc;
    use tempfile::TempDir;

    fn tag(odb: &Odb, target: ObjectId, kind: &str, name: &str) -> ObjectId {
        let body = format!(
            "object {target}\ntype {kind}\ntag {name}\ntagger T <t@example.com> 0 +0000\n\nmsg\n"
        );
        odb.write(ObjectKind::Tag, body.as_bytes()).unwrap()
    }

    fn commit(odb: &Odb) -> ObjectId {
        let tree = odb.write(ObjectKind::Tree, b"").unwrap();
        let body = format!(
            "tree {tree}\nauthor A <a@example.com> 0 +0000\ncommitter A <a@example.com> 0 +0000\n\nc\n"
        );
        odb.write(ObjectKind::Commit, body.as_bytes()).unwrap()
    }

    #[test]
    fn non_tag_is_returned_unchanged() {
        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());
        let c = commit(&odb);
        let blob = odb.write(ObjectKind::Blob, b"data").unwrap();
        assert_eq!(odb.peel_tags(c).unwrap(), (c, ObjectKind::Commit));
        assert_eq!(odb.peel_tags(blob).unwrap(), (blob, ObjectKind::Blob));
        assert_eq!(odb.peel_to_commit(c).unwrap(), Some(c));
        assert_eq!(odb.peel_to_commit(blob).unwrap(), None);
    }

    #[test]
    fn nested_tags_peel_to_the_innermost_object() {
        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());
        let c = commit(&odb);
        let inner = tag(&odb, c, "commit", "inner");
        let outer = tag(&odb, inner, "tag", "outer");
        assert_eq!(odb.peel_tags(outer).unwrap(), (c, ObjectKind::Commit));
        assert_eq!(odb.peel_to_commit(outer).unwrap(), Some(c));

        let blob = odb.write(ObjectKind::Blob, b"data").unwrap();
        let tag_of_blob = tag(&odb, blob, "blob", "b");
        assert_eq!(
            odb.peel_tags(tag_of_blob).unwrap(),
            (blob, ObjectKind::Blob)
        );
        assert_eq!(odb.peel_to_commit(tag_of_blob).unwrap(), None);
    }

    #[test]
    fn missing_objects_are_errors() {
        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());
        let missing = ObjectId::from_hex("1111111111111111111111111111111111111111").unwrap();
        assert!(matches!(
            odb.peel_tags(missing),
            Err(Error::ObjectNotFound(_))
        ));
        let dangling = tag(&odb, missing, "commit", "dangling");
        assert!(matches!(
            odb.peel_to_commit(dangling),
            Err(Error::ObjectNotFound(_))
        ));
    }

    #[test]
    fn only_the_object_header_is_required() {
        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());
        let c = commit(&odb);
        let mut body = format!("object {c}\ntype commit\n\n").into_bytes();
        body.extend_from_slice(b"latin-1 message \xe9\n");
        let bare = odb.write(ObjectKind::Tag, &body).unwrap();
        assert_eq!(odb.peel_to_commit(bare).unwrap(), Some(c));
    }

    #[test]
    fn unparsable_tag_is_corrupt() {
        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());
        let bad = odb.write(ObjectKind::Tag, b"not a tag\n").unwrap();
        assert!(matches!(odb.peel_tags(bad), Err(Error::CorruptObject(_))));
    }

    #[test]
    fn long_chains_peel_without_a_depth_limit() {
        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());
        let c = commit(&odb);
        let mut tip = tag(&odb, c, "commit", "t0");
        for i in 1..200 {
            tip = tag(&odb, tip, "tag", &format!("t{i}"));
        }
        assert_eq!(odb.peel_to_commit(tip).unwrap(), Some(c));
    }

    #[test]
    fn peel_tag_target_names_the_innermost_object_of_tags_only() {
        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());
        let c = commit(&odb);
        let inner = tag(&odb, c, "commit", "inner");
        let outer = tag(&odb, inner, "tag", "outer");
        let blob = odb.write(ObjectKind::Blob, b"data").unwrap();
        let tag_of_blob = tag(&odb, blob, "blob", "b");
        assert_eq!(odb.peel_tag_target(c).unwrap(), None);
        assert_eq!(odb.peel_tag_target(blob).unwrap(), None);
        assert_eq!(odb.peel_tag_target(inner).unwrap(), Some(c));
        assert_eq!(odb.peel_tag_target(outer).unwrap(), Some(c));
        assert_eq!(odb.peel_tag_target(tag_of_blob).unwrap(), Some(blob));

        let missing = ObjectId::from_hex("1111111111111111111111111111111111111111").unwrap();
        let dangling = tag(&odb, missing, "commit", "dangling");
        assert_eq!(odb.peel_tag_target(dangling).unwrap(), Some(missing));
        let dangling_outer = tag(&odb, missing, "tag", "dangling-outer");
        assert!(matches!(
            odb.peel_tag_target(dangling_outer),
            Err(Error::ObjectNotFound(_))
        ));
        assert!(matches!(
            odb.peel_tag_target(missing),
            Err(Error::ObjectNotFound(_))
        ));
    }

    #[test]
    fn peel_tag_target_rejects_a_type_tag_header_naming_a_non_tag() {
        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());
        let c = commit(&odb);
        let lying = tag(&odb, c, "tag", "lying");
        assert!(matches!(
            odb.peel_tag_target(lying),
            Err(Error::CorruptObject(_))
        ));
    }

    #[test]
    fn peel_tag_target_detects_a_tag_cycle() {
        let store = Arc::new(MemoryStore::new(HashAlgo::Sha1));
        let a = ObjectId::from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        let b = ObjectId::from_hex("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap();
        for (from, to) in [(a, b), (b, a)] {
            let body = format!("object {to}\ntype tag\ntag cycle\n\n");
            store.insert_object(from, ObjectKind::Tag, Arc::from(body.into_bytes()));
        }
        let dir = TempDir::new().unwrap();
        let odb = OdbBuilder::files(dir.path())
            .primary(store)
            .alternates(false)
            .build();
        assert!(matches!(
            odb.peel_tag_target(a),
            Err(Error::CorruptObject(_))
        ));
    }

    #[test]
    fn peel_to_tree_follows_tags_and_commits() {
        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());
        let tree = odb.write(ObjectKind::Tree, b"").unwrap();
        let c = commit(&odb);
        let outer = tag(&odb, tag(&odb, c, "commit", "inner"), "tag", "outer");
        let tag_of_tree = tag(&odb, tree, "tree", "t");
        let blob = odb.write(ObjectKind::Blob, b"data").unwrap();
        let tag_of_blob = tag(&odb, blob, "blob", "b");
        assert_eq!(odb.peel_to_tree(tree).unwrap(), Some(tree));
        assert_eq!(odb.peel_to_tree(c).unwrap(), Some(tree));
        assert_eq!(odb.peel_to_tree(outer).unwrap(), Some(tree));
        assert_eq!(odb.peel_to_tree(tag_of_tree).unwrap(), Some(tree));
        assert_eq!(odb.peel_to_tree(blob).unwrap(), None);
        assert_eq!(odb.peel_to_tree(tag_of_blob).unwrap(), None);
    }

    #[test]
    fn peel_to_tree_rejects_a_commit_without_a_tree_header() {
        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());
        let bad = odb
            .write(
                ObjectKind::Commit,
                b"author A <a@example.com> 0 +0000\n\nno tree\n",
            )
            .unwrap();
        assert!(matches!(
            odb.peel_to_tree(bad),
            Err(Error::CorruptObject(_))
        ));
    }

    #[test]
    fn a_store_serving_a_tag_cycle_is_corrupt() {
        let store = Arc::new(MemoryStore::new(HashAlgo::Sha1));
        let a = ObjectId::from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        let b = ObjectId::from_hex("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap();
        for (from, to) in [(a, b), (b, a)] {
            let body = format!("object {to}\ntype tag\ntag cycle\n\n");
            store.insert_object(from, ObjectKind::Tag, Arc::from(body.into_bytes()));
        }
        let dir = TempDir::new().unwrap();
        let odb = OdbBuilder::files(dir.path())
            .primary(store)
            .alternates(false)
            .build();
        assert!(matches!(odb.peel_tags(a), Err(Error::CorruptObject(_))));
    }
}
