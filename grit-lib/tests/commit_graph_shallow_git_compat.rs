//! Commit-graph writes in repositories whose parent lists are rewritten (shallow clones,
//! `info/grafts`, replace refs): grit must not leave a graph that system `git` rejects.

use std::path::{Path, PathBuf};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::maintenance::{
    gc, write_commit_graph, CommitGraphOptions, CommitGraphSkipReason, GcOptions,
};
use grit_lib::repo::Repository;
use grit_test_support::{git, git_commit_graph_verify, git_fsck};
use tempfile::TempDir;

fn commit(dir: &Path, file: &str) {
    std::fs::write(dir.join(file), format!("{file}\n")).expect("write file");
    git(dir, &["add", "."]);
    git(
        dir,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "-m",
            file,
        ],
    );
}

/// `c0 - x1 - c2` plus a merge `M(c2, x1)` on `main`, the topology from the bug report.
///
/// A `--depth 2` clone of `M` holds `M`, `c2` and `x1`, with `c2` and `x1` as shallow
/// boundaries; `c2`'s real parent `x1` is present through the merge.
fn merge_source(root: &Path) -> PathBuf {
    let src = root.join("src");
    std::fs::create_dir_all(&src).expect("mkdir src");
    git(&src, &["init", "-q", "-b", "main"]);
    for file in ["c0", "x1", "c2"] {
        commit(&src, file);
    }
    let merge = git(
        &src,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit-tree",
            "-p",
            "main",
            "-p",
            "main~1",
            "-m",
            "M",
            "main^{tree}",
        ],
    );
    git(&src, &["update-ref", "refs/heads/main", merge.trim()]);
    src
}

fn shallow_clone(root: &Path, src: &Path) -> PathBuf {
    let dst = root.join("shallow");
    let url = format!("file://{}", src.display());
    git(
        root,
        &[
            "clone",
            "-q",
            "--depth",
            "2",
            &url,
            dst.to_str().expect("utf8 path"),
        ],
    );
    let shallow = std::fs::read_to_string(dst.join(".git/shallow")).expect("read .git/shallow");
    assert_eq!(
        shallow.lines().count(),
        2,
        "expected two boundaries: {shallow}"
    );
    dst
}

fn open(dir: &Path) -> Repository {
    Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("open repository")
}

fn graph_path(dir: &Path) -> PathBuf {
    dir.join(".git/objects/info/commit-graph")
}

fn assert_git_accepts(dir: &Path) {
    let fsck = git_fsck(dir, false);
    assert!(
        fsck.ok,
        "git fsck rejected the repository: {:?}",
        fsck.msg_ids
    );
    let verify = git_commit_graph_verify(dir);
    assert!(
        verify.ok,
        "git commit-graph verify failed: {}",
        verify.output
    );
}

/// Copy the full-history graph git writes in `src` into `dst`: in a shallow clone it names
/// commits and parent edges that `.git/shallow` hides, like graphs older grit wrote.
fn seed_foreign_graph(src: &Path, dst: &Path) {
    git(src, &["commit-graph", "write", "--reachable"]);
    std::fs::copy(graph_path(src), graph_path(dst)).expect("copy commit-graph");
}

fn linear_repo(root: &Path) -> PathBuf {
    let dir = root.join("linear");
    std::fs::create_dir_all(&dir).expect("mkdir");
    git(&dir, &["init", "-q", "-b", "main"]);
    for file in ["a", "b", "c"] {
        commit(&dir, file);
    }
    dir
}

#[test]
fn gc_in_shallow_clone_skips_commit_graph_and_git_fsck_passes() {
    let tmp = TempDir::new().expect("tempdir");
    let src = merge_source(tmp.path());
    let dst = shallow_clone(tmp.path(), &src);

    let repo = open(&dst);
    let stats = gc(&repo, &GcOptions::full_run(false, 1_700_000_000)).expect("gc");
    let graph = stats
        .commit_graph
        .expect("gc reports the commit-graph task");
    assert_eq!(graph.skipped, Some(CommitGraphSkipReason::Shallow));
    assert_eq!(graph.commit_count, 0);
    assert!(
        !graph_path(&dst).exists(),
        "no graph in a shallow repository"
    );
    assert_git_accepts(&dst);
}

#[test]
fn write_commit_graph_in_shallow_clone_removes_existing_graph() {
    let tmp = TempDir::new().expect("tempdir");
    let src = merge_source(tmp.path());
    let dst = shallow_clone(tmp.path(), &src);
    seed_foreign_graph(&src, &dst);
    let chain_dir = dst.join(".git/objects/info/commit-graphs");
    std::fs::create_dir_all(&chain_dir).expect("mkdir chain");
    std::fs::write(chain_dir.join("commit-graph-chain"), "").expect("write chain");
    assert!(
        !git_fsck(&dst, false).ok,
        "the seeded graph must reproduce the broken state"
    );

    let repo = open(&dst);
    let report = write_commit_graph(&repo, &CommitGraphOptions::with_changed_paths(true))
        .expect("write commit-graph");
    assert_eq!(report.skipped, Some(CommitGraphSkipReason::Shallow));
    assert!(report.removed_chain);
    assert!(report.bloom_stats.is_none());
    assert!(!graph_path(&dst).exists(), "stale graph removed");
    assert!(!chain_dir.exists(), "stale chain removed");
    assert_git_accepts(&dst);
}

