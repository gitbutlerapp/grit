//! A large untracked cache (UNTR) written by system `git` survives a grit index
//! load and write byte for byte, and git reuses the result without rebuilding it.

use crate::support;

use std::fs;
use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::index::untracked_cache::UntrackedCacheDir;
use grit_lib::index::{IndexLoadOptions, IndexWriteOptions};
use grit_lib::repo::Repository;

use support::{
    extension_payload, fsck_strict, git, git_status_porcelain_normal_with_trace, read_index_bytes,
    trace2_read_directory_counter,
};

/// Per-node flag counts over the whole cache tree.
#[derive(Default)]
struct NodeCounts {
    nodes: usize,
    valid: usize,
    check_only: usize,
    exclude_oid: usize,
}

fn count_nodes(dir: &UntrackedCacheDir, counts: &mut NodeCounts) {
    counts.nodes += 1;
    counts.valid += usize::from(dir.valid);
    counts.check_only += usize::from(dir.check_only);
    counts.exclude_oid += usize::from(!dir.exclude_oid.is_zero());
    for child in &dir.dirs {
        count_nodes(child, counts);
    }
}

/// 40 top-level directories, each a four-level chain of tracked directories, with
/// untracked files, untracked-only directories (check-only in normal mode) and
/// per-directory `.gitignore` files spread over them so every flag bitmap spans
/// several EWAH words.
fn build_repo(root: &Path) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "core.untrackedCache", "true"]);
    for n in 0..40 {
        let top = root.join(format!("w{n:02}"));
        let leaf = top.join("a/b/c");
        fs::create_dir_all(&leaf).expect("mkdir chain");
        fs::write(top.join("t.txt"), format!("top {n}\n")).expect("write");
        fs::write(top.join("a/t.txt"), format!("a {n}\n")).expect("write");
        fs::write(top.join("a/b/t.txt"), format!("b {n}\n")).expect("write");
        fs::write(leaf.join("t.txt"), format!("c {n}\n")).expect("write");
        if n % 5 == 0 {
            fs::write(top.join(".gitignore"), "*.log\n").expect("write gitignore");
        }
    }
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "base"]);
    for n in 0..40 {
        let top = root.join(format!("w{n:02}"));
        if n % 3 == 0 {
            fs::write(top.join("a/u.txt"), "untracked\n").expect("write untracked");
        }
        if n % 4 == 0 {
            let fresh = top.join("a/b/new/deeper");
            fs::create_dir_all(&fresh).expect("mkdir untracked dir");
            fs::write(fresh.join("x.txt"), "x\n").expect("write untracked");
        }
        if n % 5 == 0 {
            fs::write(top.join("a/ignored.log"), "ignored\n").expect("write ignored");
        }
    }
    git(root, &["update-index", "--untracked-cache"]);
    git(
        root,
        &["-c", "core.untrackedCache=true", "status", "--porcelain"],
    );
    git(
        root,
        &["-c", "core.untrackedCache=true", "status", "--porcelain"],
    );
}

#[test]
fn git_written_wide_deep_untracked_cache_round_trips_through_grit() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    build_repo(root);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let git_payload =
        extension_payload(&read_index_bytes(&repo), b"UNTR").expect("git wrote an UNTR extension");

    let mut idx = repo
        .index()
        .load(&IndexLoadOptions::default())
        .expect("load index");
    let uc = idx.untracked_cache.as_ref().expect("grit parsed UNTR");
    let mut counts = NodeCounts::default();
    count_nodes(uc.root.as_ref().expect("cache has a root"), &mut counts);
    assert!(counts.nodes > 128, "only {} cache nodes", counts.nodes);
    assert!(counts.valid > 64, "only {} valid nodes", counts.valid);
    assert!(
        counts.check_only >= 10,
        "only {} check-only",
        counts.check_only
    );
    assert!(counts.exclude_oid >= 8, "only {} oids", counts.exclude_oid);

    let w00 = uc
        .root
        .as_ref()
        .and_then(|r| r.dirs.iter().find(|d| d.name == "w00"))
        .expect("w00 node");
    assert!(!w00.exclude_oid.is_zero(), "w00/.gitignore oid lost");
    let fresh = w00
        .dirs
        .iter()
        .find(|d| d.name == "a")
        .and_then(|a| a.dirs.iter().find(|d| d.name == "b"))
        .and_then(|b| b.dirs.iter().find(|d| d.name == "new"))
        .expect("w00/a/b/new/ node");
    assert!(
        fresh.check_only,
        "untracked-only directory must be check-only"
    );
    let w01 = uc
        .root
        .as_ref()
        .and_then(|r| r.dirs.iter().find(|d| d.name == "w01"))
        .expect("w01 node");
    assert!(w01.exclude_oid.is_zero(), "w01/ has no .gitignore");

    repo.index()
        .write(&mut idx, &IndexWriteOptions::default())
        .expect("write index");
    let grit_payload =
        extension_payload(&read_index_bytes(&repo), b"UNTR").expect("grit kept UNTR");
    assert_eq!(
        grit_payload, git_payload,
        "grit must write back the UNTR payload git wrote"
    );

    let trace = tempfile::NamedTempFile::new().expect("trace file");
    git_status_porcelain_normal_with_trace(root, trace.path());
    assert_eq!(
        trace2_read_directory_counter(trace.path(), "node-creation"),
        Some(0),
        "git must reuse the grit-written cache without creating nodes"
    );
    assert_eq!(
        trace2_read_directory_counter(trace.path(), "gitignore-invalidation"),
        Some(0),
        "git must not invalidate .gitignore data grit wrote back"
    );
    fsck_strict(root);
}
