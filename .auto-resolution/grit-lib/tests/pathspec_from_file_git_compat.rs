//! `--pathspec-from-file` parsing matches system `git` for NUL literals and empty records.

use std::fs;
use std::process::Command;

fn null_config() -> String {
    if cfg!(windows) {
        "NUL".to_string()
    } else {
        "/dev/null".to_string()
    }
}

fn init_repo(dir: &std::path::Path) {
    let null = null_config();
    assert!(Command::new("git")
        .current_dir(dir)
        .args(["init"])
        .env("GIT_CONFIG_GLOBAL", &null)
        .env("GIT_CONFIG_SYSTEM", &null)
        .status()
        .expect("git init")
        .success());
}

#[test]
fn nul_mode_literal_spaces_and_quotes_match_git_add() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    fs::write(dir.path().join("  spaced  "), b"x\n").expect("write");
    let quoted = dir.path().join("\"double\"");
    fs::create_dir_all(&quoted).expect("mkdir");
    fs::write(quoted.join("f"), b"y\n").expect("write");
    fs::write(dir.path().join("other"), b"z\n").expect("write");

    let list = dir.path().join("list.bin");
    fs::write(&list, b"  spaced  \0\"double\"\0").expect("write list");

    let null = null_config();
    let git_out = Command::new("git")
        .current_dir(dir.path())
        .args([
            "add",
            "--pathspec-from-file",
            list.to_str().unwrap(),
            "--pathspec-file-nul",
        ])
        .env("GIT_CONFIG_GLOBAL", &null)
        .env("GIT_CONFIG_SYSTEM", &null)
        .output()
        .expect("git add");
    assert!(
        git_out.status.success(),
        "git add failed: {}",
        String::from_utf8_lossy(&git_out.stderr)
    );

    use grit_lib::porcelain::add::{stage, StageMode, StageOptions};
    use grit_lib::progress::NullProgress;
    use grit_lib::repo::{InitOptions, Repository};
    use grit_lib::worktree::pathspec::{PathspecInput, ResolvedPathspecs};

    let dir2 = tempfile::tempdir().expect("tempdir2");
    let repo = Repository::init(dir2.path(), &InitOptions::default()).expect("init");
    fs::write(dir2.path().join("  spaced  "), b"x\n").expect("write");
    let quoted2 = dir2.path().join("\"double\"");
    fs::create_dir_all(&quoted2).expect("mkdir");
    fs::write(quoted2.join("f"), b"y\n").expect("write");
    fs::write(dir2.path().join("other"), b"z\n").expect("write");

    let data = fs::read(&list).expect("read list");
    let resolved = ResolvedPathspecs::resolve(
        &repo,
        dir2.path(),
        &[],
        Some(PathspecInput::new(data, true)),
    )
    .expect("resolve");
    let outcome = stage(
        &repo,
        &StageOptions {
            pathspecs: resolved.specs,
            pathspec_sources: resolved.sources,
            mode: StageMode::All,
            ..Default::default()
        },
        &mut NullProgress,
    )
    .expect("stage");

    assert!(outcome.total() >= 2, "expected staged paths from NUL list");
}

#[test]
fn empty_pathspec_record_matches_git_rejection() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    fs::write(dir.path().join("a"), b"1\n").expect("write");
    fs::write(dir.path().join("list"), b"\n").expect("blank line");

    let null = null_config();
    let git_out = Command::new("git")
        .current_dir(dir.path())
        .args(["add", "--pathspec-from-file", "list"])
        .env("GIT_CONFIG_GLOBAL", &null)
        .env("GIT_CONFIG_SYSTEM", &null)
        .output()
        .expect("git add");
    assert!(!git_out.status.success());
    assert!(
        String::from_utf8_lossy(&git_out.stderr).contains("empty string"),
        "git stderr: {}",
        String::from_utf8_lossy(&git_out.stderr)
    );

    use grit_lib::config::environment::RepositoryOptions;
    use grit_lib::repo::Repository;
    use grit_lib::worktree::pathspec::{PathspecInput, PathspecResolveError, ResolvedPathspecs};

    let git_dir = dir.path().join(".git");
    let repo = Repository::open(
        &git_dir,
        &RepositoryOptions::empty().with_work_tree(dir.path()),
    )
    .expect("open");
    let err = ResolvedPathspecs::resolve(
        &repo,
        dir.path(),
        &[],
        Some(PathspecInput::new(b"\n".to_vec(), false)),
    )
    .expect_err("empty line");
    assert_eq!(err, PathspecResolveError::EmptyPathspec);
}
