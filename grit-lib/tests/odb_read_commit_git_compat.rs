//! `Odb::read_commit` / `Odb::commit_tree` (and the `Objects` forwards) agree with
//! `git rev-parse` on commits written by system `git`, both loose and after `git gc` packs them.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
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

fn assert_commits_match_git(root: &Path) {
    let repo =
        Repository::discover(Some(root), &RepositoryOptions::empty()).expect("discover repo");
    for rev in ["HEAD", "HEAD~1", "HEAD~2", "side", "merge"] {
        let commit = oid(root, rev);
        let data = repo.odb().read_commit(&commit).expect("read commit");
        let tree = oid(root, &format!("{rev}^{{tree}}"));
        assert_eq!(data.tree, tree, "{rev}");
        let parents: Vec<ObjectId> = git(root, &["rev-list", "--parents", "-n1", rev])
            .split_whitespace()
            .skip(1)
            .map(|p| p.parse().expect("parent"))
            .collect();
        assert_eq!(data.parents, parents, "{rev}");
        assert_eq!(
            data.message,
            git(root, &["log", "-1", "--format=%B", rev])
                .trim_end()
                .to_owned()
                + "\n",
            "{rev}"
        );
        assert_eq!(
            repo.odb().commit_tree(&commit).expect("tree"),
            tree,
            "{rev}"
        );
        assert_eq!(repo.objects().read_commit(&commit).expect("read"), data);
        assert_eq!(repo.objects().commit_tree(&commit).expect("tree"), tree);
    }

    for (rev, actual) in [
        ("HEAD^{tree}", ObjectKind::Tree),
        ("HEAD:f", ObjectKind::Blob),
        ("refs/tags/annotated", ObjectKind::Tag),
    ] {
        let id = oid(root, rev);
        for err in [
            repo.odb().read_commit(&id).expect_err(rev),
            repo.odb().commit_tree(&id).expect_err(rev),
        ] {
            assert!(
                matches!(
                    err,
                    Error::UnexpectedObjectKind { oid, expected: ObjectKind::Commit, actual: kind }
                        if oid == id && kind == actual
                ),
                "{rev}: {err:?}"
            );
        }
    }
}

#[test]
fn read_commit_matches_git_for_loose_and_packed_commits() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("f"), "x\n").unwrap();
    git(root, &["add", "f"]);
    git(root, &["commit", "-qm", "seed"]);
    git(root, &["branch", "side"]);
    std::fs::write(root.join("f"), "y\n").unwrap();
    git(root, &["commit", "-qam", "second\n\nwith a body"]);
    git(root, &["switch", "-q", "side"]);
    std::fs::write(root.join("g"), "z\n").unwrap();
    git(root, &["add", "g"]);
    git(root, &["commit", "-qm", "side"]);
    git(root, &["switch", "-q", "main"]);
    git(root, &["merge", "-q", "--no-edit", "side"]);
    git(root, &["branch", "merge"]);
    git(root, &["tag", "-a", "annotated", "-m", "a"]);

    assert_commits_match_git(root);
    git(root, &["gc", "-q"]);
    assert_commits_match_git(root);
    git(root, &["fsck", "--strict"]);
}
