//! System-Git fixture shared by local-fetch compatibility tests and benchmarks.

use std::path::{Path, PathBuf};

use grit_test_support::{git, git_cmd};

/// Create a bare remote and cloned client under `root` with the requested extra refs.
/// Return their paths; panic if a fixture command fails.
pub fn fixture(root: &Path, branches: usize, tags: usize, reftable: bool) -> (PathBuf, PathBuf) {
    let seed = root.join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "-q", "-b", "main"]);
    std::fs::write(seed.join("f"), "base\n").unwrap();
    git(&seed, &["add", "f"]);
    git(&seed, &["commit", "-qm", "base"]);
    git(root, &["clone", "--bare", "seed", "remote.git"]);
    let remote = root.join("remote.git");
    let oid = git(&remote, &["rev-parse", "HEAD"]);
    let mut input = String::new();
    for i in 0..branches {
        input.push_str(&format!("update refs/heads/team/{i:04} {}\n", oid.trim()));
    }
    for i in 0..tags {
        input.push_str(&format!("update refs/tags/v{i:04} {}\n", oid.trim()));
    }
    git_cmd(&["update-ref", "--stdin"])
        .in_dir(&remote)
        .stdin(input)
        .suc();
    git(&remote, &["pack-refs", "--all"]);
    git(root, &["clone", "remote.git", "client"]);
    let client = root.join("client");
    if reftable {
        git(&remote, &["refs", "migrate", "--ref-format=reftable"]);
        git(&client, &["refs", "migrate", "--ref-format=reftable"]);
    }
    (remote, client)
}
