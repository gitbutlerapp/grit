//! `grit rm` with inherited system `core.autocrlf` only (issue #1190).

use std::time::Duration;

use filetime::FileTime;
use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::porcelain::paths::{remove_paths, RemoveOptions};
use grit_lib::repo::Repository;
use grit_test_support::git_cmd as git_shell;

fn write_system_autocrlf_config(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("system.gitconfig");
    std::fs::write(&path, "[core]\n\tautocrlf = true\n").expect("system config");
    path
}

fn git_with_system_config(
    repo: &std::path::Path,
    system_cfg: &std::path::Path,
    args: &[&str],
) -> String {
    git_shell(args)
        .in_dir(repo)
        .env("GIT_CONFIG_SYSTEM", system_cfg)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .suc()
        .stdout
}

fn init_repo_inherited_autocrlf(root: &std::path::Path, system_cfg: &std::path::Path) {
    git_with_system_config(root, system_cfg, &["init", "-q", "-b", "main", "."]);
    git_with_system_config(root, system_cfg, &["config", "user.email", "t@e.com"]);
    git_with_system_config(root, system_cfg, &["config", "user.name", "T"]);
}

fn open_grit_inherited_autocrlf(
    root: &std::path::Path,
    system_cfg: &std::path::Path,
) -> Repository {
    let mut env = Environment::empty();
    env.git_config_system = Some(system_cfg.display().to_string());
    env.git_config_global = Some("/dev/null".into());
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::with_environment(env).with_work_tree(root),
    )
    .expect("open grit repo")
}

/// After `git status` has refreshed index stat info, bump mtime without changing bytes so
/// `remove_paths` must compare normalized content (issue #1190), not rely on a warm stat cache.
fn force_stat_mismatch_preserving_bytes(path: &std::path::Path) {
    let bytes = std::fs::read(path).expect("read worktree file");
    assert!(
        bytes.windows(2).any(|w| w == b"\r\n"),
        "checkout must smudge LF blob to CRLF on disk"
    );
    let bumped = FileTime::from_unix_time(1_900_000_000, 0);
    filetime::set_file_mtime(path, bumped).expect("touch mtime");
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        std::fs::read(path).expect("re-read after touch"),
        bytes,
        "stat mismatch must not alter file bytes"
    );
}

#[test]
fn remove_paths_succeeds_with_system_inherited_autocrlf_only() {
    let cfg_dir = tempfile::tempdir().expect("cfg tempdir");
    let system_cfg = write_system_autocrlf_config(cfg_dir.path());

    let root = tempfile::tempdir().expect("tempdir");
    let src = root.path().join("src");
    std::fs::create_dir(&src).expect("src dir");
    init_repo_inherited_autocrlf(&src, &system_cfg);
    std::fs::write(src.join("f.txt"), b"a\nb\n").expect("write lf");
    git_with_system_config(&src, &system_cfg, &["add", "f.txt"]);
    git_with_system_config(&src, &system_cfg, &["commit", "-qm", "base"]);

    let clone = root.path().join("c");
    git_with_system_config(root.path(), &system_cfg, &["clone", "-q", "src", "c"]);
    let dir = clone;

    let porcelain = git_with_system_config(&dir, &system_cfg, &["status", "--porcelain"]);
    assert!(
        porcelain.trim().is_empty(),
        "git clone checkout must leave clean tree with inherited system autocrlf, got:\n{porcelain}"
    );

    force_stat_mismatch_preserving_bytes(&dir.join("f.txt"));

    let repo = open_grit_inherited_autocrlf(&dir, &system_cfg);
    let outcome = remove_paths(
        &repo,
        &RemoveOptions {
            pathspecs: vec!["f.txt".into()],
            ..Default::default()
        },
    )
    .expect("remove must not treat CRLF smudge as local changes");
    assert_eq!(outcome.removed, vec!["f.txt".to_string()]);

    let porcelain = git_with_system_config(&dir, &system_cfg, &["status", "--porcelain"]);
    assert!(
        porcelain.contains("D  f.txt") || porcelain.contains("D f.txt"),
        "expected staged deletion of f.txt, got:\n{porcelain}"
    );
    assert!(
        !dir.join("f.txt").exists(),
        "worktree file should be removed"
    );
}
