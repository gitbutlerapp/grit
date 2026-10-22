//! `git log -p` patch output parity (Myers default and histogram).

mod support;

use std::io::Cursor;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::config::ConfigSet;
use grit_lib::diff::{
    write_commit_log_patch, write_commits_log_patch, DiffAlgorithm, DiffOptions,
    DiffPostprocessOptions, LogPatchCommitHeader, LogPatchOptions,
};
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_list::{rev_list, RevListOptions};
use support::{assert_raw_matches_git, git_raw_lines, tree_diff_raw};
use tempfile::TempDir;

fn git_in(dir: &std::path::Path, args: &[&str]) -> Vec<u8> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn init_corpus_repo(git: &std::path::Path, dir: &std::path::Path) {
    git_in(dir, &["init", "-q", "--initial-branch=main", "."]);
    std::fs::write(dir.join("README"), b"base\n").unwrap();
    git_in(dir, &["add", "README"]);
    git_in(dir, &["commit", "-q", "-m", "init"]);
    for i in 0..12 {
        std::fs::write(dir.join("README"), format!("line {i}\n")).unwrap();
        git_in(dir, &["add", "README"]);
        git_in(dir, &["commit", "-q", "-m", &format!("change {i}")]);
        if i % 4 == 0 {
            std::fs::write(dir.join(format!("extra-{i}.txt")), b"payload\n").unwrap();
            git_in(dir, &["add", "."]);
            git_in(dir, &["commit", "-q", "-m", &format!("add file {i}")]);
        }
    }
    std::fs::write(dir.join("bin.dat"), &[0u8, 1, 2, 3, 4]).unwrap();
    git_in(dir, &["add", "bin.dat"]);
    git_in(dir, &["commit", "-q", "-m", "binary"]);
}

fn grit_log_patch(repo: &Repository, limit: usize, diff: DiffOptions) -> Vec<u8> {
    let opts = RevListOptions {
        max_count: Some(limit),
        ..Default::default()
    };
    let result = rev_list(repo, &["HEAD".to_owned()], &[], &opts).expect("rev-list");
    let config = repo.config().map(|c| (*c).clone()).unwrap_or_default();
    let patch_opts = LogPatchOptions {
        diff,
        ..LogPatchOptions::git_default(&config)
    };
    let mut patch_opts = patch_opts;
    patch_opts.commit_header = LogPatchCommitHeader::FormatPercentH;
    let mut out = Cursor::new(Vec::new());
    repo.odb()
        .with_pack_read_context(|| {
            write_commits_log_patch(repo, &mut out, &result.commits, &patch_opts)
        })
        .expect("patch");
    out.into_inner()
}

fn git_git_corpus_path() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("GRIT_LOG_PATCH_GIT_GIT") {
        let path = std::path::PathBuf::from(p);
        if path.is_dir() {
            return Some(path);
        }
    }
    let default = std::path::PathBuf::from("/tmp/grit-bench-odb-cache/git.git");
    default.is_dir().then_some(default)
}

#[test]
fn log_patch_corpus_matches_git_default_algorithm() {
    let dir = TempDir::new().expect("tempdir");
    init_corpus_repo(std::path::Path::new("git"), dir.path());
    let git_out = git_in(
        dir.path(),
        &["log", "-p", "--no-color", "--format=%H", "-15"],
    );
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    let grit_out = grit_log_patch(&repo, 15, DiffOptions::default());
    assert_eq!(grit_out, git_out);
}

fn assert_last_log_patch_matches_git(dir: &std::path::Path) {
    let git_out = git_in(dir, &["log", "-p", "--no-color", "--format=%H", "-1"]);
    let repo = Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("open");
    let grit_out = grit_log_patch(&repo, 1, DiffOptions::default());
    assert_eq!(
        grit_out,
        git_out,
        "patch mismatch:\ngrit:\n{}\ngit:\n{}",
        String::from_utf8_lossy(&grit_out),
        String::from_utf8_lossy(&git_out)
    );
}

#[test]
fn log_patch_mode_only_matches_git() {
    let dir = TempDir::new().expect("tempdir");
    git_in(dir.path(), &["init", "-q", "--initial-branch=main", "."]);
    std::fs::write(dir.path().join("f"), b"x\n").unwrap();
    git_in(dir.path(), &["add", "f"]);
    git_in(dir.path(), &["commit", "-q", "-m", "init"]);
    std::fs::write(dir.path().join("f"), b"x\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path().join("f"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
    }
    git_in(dir.path(), &["add", "f"]);
    git_in(dir.path(), &["commit", "-q", "-m", "chmod"]);
    assert_last_log_patch_matches_git(dir.path());
}

#[test]
fn log_patch_empty_file_add_matches_git() {
    let dir = TempDir::new().expect("tempdir");
    git_in(dir.path(), &["init", "-q", "--initial-branch=main", "."]);
    git_in(dir.path(), &["commit", "--allow-empty", "-q", "-m", "init"]);
    std::fs::write(dir.path().join("empty"), b"").unwrap();
    git_in(dir.path(), &["add", "empty"]);
    git_in(dir.path(), &["commit", "-q", "-m", "add empty"]);
    assert_last_log_patch_matches_git(dir.path());
}

