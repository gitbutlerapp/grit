//! Git compatibility for collapsed ignored directories under untracked parents (issue #1515).

use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::status::{status, IgnoredMode, StatusOptions, UntrackedMode};
use grit_lib::progress::NullProgress;
use grit_lib::repo::{InitOptions, Repository};
use grit_test_support::git;

fn grit_bin() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("GRIT_BIN") {
        let path = std::path::PathBuf::from(p);
        if path.is_file() {
            return path;
        }
    }
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest.parent().expect("workspace root");
    let target_dir = std::env::var("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| workspace.join("target"));
    for subpath in ["debug/grit", "release/grit"] {
        let candidate = target_dir.join(subpath);
        if candidate.is_file() {
            return candidate;
        }
    }
    panic!("grit binary not found; build with cargo build -p grit-cli");
}

fn git_untracked_and_ignored(root: &Path, untracked_files: &str) -> (Vec<String>, Vec<String>) {
    let out = git(
        root,
        &[
            "status",
            "--porcelain=v1",
            "--ignored",
            &format!("--untracked-files={untracked_files}"),
        ],
    );
    let mut untracked = Vec::new();
    let mut ignored = Vec::new();
    for line in out.lines() {
        if let Some(path) = line.strip_prefix("?? ") {
            untracked.push(path.to_owned());
        } else if let Some(path) = line.strip_prefix("!! ") {
            ignored.push(path.to_owned());
        }
    }
    untracked.sort();
    ignored.sort();
    (untracked, ignored)
}

fn grit_untracked_and_ignored(
    repo: &Repository,
    untracked: UntrackedMode,
) -> (Vec<String>, Vec<String>) {
    let opts = StatusOptions {
        untracked,
        ignored: IgnoredMode::Traditional,
        ahead_behind: false,
        ..StatusOptions::default()
    };
    let model = status(repo, &opts, &mut NullProgress).expect("status");
    let mut untracked_paths = model.untracked;
    let mut ignored_paths = model.ignored;
    untracked_paths.sort();
    ignored_paths.sort();
    (untracked_paths, ignored_paths)
}

/// Layout from issue #1515: ignored dirs nested under untracked dirs must collapse like Git.
#[test]
fn ignored_dirs_under_untracked_parent_collapse_like_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    Repository::init(
        root,
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_bare(false),
    )
    .expect("init");

    fs::create_dir_all(root.join("sub/tmpdir")).expect("mkdir");
    fs::create_dir_all(root.join("cdir/skipme")).expect("mkdir");
    fs::write(root.join("sub/.gitignore"), "tmpdir/\n").expect("write");
    fs::write(root.join(".gitignore"), "skipme/\n").expect("write");
    fs::write(root.join("sub/tmpdir/f1"), b"x\n").expect("write");
    fs::write(root.join("sub/keep.txt"), b"y\n").expect("write");
    fs::write(root.join("cdir/skipme/f"), b"z\n").expect("write");
    fs::write(root.join("cdir/other"), b"z\n").expect("write");

    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);

    let grit_repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open grit on same tree");

    let (git_u, git_i) = git_untracked_and_ignored(root, "normal");
    let (grit_u, grit_i) = grit_untracked_and_ignored(&grit_repo, UntrackedMode::Normal);

    assert_eq!(
        grit_u, git_u,
        "untracked paths (normal) must match git, git={git_u:?} grit={grit_u:?}"
    );
    assert_eq!(
        grit_i, git_i,
        "ignored paths (normal) must match git, git={git_i:?} grit={grit_i:?}"
    );

    assert!(
        grit_i.iter().all(|p| p.ends_with('/')),
        "ignored dirs must collapse to trailing slash, got {grit_i:?}"
    );
    assert!(
        !grit_i
            .iter()
            .any(|p| p.contains("skipme/f") || p.contains("tmpdir/f1")),
        "must not list files inside ignored dirs in normal mode, got {grit_i:?}"
    );

    let (git_u_all, git_i_all) = git_untracked_and_ignored(root, "all");
    let (grit_u_all, grit_i_all) = grit_untracked_and_ignored(&grit_repo, UntrackedMode::All);

    assert_eq!(grit_u_all, git_u_all, "untracked (all) vs git");
    assert_eq!(grit_i_all, git_i_all, "ignored (all) vs git");
}

/// Smoke: grit CLI short output lists collapsed ignored dirs (not every file).
#[test]
fn grit_cli_status_ignored_short_collapses_nested_dirs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    Repository::init(
        root,
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_bare(false),
    )
    .expect("init");

    fs::create_dir_all(root.join("sub/tmpdir")).expect("mkdir");
    fs::write(root.join("sub/.gitignore"), "tmpdir/\n").expect("write");
    fs::write(root.join("sub/tmpdir/f1"), b"x\n").expect("write");
    fs::write(root.join("sub/keep.txt"), b"y\n").expect("write");

    let grit_bin = grit_bin();
    let out = Command::new(&grit_bin)
        .current_dir(root)
        .args(["status", "--ignored", "--short"])
        .output()
        .expect("grit status");
    assert!(
        out.status.success(),
        "grit status failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("!! sub/tmpdir/"),
        "expected collapsed ignored dir in short output, got:\n{stdout}"
    );
    assert!(
        !stdout.contains("tmpdir/f1"),
        "must not enumerate ignored files in normal mode, got:\n{stdout}"
    );
}
