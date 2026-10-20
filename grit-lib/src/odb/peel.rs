//! Following annotated tags to the object they ultimately name.
//!
//! Every caller that turns a ref tip into "the commit it means" (revision walks, fetch
//! negotiation, bundles, commit-graph tips, `packed-refs` peel lines) shares these two
//! [`Odb`] methods instead of its own tag loop.

use crate::error::{Error, Result};
use crate::objects::{parse_tag, ObjectId, ObjectKind};

use super::Odb;

/// Longest chain of annotated tags [`Odb::peel_tags`] follows before reporting corruption.
///
/// Object ids make real cycles impossible, but a pluggable store can still serve a chain that
/// never ends; real repositories rarely nest tags more than a couple of levels.
const MAX_TAG_CHAIN: usize = 64;

impl Odb {
    /// Follow annotated tags starting at `oid` until reaching an object that is not a tag.
    ///
    /// A non-tag `oid` is returned unchanged. Only tag objects are fully read; the final
    /// object's kind comes from [`Self::read_info`], so peeling to a large blob or tree does not
    /// inflate it.
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
    /// - [`Error::CorruptObject`] when a tag cannot be parsed, or the chain is longer than
    ///   64 tags.
    /// - Any other error [`Self::read`] / [`Self::read_info`] reports for the objects read.
    pub fn peel_tags(&self, oid: ObjectId) -> Result<(ObjectId, ObjectKind)> {
        let mut current = oid;
        for _ in 0..=MAX_TAG_CHAIN {
            let kind = self.read_info(&current)?.kind;
            if kind != ObjectKind::Tag {
                return Ok((current, kind));
            }
            current = parse_tag(&self.read(&current)?.data)?.object;
        }
        Err(Error::CorruptObject(format!(
            "tag chain from {oid} is longer than {MAX_TAG_CHAIN} tags"
        )))
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
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
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
    fn unparsable_tag_is_corrupt() {
        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());
        let bad = odb.write(ObjectKind::Tag, b"not a tag\n").unwrap();
        assert!(matches!(odb.peel_tags(bad), Err(Error::CorruptObject(_))));
    }

    #[test]
    fn chain_at_the_limit_peels_and_one_more_is_corrupt() {
        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());
        let c = commit(&odb);
        let mut tip = tag(&odb, c, "commit", "t0");
        for i in 1..MAX_TAG_CHAIN {
            tip = tag(&odb, tip, "tag", &format!("t{i}"));
        }
        assert_eq!(odb.peel_to_commit(tip).unwrap(), Some(c));
        let too_deep = tag(&odb, tip, "tag", "over");
        assert!(matches!(
            odb.peel_tags(too_deep),
            Err(Error::CorruptObject(_))
        ));
    }
}
