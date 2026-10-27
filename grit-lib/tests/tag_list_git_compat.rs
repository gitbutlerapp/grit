//! Annotated tag list metadata matches `git for-each-ref` (subject, tagger, date).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::porcelain::tag::{list_tags, TagKind};
use grit_lib::refs::listing::{RefNamespace, RefQuery, RefSort, SortKey};
use grit_lib::repo::Repository;
use std::process::Command;

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn open(dir: &std::path::Path) -> Repository {
    Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("discover")
}

fn git_tag_field(dir: &std::path::Path, name: &str, field: &str) -> String {
    git(
        dir,
        &[
            "for-each-ref",
            &format!("refs/tags/{name}"),
            &format!("--format=%({field})"),
        ],
    )
    .trim()
    .to_owned()
}

fn name_query() -> RefQuery {
    RefQuery {
        namespaces: vec![RefNamespace::Tags],
        sort: RefSort {
            key: SortKey::Name,
            reverse: false,
        },
        ..RefQuery::default()
    }
}

/// Signed tags append armor directly after the message; Git's `%(contents:subject)`
/// ignores the signature even when there is no blank line before the armor.
#[test]
fn signed_tag_subject_ignores_appended_signature_without_blank_line() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["commit", "--allow-empty", "-q", "-m", "seed"]);
    let commit = git(&root, &["rev-parse", "HEAD"]).trim().to_string();
    let body = format!(
        "object {commit}\ntype commit\ntag 0.1.0\ntagger T <t@example.com> 946684800 +0000\n\n\
         Version 0.1.0\n-----BEGIN PGP SIGNATURE-----\nVersion: GnuPG v1\n-----END PGP SIGNATURE-----\n"
    );
    let mut child = Command::new("git")
        .args(["hash-object", "-w", "-t", "tag", "--stdin"])
        .current_dir(&root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("git hash-object");
    std::io::Write::write_all(child.stdin.as_mut().expect("stdin"), body.as_bytes()).unwrap();
    let out = child.wait_with_output().expect("wait");
    assert!(
        out.status.success(),
        "hash-object: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let tag_oid = String::from_utf8_lossy(&out.stdout).trim().to_string();
    git(&root, &["update-ref", "refs/tags/0.1.0", &tag_oid]);

    let repo = open(&root);
    let tags = list_tags(&repo, &name_query()).unwrap();
    let tag = tags.iter().find(|t| t.name == "0.1.0").expect("0.1.0");
    let ann = tag.annotation.as_ref().expect("annotated");
    assert_eq!(
        ann.subject,
        git_tag_field(&root, "0.1.0", "contents:subject"),
    );
    assert_eq!(ann.subject, "Version 0.1.0");
    assert!(ann.message.contains("-----BEGIN PGP SIGNATURE-----"));
}

#[test]
fn annotation_fields_match_git_for_each_ref() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["commit", "--allow-empty", "-q", "-m", "seed"]);
    git(
        &root,
        &[
            "tag",
            "-a",
            "release",
            "-m",
            "First line\n\nBody line",
            "HEAD",
        ],
    );
    git(&root, &["tag", "-a", "inner", "-m", "inner tag", "HEAD"]);
    git(&root, &["tag", "-a", "outer", "-m", "outer tag", "inner"]);

    let tree = git(&root, &["rev-parse", "HEAD^{tree}"]).trim().to_string();
    git(&root, &["tag", "-a", "tree-tag", "-m", "tree", &tree]);

    let repo = open(&root);
    let tags = list_tags(&repo, &name_query()).unwrap();
    for tag in &tags {
        if tag.kind != TagKind::Annotated {
            continue;
        }
        let ann = tag.annotation.as_ref().expect("annotation");
        assert_eq!(
            ann.subject,
            git_tag_field(&root, &tag.name, "contents:subject"),
            "{}",
            tag.name
        );
        assert_eq!(
            ann.tagger.name,
            git_tag_field(&root, &tag.name, "taggername"),
            "{}",
            tag.name
        );
        assert_eq!(
            ann.date,
            git_tag_field(&root, &tag.name, "taggerdate:unix")
                .parse::<i64>()
                .expect("unix"),
            "{}",
            tag.name
        );
        match tag.target_kind {
            ObjectKind::Commit | ObjectKind::Tag | ObjectKind::Tree => {}
            other => panic!("unexpected target kind {other:?} for {}", tag.name),
        }
    }

    let tree_tag = tags
        .iter()
        .find(|t| t.name == "tree-tag")
        .expect("tree-tag");
    assert_eq!(tree_tag.target_kind, ObjectKind::Tree);
    let outer = tags.iter().find(|t| t.name == "outer").expect("outer");
    assert_eq!(outer.target_kind, ObjectKind::Tag);
    let _inner_oid: ObjectId = git(&root, &["rev-parse", "inner^{}"])
        .trim()
        .parse()
        .unwrap();
}

