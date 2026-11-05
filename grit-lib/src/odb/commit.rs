//! Reading commit objects by id.
//!
//! Callers that need "the commit at this id" or "the root tree of this commit" share these
//! [`Odb`] methods instead of reading the object, checking its kind and parsing it by hand.

use std::path::Path;

use crate::error::{Error, Result};
use crate::objects::{
    commit_through_subject_len, parse_commit, CommitData, CommitRef, ObjectId, ObjectKind,
};

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

    /// The leading part of commit `oid`'s payload that one-line listings need: every header
    /// and the message up to and including its first non-blank line.
    ///
    /// [`parse_commit`] and [`crate::objects::read_commit_subject_line`] give the same tree,
    /// parents, author, committer, extra headers and subject for these bytes as for the whole
    /// commit; only the message after the subject line is missing. A commit with no such
    /// prefix (for example one that declares a non-UTF-8 `encoding`, or has no message) is
    /// returned whole.
    ///
    /// When the commit is stored whole in a pack, inflation stops at the subject line, so a
    /// long message body costs nothing. The skipped tail is not checked for corruption;
    /// [`Self::read`] the commit to check all of it.
    ///
    /// # Parameters
    ///
    /// - `oid`: the commit to read.
    ///
    /// # Errors
    ///
    /// - [`Error::ObjectNotFound`] when `oid` is missing.
    /// - [`Error::UnexpectedObjectKind`] when `oid` names a tree, blob or tag.
    /// - Any other error [`Self::read`] reports.
    pub fn read_commit_through_subject(&self, oid: &ObjectId) -> Result<Vec<u8>> {
        if self.custom_primary.is_none() {
            if crate::pack_store::PackStore::has_context() {
                let active = crate::pack_store::PackStore::active_for_reads();
                let active_dir = active.objects_dir();
                if active_dir != self.objects_dir.as_path() {
                    if let Some(data) =
                        self.try_read_commit_through_subject_in_dir(active_dir, oid)?
                    {
                        return Ok(data);
                    }
                }
            }
            let primary_empty = crate::pack::pack_cache::primary_has_verified_empty_listing(
                self.objects_dir.as_path(),
            );
            if primary_empty && self.alternates_enabled {
                if let Some(data) = self.try_read_commit_through_subject_from_alternates(oid)? {
                    return Ok(data);
                }
            } else if let Some(data) =
                self.try_read_commit_through_subject_in_dir(self.objects_dir.as_path(), oid)?
            {
                return Ok(data);
            } else if self.alternates_enabled {
                if let Some(data) = self.try_read_commit_through_subject_from_alternates(oid)? {
                    return Ok(data);
                }
            }
        }
        let object = self.read(oid)?;
        expect_commit(oid, object.kind)?;
        Ok(through_subject(object.data))
    }

    fn try_read_commit_through_subject_from_alternates(
        &self,
        oid: &ObjectId,
    ) -> Result<Option<Vec<u8>>> {
        for alt in self.file_alternate_dirs_snapshot().iter() {
            if let Some(data) = self.try_read_commit_through_subject_in_dir(alt, oid)? {
                return Ok(Some(data));
            }
        }
        for alt in self.env_alternate_dirs_snapshot().iter() {
            if let Some(data) = self.try_read_commit_through_subject_in_dir(alt, oid)? {
                return Ok(Some(data));
            }
        }
        Ok(None)
    }

    fn try_read_commit_through_subject_in_dir(
        &self,
        objects_dir: &Path,
        oid: &ObjectId,
    ) -> Result<Option<Vec<u8>>> {
        let read = || -> Result<Option<Vec<u8>>> {
            if let Some(data) =
                crate::pack::pack_cache::try_read_commit_through_subject_via_single_pack_reader(
                    objects_dir,
                    oid,
                )?
            {
                return Ok(Some(through_subject(data)));
            }
            let mut prefix = Vec::new();
            if crate::pack::read_packed_commit_through_subject_into(objects_dir, oid, &mut prefix)?
                && !prefix.is_empty()
            {
                return Ok(Some(through_subject(prefix)));
            }
            Ok(None)
        };
        let alternate_already_pinned = objects_dir != self.objects_dir.as_path()
            && crate::pack::pack_cache::alternate_read_store()
                .is_some_and(|store| store.objects_dir() == objects_dir);
        if objects_dir == self.objects_dir.as_path() || alternate_already_pinned {
            read()
        } else {
            self.with_objects_dir_pack_read_access(objects_dir, read)
        }
    }
}

