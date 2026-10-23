//! `list_tags` classification from `packed-refs` peel lines and `create_tag` against packed
//! refs, checked against the system `git` on repositories it wrote.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::tag::{
    create_tag, list_tags, CreateTagRequest, TagError, TagInfo, TagKind,
};
use grit_lib::repo::Repository;
use grit_test_support::{git, git_cmd};

fn open(dir: &Path) -> Repository {
    Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("discover")
}

fn rev(dir: &Path, rev: &str) -> ObjectId {
    git(dir, &["rev-parse", rev]).trim().parse().expect("oid")
}

/// Lightweight, annotated, nested-annotated, tree, annotated-tree and blob tags, all packed
/// by `git pack-refs --all` (which writes `peeled fully-peeled sorted`).
fn packed_fixture(root: &Path) -> PathBuf {
    let dir = root.join("repo");
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("f"), "one\n").unwrap();
    git(&dir, &["add", "f"]);
    git(&dir, &["commit", "-qm", "c1"]);
    std::fs::write(dir.join("f"), "two\n").unwrap();
    git(&dir, &["commit", "-qam", "c2"]);
    git(&dir, &["tag", "light", "main~1"]);
    git(&dir, &["tag", "-a", "annotated", "-m", "release", "main"]);
    git(&dir, &["tag", "-a", "nested", "-m", "outer", "annotated"]);
    let tree = git(&dir, &["rev-parse", "main^{tree}"]);
    git(&dir, &["tag", "tree-light", tree.trim()]);
    git(
        &dir,
        &["tag", "-a", "tree-annotated", "-m", "tree", tree.trim()],
    );
    let blob = git(&dir, &["rev-parse", "main:f"]);
    git(&dir, &["tag", "blob-light", blob.trim()]);
    git(&dir, &["pack-refs", "--all"]);
    assert!(!dir.join(".git/refs/tags/annotated").exists());
    dir
}

/// What `git` says each tag is: (name, ref value, annotated?, commit it peels to).
fn git_expectations(dir: &Path) -> Vec<TagInfo> {
    let listing = git(
        dir,
        &[
            "for-each-ref",
            "--format=%(refname:strip=2) %(objectname) %(objecttype)",
            "refs/tags/",
        ],
    );
    listing
        .lines()
        .map(|line| {
            let mut parts = line.split(' ');
            let name = parts.next().unwrap().to_owned();
            let oid: ObjectId = parts.next().unwrap().parse().unwrap();
            let kind = if parts.next() == Some("tag") {
                TagKind::Annotated
            } else {
                TagKind::Lightweight
            };
            let peeled = git_cmd(&["rev-parse", "-q", "--verify", &format!("{oid}^{{commit}}")])
                .in_dir(dir)
                .exec();
            let peeled = peeled
                .ok()
                .then(|| peeled.stdout.trim().parse::<ObjectId>().expect("oid"));
            TagInfo {
                name,
                oid,
                kind,
                peeled,
            }
        })
        .collect()
}

fn rewrite_packed_header(dir: &Path, header: &str) {
    let path = dir.join(".git/packed-refs");
    let body = std::fs::read_to_string(&path).unwrap();
    let rest = body
        .strip_prefix('#')
        .and_then(|b| b.split_once('\n'))
        .map_or(body.as_str(), |(_, rest)| rest);
    std::fs::write(&path, format!("{header}{rest}")).unwrap();
}

#[test]
fn packed_tags_classify_like_git() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = packed_fixture(tmp.path());
    assert_eq!(list_tags(&open(&dir)).unwrap(), git_expectations(&dir));
}

#[test]
fn every_peel_trait_classifies_like_git() {
    for header in [
        "# pack-refs with: peeled fully-peeled sorted \n",
        "# pack-refs with: peeled sorted \n",
        "# pack-refs with: peeled \n",
        "# pack-refs with: sorted \n",
        "",
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let dir = packed_fixture(tmp.path());
        rewrite_packed_header(&dir, header);
        assert_eq!(
            list_tags(&open(&dir)).unwrap(),
            git_expectations(&dir),
            "header {header:?}"
        );
    }
}