#[test]
fn log_patch_quoted_tab_path_matches_git() {
    let dir = TempDir::new().expect("tempdir");
    git_in(dir.path(), &["init", "-q", "--initial-branch=main", "."]);
    let name = format!("tab{}name.txt", '\t');
    std::fs::write(dir.path().join(&name), b"x\n").unwrap();
    git_in(dir.path(), &["add", "."]);
    git_in(dir.path(), &["commit", "-q", "-m", "tab path"]);
    assert_last_log_patch_matches_git(dir.path());
}

#[test]
fn log_patch_non_utf8_text_matches_git() {
    let dir = TempDir::new().expect("tempdir");
    git_in(dir.path(), &["init", "-q", "--initial-branch=main", "."]);
    std::fs::write(dir.path().join("f"), b"good\n").unwrap();
    git_in(dir.path(), &["add", "f"]);
    git_in(dir.path(), &["commit", "-q", "-m", "init"]);
    std::fs::write(dir.path().join("f"), b"bad\xff\n").unwrap();
    git_in(dir.path(), &["add", "f"]);
    git_in(dir.path(), &["commit", "-q", "-m", "bad bytes"]);
    assert_last_log_patch_matches_git(dir.path());
}

#[test]
fn log_patch_git_git_corpus_matches_git_default_and_histogram() {
    let Some(corpus) = git_git_corpus_path() else {
        eprintln!("SKIP: set GRIT_LOG_PATCH_GIT_GIT or clone git.git to /tmp/grit-bench-odb-cache/git.git");
        return;
    };
    for (limit, histogram) in [(80, false), (40, true)] {
        if histogram {
            git_in(corpus.as_path(), &["config", "diff.algorithm", "histogram"]);
        } else {
            let _ = Command::new("git")
                .current_dir(&corpus)
                .args(["config", "--unset", "diff.algorithm"])
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output();
        }
        let git_out = git_in(
            corpus.as_path(),
            &[
                "log",
                "-p",
                "--no-color",
                "--format=%H",
                &format!("-{limit}"),
            ],
        );
        let repo =
            Repository::discover(Some(&corpus), &RepositoryOptions::empty()).expect("open git.git");
        let mut diff =
            DiffOptions::from_config(&repo.config().map(|c| (*c).clone()).unwrap_or_default());
        if histogram {
            diff.algorithm = DiffAlgorithm::Histogram;
        }
        let grit_out = grit_log_patch(&repo, limit, diff);
        if grit_out != git_out {
            let pos = grit_out
                .iter()
                .zip(git_out.iter())
                .position(|(a, b)| a != b)
                .unwrap_or(git_out.len().min(git_out.len()));
            panic!(
                "git.git log-patch mismatch (histogram={histogram}, limit {limit}) at byte {pos}: grit[{}..] {:?} git[{}..] {:?}",
                pos,
                &grit_out.get(pos..pos.saturating_add(40)),
                pos,
                &git_out.get(pos..pos.saturating_add(40)),
            );
        }
    }
}

#[test]
#[test]
fn log_patch_git_git_diff_tree_no_renames_matches_git() {
    let Some(corpus) = git_git_corpus_path() else {
        eprintln!("SKIP: set GRIT_LOG_PATCH_GIT_GIT or clone git.git to /tmp/grit-bench-odb-cache/git.git");
        return;
    };
    let repo =
        Repository::discover(Some(&corpus), &RepositoryOptions::empty()).expect("open git.git");
    let parent_hex = git_in(corpus.as_path(), &["rev-parse", "HEAD~1^{tree}"]);
    let head_hex = git_in(corpus.as_path(), &["rev-parse", "HEAD^{tree}"]);
    let parent: ObjectId = String::from_utf8_lossy(&parent_hex)
        .trim()
        .parse()
        .expect("parent oid");
    let head: ObjectId = String::from_utf8_lossy(&head_hex)
        .trim()
        .parse()
        .expect("head oid");
    let config = ConfigSet::new();
    let opts = DiffPostprocessOptions {
        rename_threshold: grit_lib::diff::RENAME_DETECTION_DISABLED,
        ..Default::default()
    };
    let git_raw = git_raw_lines(
        corpus.as_path(),
        &[
            "diff-tree",
            "-r",
            "--no-renames",
            "--root",
            "--raw",
            &parent.to_hex(),
            &head.to_hex(),
        ],
    );
    let grit_raw = tree_diff_raw(&repo, Some(&parent), Some(&head), &config, &opts);
    assert_raw_matches_git(&grit_raw, &git_raw, "git.git HEAD tree diff (no renames)");
}

#[test]
fn log_patch_corpus_matches_git_histogram() {
    let dir = TempDir::new().expect("tempdir");
    init_corpus_repo(std::path::Path::new("git"), dir.path());
    git_in(dir.path(), &["config", "diff.algorithm", "histogram"]);
    let git_out = git_in(
        dir.path(),
        &["log", "-p", "--no-color", "--format=%H", "-10"],
    );
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    let mut diff = DiffOptions::default();
    diff.algorithm = DiffAlgorithm::Histogram;
    let grit_out = grit_log_patch(&repo, 10, diff);
    assert_eq!(grit_out, git_out);
}