fn through_subject(mut data: Vec<u8>) -> Vec<u8> {
    if let Some(len) = commit_through_subject_len(&data) {
        data.truncate(len);
    }
    data
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

    fn git(dir: &std::path::Path, args: &[&str], stdin: Option<&[u8]>) -> Vec<u8> {
        use std::io::Write as _;
        let mut child = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.unwrap_or_default())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    }

    /// Commits git writes with awkward messages, each with the prefix
    /// [`Odb::read_commit_through_subject`] must return.
    fn git_commits_with_prefixes(dir: &std::path::Path) -> Vec<(ObjectId, Vec<u8>)> {
        git(dir, &["init", "-q", "--bare", "."], None);
        let tree = String::from_utf8(git(dir, &["mktree"], Some(b""))).unwrap();
        let head = format!(
            "tree {}\nauthor A U Thor <a@example.com> 1700000000 +0100\ncommitter C O Mitter <c@example.com> 1700000001 -0700\n",
            tree.trim()
        );
        let body: String = (0..200)
            .map(|i| format!("Body line {i} with some words.\n"))
            .collect();
        let signed_head = format!(
            "{head}gpgsig -----BEGIN PGP SIGNATURE-----\n \n {}\n -----END PGP SIGNATURE-----\nchange-id kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk\n",
            "A".repeat(64)
        );
        let cases: Vec<(Vec<u8>, usize)> = vec![
            {
                let prefix = format!("{head}\nSubject line\n");
                (format!("{prefix}\n{body}").into_bytes(), prefix.len())
            },
            {
                let prefix = format!("{head}\n\n  \n\t  Indented subject\n");
                (
                    format!("{prefix}second line\n\n{body}").into_bytes(),
                    prefix.len(),
                )
            },
            {
                let prefix = format!("{signed_head}\nSigned subject\n");
                (format!("{prefix}\n{body}").into_bytes(), prefix.len())
            },
            {
                let prefix = format!("{head}\n\u{65e5}\u{672c}\n\u{a0}\nascii line\n");
                (format!("{prefix}{body}").into_bytes(), prefix.len())
            },
            {
                let mut raw = format!("{head}encoding ISO-8859-1\n\n").into_bytes();
                raw.extend_from_slice(b"S\xe9ance\n\ncaf\xe9\n");
                raw.extend_from_slice(body.as_bytes());
                let len = raw.len();
                (raw, len)
            },
            {
                let raw = format!("{head}\nno trailing newline").into_bytes();
                let len = raw.len();
                (raw, len)
            },
            {
                let raw = head.clone().into_bytes();
                let len = raw.len();
                (raw, len)
            },
        ];
        cases
            .into_iter()
            .map(|(raw, prefix_len)| {
                let hex = git(
                    dir,
                    &[
                        "hash-object",
                        "-t",
                        "commit",
                        "-w",
                        "--literally",
                        "--stdin",
                    ],
                    Some(&raw),
                );
                let oid = ObjectId::from_hex(std::str::from_utf8(&hex).unwrap().trim()).unwrap();
                (oid, raw[..prefix_len].to_vec())
            })
            .collect()
    }

    fn assert_prefixes(odb: &Odb, dir: &std::path::Path, cases: &[(ObjectId, Vec<u8>)]) {
        use crate::objects::read_commit_subject_line;
        for (oid, expected) in cases {
            let full = git(dir, &["cat-file", "commit", &oid.to_hex()], None);
            let prefix = odb
                .with_pack_read_context(|| odb.read_commit_through_subject(oid))
                .unwrap();
            assert_eq!(prefix, *expected, "{oid}");
            assert!(full.starts_with(&prefix), "{oid}");
            assert_eq!(
                read_commit_subject_line(&prefix).unwrap(),
                read_commit_subject_line(&full).unwrap(),
                "{oid}"
            );
            let (short, whole) = (parse_commit(&prefix).unwrap(), parse_commit(&full).unwrap());
            assert_eq!(
                (short.tree, &short.parents, &short.author, &short.committer),
                (whole.tree, &whole.parents, &whole.author, &whole.committer),
                "{oid}"
            );
            assert_eq!(
                (&short.encoding, &short.extra_headers),
                (&whole.encoding, &whole.extra_headers),
                "{oid}"
            );
        }
    }

    #[test]
    fn read_commit_through_subject_matches_git_loose_and_packed() {
        let dir = TempDir::new().unwrap();
        let cases = git_commits_with_prefixes(dir.path());
        let odb = Odb::new(&dir.path().join("objects"));
        assert_prefixes(&odb, dir.path(), &cases);

        git(
            dir.path(),
            &["update-ref", "refs/heads/a", &cases[0].0.to_hex()],
            None,
        );
        for (i, (oid, _)) in cases.iter().enumerate() {
            git(
                dir.path(),
                &["update-ref", &format!("refs/tags/t{i}"), &oid.to_hex()],
                None,
            );
        }
        git(dir.path(), &["repack", "-a", "-d", "-q"], None);
        git(dir.path(), &["prune-packed"], None);
        let odb = Odb::new(&dir.path().join("objects"));
        assert_prefixes(&odb, dir.path(), &cases);

        // Commits the pack stores whole come from the bounded pack read (deltas fall back to
        // a whole read); git deltifies these similar commits, so only some are whole.
        let objects = dir.path().join("objects");
        let mut cut_short = 0;
        for (oid, expected) in &cases {
            let read = odb.with_pack_read_context(|| {
                crate::pack::pack_cache::try_read_commit_through_subject_via_single_pack_reader(
                    &objects, oid,
                )
            });
            let Some(prefix) = read.unwrap() else {
                continue;
            };
            let full = git(dir.path(), &["cat-file", "commit", &oid.to_hex()], None);
            if prefix.len() < full.len() {
                assert_eq!(prefix, *expected, "{oid}");
                cut_short += 1;
            } else {
                assert_eq!(prefix, full, "{oid}");
            }
        }
        assert!(cut_short > 0, "no commit was read only through its subject");
    }

    #[test]
    fn read_commit_through_subject_packed_without_pack_read_context() {
        use crate::objects::read_commit_subject_line;

        let dir = TempDir::new().unwrap();
        let cases = git_commits_with_prefixes(dir.path());
        git(dir.path(), &["repack", "-a", "-d", "-q"], None);
        let objects = dir.path().join("objects");
        let odb = Odb::new(&objects);
        for (oid, expected) in &cases {
            let prefix = odb.read_commit_through_subject(oid).unwrap();
            assert_eq!(prefix, *expected, "{oid}");
            assert_eq!(
                read_commit_subject_line(&prefix).unwrap(),
                read_commit_subject_line(&git(
                    dir.path(),
                    &["cat-file", "commit", &oid.to_hex()],
                    None
                ))
                .unwrap(),
                "{oid}"
            );
        }

        git(dir.path(), &["commit-graph", "write", "--reachable"], None);
        for (oid, expected) in &cases {
            let prefix = odb.read_commit_through_subject(oid).unwrap();
            assert_eq!(prefix, *expected, "{oid} with commit-graph");
        }
    }

    #[test]
    fn read_commit_through_subject_rejects_other_kinds() {
        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());
        let blob = odb.write(ObjectKind::Blob, b"data").unwrap();
        assert!(matches!(
            odb.read_commit_through_subject(&blob),
            Err(Error::UnexpectedObjectKind {
                actual: ObjectKind::Blob,
                ..
            })
        ));
        let missing = ObjectId::from_hex("1111111111111111111111111111111111111111").unwrap();
        assert!(matches!(
            odb.read_commit_through_subject(&missing),
            Err(Error::ObjectNotFound(_))
        ));
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
