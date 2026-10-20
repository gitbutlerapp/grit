//! `Odb::peel_tags` / `Odb::peel_to_commit` agree with `git rev-parse <tag>^{}` on tags written
//! by system `git`, both as loose objects and after `git gc` packs them.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::repo::Repository;
use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "A")
        .env("GIT_AUTHOR_EMAIL", "a@example.com")
        .env("GIT_COMMITTER_NAME", "C")
        .env("GIT_COMMITTER_EMAIL", "c@example.com")
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

fn oid(dir: &Path, rev: &str) -> ObjectId {
    git(dir, &["rev-parse", rev]).trim().parse().expect("oid")
}

/// Every tag in the fixture, with the kind git reports for `<tag>^{}`.
const TAGS: [(&str, ObjectKind); 5] = [
    ("light", ObjectKind::Commit),
    ("annotated", ObjectKind::Commit),
    ("nested", ObjectKind::Commit),
    ("of-tree", ObjectKind::Tree),
    ("of-blob", ObjectKind::Blob),
];

fn assert_peels_match_git(root: &Path) {
    let repo =
        Repository::discover(Some(root), &RepositoryOptions::empty()).expect("discover repo");
    for (name, kind) in TAGS {
        let tip = oid(root, &format!("refs/tags/{name}"));
        let git_peeled = oid(root, &format!("refs/tags/{name}^{{}}"));
        assert_eq!(
            repo.odb().peel_tags(tip).expect("peel"),
            (git_peeled, kind),
            "{name}"
        );
        let expected_commit = (kind == ObjectKind::Commit).then_some(git_peeled);
        assert_eq!(
            repo.odb().peel_to_commit(tip).expect("peel to commit"),
            expected_commit,
            "{name}"
        );
    }
}

#[test]
fn peel_matches_git_for_loose_and_packed_tags() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("f"), "x\n").unwrap();
    git(root, &["add", "f"]);
    git(root, &["commit", "-qm", "seed"]);
    git(root, &["tag", "light"]);
    git(root, &["tag", "-a", "annotated", "-m", "a"]);
    git(root, &["tag", "-a", "nested", "annotated", "-m", "n"]);
    git(root, &["tag", "-a", "of-tree", "HEAD^{tree}", "-m", "t"]);
    git(root, &["tag", "-a", "of-blob", "HEAD:f", "-m", "b"]);

    assert_peels_match_git(root);
    git(root, &["gc", "-q"]);
    assert_peels_match_git(root);
    git(root, &["fsck", "--strict"]);
}

#[test]
fn peel_reports_missing_tag_target() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    let missing = "1111111111111111111111111111111111111111";
    let body = format!(
        "object {missing}\ntype commit\ntag dangling\ntagger T <t@example.com> 0 +0000\n\nd\n"
    );
    let path = root.join("tag.txt");
    std::fs::write(&path, body).unwrap();
    let tag: ObjectId = git(
        root,
        &["hash-object", "-t", "tag", "-w", "--literally", "tag.txt"],
    )
    .trim()
    .parse()
    .expect("tag oid");

    let repo =
        Repository::discover(Some(root), &RepositoryOptions::empty()).expect("discover repo");
    assert!(matches!(
        repo.odb().peel_to_commit(tag),
        Err(grit_lib::error::Error::ObjectNotFound(_))
    ));
}
