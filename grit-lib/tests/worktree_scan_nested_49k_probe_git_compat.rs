//! Issue #1622: nested 8×8×8×8×12 (~49k files) worktree scans should stat each path once.
//!
//! `GRIT_WORKTREE_SCAN_49K_NESTED=1 cargo test -p grit-lib --test worktree_scan_nested_49k_probe_git_compat -- --test-threads=1`

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::{diff_index_to_worktree_with_options, DiffIndexToWorktreeOptions};
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::stage_tracked::stage_tracked_modifications;
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_lib::{reset_worktree_metadata_probe, worktree_metadata_probe_count};
use grit_test_support::git;

const BRANCHES: [usize; 4] = [8, 8, 8, 8];
const FILES_PER_LEAF: usize = 12;
const TRACKED: usize = BRANCHES.iter().product::<usize>() * FILES_PER_LEAF;

fn build_fixture(root: &std::path::Path) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    std::fs::write(root.join(".gitattributes"), "*.txt text eol=lf\n").expect("attrs");
    for a in 0..BRANCHES[0] {
        for b in 0..BRANCHES[1] {
            for c in 0..BRANCHES[2] {
                for d in 0..BRANCHES[3] {
                    let leaf = format!("{a:01x}/{b:01x}/{c:01x}/{d:01x}");
                    let abs_leaf = root.join(&leaf);
                    std::fs::create_dir_all(&abs_leaf).expect("mkdir");
                    for f in 0..FILES_PER_LEAF {
                        let path = abs_leaf.join(format!("f{f}.txt"));
                        let size = 10 + ((a + b + c + d + f) % 391);
                        std::fs::write(path, vec![b'x'; size]).expect("write");
                    }
                }
            }
        }
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "big"]);
}

fn open_repo(root: &std::path::Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open")
}

fn touch_hundred_files(root: &std::path::Path) {
    use std::io::Write;
    let mut touched = 0usize;
    'outer: for a in 0..BRANCHES[0] {
        for b in 0..BRANCHES[1] {
            for c in 0..BRANCHES[2] {
                for d in 0..BRANCHES[3] {
                    if (a + b + c + d) % 17 != 0 {
                        continue;
                    }
                    let leaf = format!("{a:01x}/{b:01x}/{c:01x}/{d:01x}");
                    for f in 0..FILES_PER_LEAF {
                        if f % 3 != 0 {
                            continue;
                        }
                        let path = root.join(&leaf).join(format!("f{f}.txt"));
                        let mut file = std::fs::OpenOptions::new()
                            .append(true)
                            .open(&path)
                            .expect("open");
                        writeln!(file, "edit").expect("append");
                        touched += 1;
                        if touched >= 100 {
                            break 'outer;
                        }
                    }
                }
            }
        }
    }
    assert_eq!(touched, 100, "fixture must touch exactly 100 paths");
}

#[test]
fn nested_49k_clean_tracked_scan_metadata_probe_budget() {
    if std::env::var("GRIT_WORKTREE_SCAN_49K_NESTED")
        .ok()
        .as_deref()
        != Some("1")
    {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    build_fixture(root);
    let repo = open_repo(root);

    let _probe = reset_worktree_metadata_probe(root);
    let summary = stage_tracked_modifications(&repo, root).expect("tracked scan");
    assert_eq!(summary.change_count(), 0, "clean tree");
    let meta = worktree_metadata_probe_count();
    assert!(
        meta <= TRACKED + 512,
        "clean nested tree should stat ~once per tracked file, got {meta} (tracked {TRACKED})"
    );
}

#[test]
fn nested_49k_hundred_edits_diff_metadata_probe_budget() {
    if std::env::var("GRIT_WORKTREE_SCAN_49K_NESTED")
        .ok()
        .as_deref()
        != Some("1")
    {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    build_fixture(root);
    touch_hundred_files(root);
    let repo = open_repo(root);
    let mut index = repo.load_index().expect("index");

    let _probe = reset_worktree_metadata_probe(root);
    let (diffs, _) = diff_index_to_worktree_with_options(
        repo.odb(),
        &mut index,
        root,
        DiffIndexToWorktreeOptions {
            refresh_index_stat_in_pass: false,
            ..DiffIndexToWorktreeOptions::default()
        },
    )
    .expect("diff");
    assert_eq!(diffs.len(), 100, "expected 100 modified paths");
    let meta = worktree_metadata_probe_count();
    assert!(
        meta <= TRACKED + 512,
        "diff after 100 edits must not double-stat the tree (metadata {meta}, tracked {TRACKED})"
    );
}

#[test]
fn nested_49k_noop_add_metadata_probe_budget() {
    if std::env::var("GRIT_WORKTREE_SCAN_49K_NESTED")
        .ok()
        .as_deref()
        != Some("1")
    {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    build_fixture(root);
    let repo = open_repo(root);

    let _probe = reset_worktree_metadata_probe(root);
    let outcome = stage(&repo, &StageOptions::default(), &mut NullProgress).expect("add");
    assert_eq!(outcome.total(), 0);
    let meta = worktree_metadata_probe_count();
    assert!(
        meta <= TRACKED + 512,
        "no-op add on nested tree should not double-stat (metadata {meta}, tracked {TRACKED})"
    );
}