#[test]
fn dry_run_in_shallow_clone_reports_skip_without_touching_graph() {
    let tmp = TempDir::new().expect("tempdir");
    let src = merge_source(tmp.path());
    let dst = shallow_clone(tmp.path(), &src);
    seed_foreign_graph(&src, &dst);
    let before = std::fs::read(graph_path(&dst)).expect("read seeded graph");

    let repo = open(&dst);
    let report = write_commit_graph(&repo, &CommitGraphOptions::with_dry_run(true))
        .expect("dry-run commit-graph");
    assert_eq!(report.skipped, Some(CommitGraphSkipReason::Shallow));
    assert!(!report.removed_chain);
    assert_eq!(std::fs::read(graph_path(&dst)).expect("graph kept"), before);
}

#[test]
fn odb_write_commit_graph_in_shallow_clone_writes_nothing() {
    let tmp = TempDir::new().expect("tempdir");
    let src = merge_source(tmp.path());
    let dst = shallow_clone(tmp.path(), &src);
    seed_foreign_graph(&src, &dst);

    let repo = open(&dst);
    repo.odb().write_commit_graph().expect("odb commit-graph");
    assert!(!graph_path(&dst).exists());
    assert_git_accepts(&dst);
}

#[test]
fn skip_reason_identifiers_are_stable() {
    assert_eq!(CommitGraphSkipReason::Shallow.as_str(), "shallow");
    assert_eq!(CommitGraphSkipReason::Grafts.as_str(), "grafts");
    assert_eq!(CommitGraphSkipReason::ReplaceRefs.as_str(), "replace-refs");
}

#[test]
fn grafts_skip_commit_graph_write() {
    let tmp = TempDir::new().expect("tempdir");
    let dir = linear_repo(tmp.path());
    let head = git(&dir, &["rev-parse", "HEAD"]);
    std::fs::write(dir.join(".git/info/grafts"), head).expect("write grafts");

    let repo = open(&dir);
    let report = write_commit_graph(&repo, &CommitGraphOptions::default()).expect("write");
    assert_eq!(report.skipped, Some(CommitGraphSkipReason::Grafts));
    assert!(!graph_path(&dir).exists());
}

#[test]
fn comment_only_grafts_file_does_not_skip() {
    let tmp = TempDir::new().expect("tempdir");
    let dir = linear_repo(tmp.path());
    std::fs::write(dir.join(".git/info/grafts"), "# no grafts\n").expect("write grafts");

    let repo = open(&dir);
    let report = write_commit_graph(&repo, &CommitGraphOptions::default()).expect("write");
    assert_eq!(report.skipped, None);
    assert_eq!(report.commit_count, 3);
}

#[test]
fn replace_refs_skip_commit_graph_write_unless_disabled() {
    let tmp = TempDir::new().expect("tempdir");
    let dir = linear_repo(tmp.path());
    git(
        &dir,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "replace",
            "--graft",
            "HEAD",
        ],
    );
    git(&dir, &["pack-refs", "--all"]);
    assert!(
        !dir.join(".git/refs/replace")
            .read_dir()
            .is_ok_and(|mut d| d.next().is_some()),
        "replace ref is only in packed-refs"
    );

    let repo = open(&dir);
    let report = write_commit_graph(&repo, &CommitGraphOptions::default()).expect("write");
    assert_eq!(report.skipped, Some(CommitGraphSkipReason::ReplaceRefs));
    assert!(!graph_path(&dir).exists());

    git(&dir, &["config", "core.useReplaceRefs", "false"]);
    let repo = open(&dir);
    let report = write_commit_graph(&repo, &CommitGraphOptions::default()).expect("write");
    assert_eq!(report.skipped, None);
    assert_eq!(report.commit_count, 4, "three commits plus the replacement");
    assert!(graph_path(&dir).exists());
    assert_git_accepts(&dir);
}

#[test]
fn full_clone_still_writes_a_graph_git_verifies() {
    let tmp = TempDir::new().expect("tempdir");
    let src = merge_source(tmp.path());

    let repo = open(&src);
    let report = write_commit_graph(&repo, &CommitGraphOptions::default()).expect("write");
    assert_eq!(report.skipped, None);
    assert_eq!(report.commit_count, 4);
    assert!(graph_path(&src).exists());
    assert_git_accepts(&src);
}
