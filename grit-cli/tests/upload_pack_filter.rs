//! Partial clones served by `grit upload-pack` to the system `git` client (issue #1819):
//! annotated tags that share a commit must all arrive with a `--filter`, and the client
//! must end up with a valid partial clone.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use grit_test_support::git;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

/// A served bare repository where `main` moved past a side commit carrying two annotated
/// tags and a tag of one of them, with partial clone enabled.
fn served_repo(dir: &Path) -> PathBuf {
    let work = dir.join("work");
    std::fs::create_dir_all(work.join("src")).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("src/lib.rs"), "fn one() {}\n").unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "one"]);
    git(&work, &["checkout", "-q", "-b", "release"]);
    std::fs::write(work.join("CHANGELOG"), "0.58\n").unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "release"]);
    git(&work, &["tag", "-a", "-m", "short", "0.58"]);
    git(&work, &["tag", "-a", "-m", "long", "0.58.0"]);
    git(&work, &["tag", "-a", "-m", "nested", "0.58-signed", "0.58"]);
    git(&work, &["checkout", "-q", "main"]);
    std::fs::write(work.join("src/lib.rs"), "fn one() {}\nfn two() {}\n").unwrap();
    git(&work, &["commit", "-q", "-am", "two"]);
    let bare = dir.join("served.git");
    git(
        dir,
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    git(&bare, &["config", "uploadpack.allowFilter", "true"]);
    bare
}

fn upload_pack_arg() -> String {
    format!("--upload-pack={GRIT} upload-pack")
}

fn file_url(path: &Path) -> String {
    format!("file://{}", path.display())
}

/// Checks the objects stored locally: `cat-file -e` would fetch a missing object from
/// the promisor remote instead of failing.
fn assert_has_object(repo: &Path, name: &str, oid: &str) {
    let stored = git(
        repo,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname)",
        ],
    );
    assert!(
        stored.lines().any(|line| line == oid),
        "{name} ({oid}) missing in {}",
        repo.display()
    );
}

#[test]
fn filtered_fetch_of_one_tag_brings_every_tag_on_its_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = served_repo(tmp.path());
    let client = tmp.path().join("client.git");
    git(
        tmp.path(),
        &["init", "-q", "--bare", client.to_str().unwrap()],
    );
    git(&client, &["remote", "add", "origin", &file_url(&bare)]);
    git(
        &client,
        &[
            "fetch",
            "-q",
            "--filter=blob:none",
            &upload_pack_arg(),
            "origin",
            "refs/tags/0.58.0:refs/tags/0.58.0",
        ],
    );
    for tag in ["0.58", "0.58.0", "0.58-signed"] {
        let oid = git(&bare, &["rev-parse", &format!("refs/tags/{tag}")]);
        assert_has_object(&client, tag, oid.trim());
    }
    git(&client, &["fsck", "--no-dangling"]);
}

#[test]
fn filtered_clone_matches_git_upload_pack() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = served_repo(tmp.path());
    let url = file_url(&bare);
    let mut objects = Vec::new();
    for (name, upload_pack) in [("grit", Some(upload_pack_arg())), ("git", None)] {
        let dest = tmp.path().join(format!("{name}-clone.git"));
        let mut args = vec!["clone", "-q", "--bare", "--filter=blob:none"];
        if let Some(arg) = upload_pack.as_deref() {
            args.push(arg);
        }
        args.extend([url.as_str(), dest.to_str().unwrap()]);
        git(tmp.path(), &args);
        git(&dest, &["fsck", "--no-dangling"]);
        let mut listed: Vec<String> = git(
            &dest,
            &[
                "cat-file",
                "--batch-all-objects",
                "--batch-check=%(objectname) %(objecttype)",
            ],
        )
        .lines()
        .map(str::to_owned)
        .collect();
        listed.sort();
        objects.push((git(&dest, &["for-each-ref"]), listed));
    }
    assert_eq!(objects[0], objects[1], "same refs and objects as git");
    assert!(
        objects[0].1.iter().all(|line| !line.ends_with(" blob")),
        "blob:none clone holds no blobs"
    );
}
