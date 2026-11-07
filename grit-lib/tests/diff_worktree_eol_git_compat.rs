//! Worktree↔index diffs normalize `eol=crlf` worktree bytes before line diffs (issue #990), and
//! a worktree whose size changed only by line endings that clean away is not modified (#2164).

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::config::ConfigSet;
use grit_lib::diff::{
    diff_index_to_worktree_with_options, read_worktree_side_bytes_for_diff,
    DiffIndexToWorktreeOptions, DiffStatus,
};
use grit_lib::porcelain::status::{StatusOptions, UntrackedMode};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_lib::worktree::worktree_rules::WorktreeRules;
use std::fs;
use std::process::Command;
use tempfile::TempDir;

fn init_git_repo(root: &TempDir) -> std::path::PathBuf {
    let repo = root.path().join("rp");
    fs::create_dir_all(&repo).expect("mkdir");
    assert!(Command::new("git")
        .args(["init", "-q"])
        .current_dir(&repo)
        .status()
        .expect("git init")
        .success());
    assert!(Command::new("git")
        .args(["config", "user.name", "a"])
        .current_dir(&repo)
        .status()
        .expect("user.name")
        .success());
    assert!(Command::new("git")
        .args(["config", "user.email", "a@b"])
        .current_dir(&repo)
        .status()
        .expect("user.email")
        .success());
    repo
}

#[test]
fn diff_index_to_worktree_eol_crlf_single_line_change() {
    let root = TempDir::new().expect("tempdir");
    let repo = init_git_repo(&root);
    fs::write(repo.join(".gitattributes"), "*.txt text eol=crlf\n").expect("attrs");
    fs::write(repo.join("f.txt"), "a\nb\nc\n").expect("blob lf");
    assert!(Command::new("git")
        .args(["add", "-A"])
        .current_dir(&repo)
        .status()
        .expect("add")
        .success());
    assert!(Command::new("git")
        .args(["commit", "-qm", "1"])
        .current_dir(&repo)
        .status()
        .expect("commit")
        .success());

    fs::remove_file(repo.join("f.txt")).expect("rm");
    assert!(Command::new("git")
        .args(["checkout", "--", "f.txt"])
        .current_dir(&repo)
        .status()
        .expect("checkout")
        .success());
    fs::write(repo.join("f.txt"), "a\r\nB\r\nc\r\n").expect("crlf edit");

    let git_stat = Command::new("git")
        .args(["diff", "--stat"])
        .current_dir(&repo)
        .output()
        .expect("git diff");
    assert!(git_stat.status.success());
    let stat = String::from_utf8_lossy(&git_stat.stdout);
    assert!(
        stat.contains("1 insertion") && stat.contains("1 deletion"),
        "git should report one line changed: {stat}"
    );

    let grit_repo = Repository::open(
        &repo.join(".git"),
        &RepositoryOptions::empty().with_work_tree(&repo),
    )
    .expect("open");
    let work_tree = grit_repo.work_tree().expect("wt");
    let mut index = grit_repo
        .index()
        .load(&grit_lib::index::IndexLoadOptions::default())
        .expect("index");
    let rules = WorktreeRules::from_repository(&grit_repo, &index).expect("rules");
    let cfg = ConfigSet::load(
        &Environment::empty(),
        Some(grit_repo.git_dir()),
        &crate::config::LoadConfigOptions::standard(
            &Environment::empty(),
            Some(grit_repo.git_dir()),
        ),
    )
    .expect("config");
    let entries = grit_repo
        .diff()
        .index_to_worktree(&mut index, &TreeDiffOptions::default())
        .expect("diff")
        .entries;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].status, DiffStatus::Modified);

    let old = grit_repo
        .odb()
        .read(&entries[0].old_oid)
        .expect("old blob")
        .data;
    let new = read_worktree_side_bytes_for_diff(
        grit_repo.odb(),
        work_tree,
        "f.txt",
        entries[0].new_mode.bits(),
        &cfg,
        index.get(b"f.txt", 0),
        Some(&rules),
    )
    .expect("new side");
    assert_eq!(old, b"a\nb\nc\n");
    assert_eq!(new, b"a\nB\nc\n");

    let old_lines = old.split(|&b| b == b'\n').collect::<Vec<_>>();
    let new_lines = new.split(|&b| b == b'\n').collect::<Vec<_>>();
    let mut changes = 0usize;
    for (o, n) in old_lines.iter().zip(new_lines.iter()) {
        if o != n {
            changes += 1;
        }
    }
    assert_eq!(changes, 1, "normalized sides differ on one line only");
}