fn oid(s: &str) -> ObjectId {
    s.trim().parse().expect("oid")
}

fn kind_from_git(s: &str) -> ObjectKind {
    match s {
        "commit" => ObjectKind::Commit,
        "tree" => ObjectKind::Tree,
        "blob" => ObjectKind::Blob,
        "tag" => ObjectKind::Tag,
        other => panic!("unexpected object type {other:?}"),
    }
}

/// The `type` header of the tag object `oid` (the kind of object it points at directly).
fn tag_type_header(root: &std::path::Path, oid: &str) -> String {
    git(root, &["cat-file", "tag", oid])
        .lines()
        .find_map(|line| line.strip_prefix("type "))
        .expect("tag object type header")
        .to_owned()
}

/// Every listed tag's kind, peeled id and target kind, checked against `git for-each-ref`.
fn assert_tags_classified_like_git(root: &std::path::Path) {
    let repo = open(root);
    let tags = list_tags(&repo, &name_query()).unwrap();
    let rows = git(
        root,
        &[
            "for-each-ref",
            "--sort=refname",
            "--format=%(refname:short) %(objectname) %(objecttype)",
            "refs/tags/",
        ],
    );
    let rows: Vec<&str> = rows.lines().collect();
    assert_eq!(tags.len(), rows.len(), "tag count");
    for (tag, row) in tags.iter().zip(rows) {
        let fields: Vec<&str> = row.split(' ').collect();
        let (name, git_oid, git_type) = (fields[0], fields[1], fields[2]);
        assert_eq!(tag.name, name);
        assert_eq!(tag.oid, oid(git_oid), "{name}");
        if git_type == "tag" {
            assert_eq!(tag.kind, TagKind::Annotated, "{name}");
            // `target_kind` is the tag object's own `type` header (one level). Git's
            // `%(*objecttype)` peels nested tags fully, so read the header directly.
            assert_eq!(
                tag.target_kind,
                kind_from_git(&tag_type_header(root, git_oid)),
                "{name}"
            );
            let fully_peeled = oid(&git(root, &["rev-parse", &format!("{git_oid}^{{}}")]));
            assert_eq!(tag.peeled, Some(fully_peeled), "{name}");
            assert!(tag.annotation.is_some(), "{name}");
        } else {
            assert_eq!(tag.kind, TagKind::Lightweight, "{name}");
            assert_eq!(tag.target_kind, kind_from_git(git_type), "{name}");
            let expected = (git_type == "commit").then(|| oid(git_oid));
            assert_eq!(tag.peeled, expected, "{name}");
            assert!(tag.annotation.is_none(), "{name}");
        }
    }
}