/// With `fully-peeled`, an annotated tag is classified from its peel line alone: the tag
/// object is never read, so the listing survives the object going missing.
#[test]
fn fully_peeled_listing_does_not_read_tag_objects() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = packed_fixture(tmp.path());
    let expected = git_expectations(&dir);
    let annotated = rev(&dir, "refs/tags/annotated");
    let hex = annotated.to_hex();
    let loose = dir.join(".git/objects").join(&hex[..2]).join(&hex[2..]);
    assert!(loose.is_file(), "annotated tag object should be loose");
    std::fs::remove_file(&loose).unwrap();

    let tags = list_tags(&open(&dir)).unwrap();
    let ours = tags.iter().find(|t| t.name == "annotated").unwrap();
    let theirs = expected.iter().find(|t| t.name == "annotated").unwrap();
    assert_eq!(ours, theirs);
}

/// A loose ref shadows its packed record, whose peel line then no longer applies.
#[test]
fn loose_tag_overriding_packed_record_is_peeled_from_objects() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = packed_fixture(tmp.path());
    git(&dir, &["tag", "-f", "annotated", "main~1"]);
    git(
        &dir,
        &["tag", "-f", "-a", "light", "-m", "now annotated", "main"],
    );
    assert!(dir.join(".git/refs/tags/annotated").is_file());

    let tags = list_tags(&open(&dir)).unwrap();
    assert_eq!(tags, git_expectations(&dir));
    let annotated = tags.iter().find(|t| t.name == "annotated").unwrap();
    assert_eq!(annotated.kind, TagKind::Lightweight);
    let light = tags.iter().find(|t| t.name == "light").unwrap();
    assert_eq!(light.kind, TagKind::Annotated);
}

#[test]
fn unsorted_packed_refs_classify_like_git() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = packed_fixture(tmp.path());
    let path = dir.join(".git/packed-refs");
    let body = std::fs::read_to_string(&path).unwrap();
    let mut records: Vec<String> = Vec::new();
    for line in body.lines().skip(1) {
        if line.starts_with('^') {
            let last = records.last_mut().unwrap();
            last.push('\n');
            last.push_str(line);
        } else {
            records.push(line.to_owned());
        }
    }
    records.reverse();
    std::fs::write(
        &path,
        format!(
            "# pack-refs with: peeled fully-peeled \n{}\n",
            records.join("\n")
        ),
    )
    .unwrap();
    assert_eq!(list_tags(&open(&dir)).unwrap(), git_expectations(&dir));
}

fn lightweight(name: &str, target: ObjectId, force: bool) -> CreateTagRequest {
    CreateTagRequest {
        name: name.into(),
        target,
        message: None,
        tagger: None,
        sign: None,
        signing_key: None,
        force,
        dry_run: false,
    }
}

#[test]
fn create_tag_sees_packed_tags_and_writes_refs_git_reads() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = packed_fixture(tmp.path());
    let repo = open(&dir);
    let head = rev(&dir, "HEAD");
    let older = rev(&dir, "main~1");

    let err = create_tag(&repo, &lightweight("light", head, false)).unwrap_err();
    assert!(
        matches!(&err, Error::Tag(TagError::AlreadyExists { name }) if name == "light"),
        "{err:?}"
    );

    let replaced = create_tag(&repo, &lightweight("light", head, true)).unwrap();
    assert_eq!(replaced.replaced, Some(older));
    assert_eq!(rev(&dir, "refs/tags/light"), head);

    let created = create_tag(&repo, &lightweight("fresh", older, false)).unwrap();
    assert_eq!(created.replaced, None);
    assert_eq!(created.peeled, Some(older));
    assert_eq!(rev(&dir, "refs/tags/fresh"), older);

    git(&dir, &["fsck", "--strict"]);
    assert_eq!(list_tags(&open(&dir)).unwrap(), git_expectations(&dir));
}

#[test]
fn create_tag_refuses_name_under_packed_tag() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = packed_fixture(tmp.path());
    let repo = open(&dir);
    let head = rev(&dir, "HEAD");

    assert!(create_tag(&repo, &lightweight("light/child", head, false)).is_err());
    assert!(!dir.join(".git/refs/tags/light/child").exists());
    assert!(
        !git_cmd(&["rev-parse", "-q", "--verify", "refs/tags/light/child"])
            .in_dir(&dir)
            .exec()
            .ok()
    );
}