fn git_ok(repo: &std::path::Path, args: &[&str]) -> std::process::Output {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// Commit `committed` under `attributes` with system git, then rewrite the worktree file as
/// `rewritten`, which differs in size but cleans to the same blob. `fillers` extra tracked
/// files push the status scan onto its parallel path.
fn eol_only_rewrite_repo(
    root: &TempDir,
    attributes: &str,
    committed: &[u8],
    rewritten: &[u8],
    fillers: usize,
) -> std::path::PathBuf {
    let repo = init_git_repo(root);
    fs::write(repo.join(".gitattributes"), attributes).expect("attrs");
    fs::write(repo.join("n.txt"), committed).expect("write");
    for i in 0..fillers {
        fs::write(repo.join(format!("fill{i:05}.dat")), format!("{i}\n")).expect("filler");
    }
    git_ok(&repo, &["add", "-A"]);
    git_ok(&repo, &["commit", "-qm", "c"]);
    fs::write(repo.join("n.txt"), rewritten).expect("rewrite");
    assert_ne!(
        committed.len(),
        rewritten.len(),
        "fixture must change the size"
    );
    assert!(
        Command::new("git")
            .args(["diff", "--quiet", "--exit-code"])
            .current_dir(&repo)
            .status()
            .expect("git diff")
            .success(),
        "system git sees no change"
    );
    repo
}

fn open(repo: &std::path::Path) -> Repository {
    Repository::open(
        &repo.join(".git"),
        &RepositoryOptions::empty().with_work_tree(repo),
    )
    .expect("open")
}

fn assert_eol_only_rewrite_is_clean(
    attributes: &str,
    committed: &[u8],
    rewritten: &[u8],
    fillers: usize,
    threads: usize,
) {
    let root = TempDir::new().expect("tempdir");
    let repo = eol_only_rewrite_repo(&root, attributes, committed, rewritten, fillers);
    let case = format!("{attributes:?} {committed:?} -> {rewritten:?} threads={threads}");

    let grit_repo = open(&repo);
    let mut index = grit_repo
        .index()
        .load(&grit_lib::index::IndexLoadOptions::default())
        .expect("index");
    let options = TreeDiffOptions::default()
        .with_for_status(true)
        .with_threads(Some(
            ThreadPolicy::with_threads_override(Some(threads)).min_items_override(Some(0)),
        ));
    let entries = grit_repo
        .diff()
        .index_to_worktree(&mut index, &options)
        .expect("diff")
        .entries;
    assert!(entries.is_empty(), "{case}: phantom change {entries:?}");

    let options = StatusOptions {
        untracked: UntrackedMode::No,
        stat_parallel_threads: Some(threads),
        ..StatusOptions::default()
    };
    for run in 0..2 {
        let model = grit_repo
            .status()
            .run(&options, &mut NullProgress, &CancelToken::never())
            .expect("status");
        assert!(model.is_clean(), "{case} run {run}: {:?}", model.unstaged);
    }

    // The index grit refreshed stays readable and clean for system git.
    git_ok(&repo, &["fsck", "--no-progress"]);
    let porcelain = git_ok(&repo, &["status", "--porcelain"]);
    assert!(
        porcelain.stdout.is_empty(),
        "{case}: git status {porcelain:?}"
    );
}

#[test]
fn eol_crlf_lf_rewrite_is_not_modified_issue_2164() {
    for (fillers, threads) in [
        (0, 1),
        (
            grit_lib::test_support::counters::PARALLEL_STAT_MIN_ENTRIES,
            4,
        ),
    ] {
        assert_eol_only_rewrite_is_clean(
            "*.txt text eol=crlf\n",
            b"one\r\ntwo\r\n",
            b"one\ntwo\n",
            fillers,
            threads,
        );
    }
}

#[test]
fn text_auto_line_ending_rewrite_is_not_modified_issue_2164() {
    for (fillers, threads) in [
        (0, 1),
        (
            grit_lib::test_support::counters::PARALLEL_STAT_MIN_ENTRIES,
            4,
        ),
    ] {
        assert_eol_only_rewrite_is_clean(
            "* text=auto\n",
            b"one\ntwo\n",
            b"one\r\ntwo\r\n",
            fillers,
            threads,
        );
        assert_eol_only_rewrite_is_clean(
            "* text=auto\n",
            b"one\r\ntwo\r\n",
            b"one\ntwo\n",
            fillers,
            threads,
        );
    }
}

#[test]
fn eol_crlf_real_size_change_is_still_modified() {
    let root = TempDir::new().expect("tempdir");
    let repo = init_git_repo(&root);
    fs::write(repo.join(".gitattributes"), "*.txt text eol=crlf\n").expect("attrs");
    fs::write(repo.join("n.txt"), "one\r\ntwo\r\n").expect("write");
    git_ok(&repo, &["add", "-A"]);
    git_ok(&repo, &["commit", "-qm", "c"]);
    fs::write(repo.join("n.txt"), "one\ntwo\nthree\n").expect("rewrite");

    let grit_repo = open(&repo);
    let model = grit_repo
        .status()
        .run(
            &StatusOptions {
                untracked: UntrackedMode::No,
                ..StatusOptions::default()
            },
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("status");
    let unstaged: Vec<_> = model.unstaged.iter().map(|e| e.path().to_owned()).collect();
    assert_eq!(unstaged, ["n.txt"]);
    let porcelain = git_ok(&repo, &["status", "--porcelain"]);
    assert_eq!(String::from_utf8_lossy(&porcelain.stdout), " M n.txt\n");
}