fn seed_mixed_tags(root: &std::path::Path) {
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["commit", "--allow-empty", "-q", "-m", "seed"]);
    git(root, &["commit", "--allow-empty", "-q", "-m", "second"]);
    git(root, &["tag", "light-commit", "HEAD~1"]);
    git(root, &["tag", "-a", "annotated", "-m", "subject", "HEAD"]);
    git(root, &["tag", "-a", "inner", "-m", "inner", "HEAD~1"]);
    git(root, &["tag", "-a", "outer", "-m", "outer", "inner"]);
    let tree = git(root, &["rev-parse", "HEAD^{tree}"]);
    git(root, &["tag", "light-tree", tree.trim()]);
    git(root, &["tag", "-a", "tree-tag", "-m", "tree", tree.trim()]);
    std::fs::write(root.join("blob.txt"), "blob\n").unwrap();
    let blob = git(root, &["hash-object", "-w", "blob.txt"]);
    git(root, &["tag", "light-blob", blob.trim()]);
}

#[test]
fn list_classification_matches_git_for_loose_packed_and_shadowed_tags() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    seed_mixed_tags(&root);
    assert_tags_classified_like_git(&root);

    git(&root, &["pack-refs", "--all"]);
    let packed = std::fs::read_to_string(root.join(".git/packed-refs")).unwrap();
    assert!(packed.starts_with("# pack-refs with: peeled fully-peeled"));
    assert_tags_classified_like_git(&root);

    // Loose refs that shadow packed records with a different value, and refs packed-refs
    // never saw, must be peeled through the objects.
    git(
        &root,
        &[
            "tag",
            "-f",
            "-a",
            "light-commit",
            "-m",
            "now annotated",
            "HEAD",
        ],
    );
    git(&root, &["tag", "-f", "annotated", "HEAD~1"]);
    git(&root, &["tag", "-a", "loose-new", "-m", "loose", "HEAD"]);
    git(&root, &["tag", "loose-light", "HEAD"]);
    assert_tags_classified_like_git(&root);
}

#[test]
fn packed_peel_lines_answer_without_reading_tag_objects() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["commit", "--allow-empty", "-q", "-m", "seed"]);
    git(
        &root,
        &["tag", "-a", "gone", "-m", "object will be removed", "HEAD"],
    );
    git(&root, &["tag", "light", "HEAD"]);
    // Enough refs that the listing indexes packed-refs instead of reading each object.
    let filler: String = (0..70)
        .map(|i| format!("create refs/tags/filler-{i:03} HEAD\n"))
        .collect();
    let mut child = Command::new("git")
        .args(["update-ref", "--stdin"])
        .current_dir(&root)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("git update-ref");
    std::io::Write::write_all(child.stdin.as_mut().expect("stdin"), filler.as_bytes()).unwrap();
    assert!(child.wait().expect("wait").success());
    git(&root, &["pack-refs", "--all"]);
    let tag_oid = oid(&git(&root, &["rev-parse", "refs/tags/gone"]));
    let commit = oid(&git(&root, &["rev-parse", "HEAD"]));
    let hex = tag_oid.to_hex();
    std::fs::remove_file(root.join(".git/objects").join(&hex[..2]).join(&hex[2..])).unwrap();

    let repo = open(&root);
    let options = grit_lib::refs::listing::ref_query_options_for_repo(&repo).unwrap();
    let listings = grit_lib::refs::listing::query_refs(&repo, &name_query(), &options).unwrap();
    let gone = listings
        .iter()
        .find(|l| l.short_name == "gone")
        .expect("gone");
    assert_eq!(gone.oid, tag_oid);
    assert_eq!(gone.peeled, Some(commit), "peel comes from packed-refs");
    let light = listings
        .iter()
        .find(|l| l.short_name == "light")
        .expect("light");
    assert_eq!(light.peeled, None);

    let tags = list_tags(&repo, &name_query()).unwrap();
    let gone = tags.iter().find(|t| t.name == "gone").expect("gone");
    assert_eq!(gone.kind, TagKind::Lightweight, "unreadable tag object");
    assert_eq!(gone.peeled, Some(commit));
    assert!(gone.annotation.is_none());
    let light = tags.iter().find(|t| t.name == "light").expect("light");
    assert_eq!(light.kind, TagKind::Lightweight);
    assert_eq!(light.peeled, Some(commit));
    assert_eq!(light.target_kind, ObjectKind::Commit);
}
