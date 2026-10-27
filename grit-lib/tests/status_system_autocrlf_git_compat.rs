//! `status()` with `core.autocrlf` set only in the system gitconfig (issue #2110).
//!
//! Git for Windows ships `core.autocrlf=true` in its system config, so a clean checkout has
//! CRLF working files. [`RepositoryOptions::empty`] does not read that config (no home, no
//! Git for Windows install root), while an environment that names the system config, as
//! [`RepositoryOptions::from_process`] does on a real machine, agrees with `git status`.

use std::time::Duration;

use filetime::FileTime;
use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::porcelain::status::{status, StatusOptions};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::Repository;
use grit_test_support::git_cmd as git_shell;

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

/// A clone checked out by `git` with only a system `core.autocrlf=true`, whose index stat data
/// no longer matches `f.txt`, so `status()` has to compare normalized content.
struct CrlfCheckout {
    _tmp: tempfile::TempDir,
    system_cfg: std::path::PathBuf,
    work_tree: std::path::PathBuf,
}

fn crlf_checkout() -> CrlfCheckout {
    let tmp = tempfile::tempdir().expect("tempdir");
    let system_cfg = tmp.path().join("system.gitconfig");
    std::fs::write(&system_cfg, "[core]\n\tautocrlf = true\n").expect("system config");

    let src = tmp.path().join("src");
    std::fs::create_dir(&src).expect("src dir");
    git_with_system_config(&src, &system_cfg, &["init", "-q", "-b", "main", "."]);
    git_with_system_config(&src, &system_cfg, &["config", "user.email", "t@e.com"]);
    git_with_system_config(&src, &system_cfg, &["config", "user.name", "T"]);
    std::fs::write(src.join("f.txt"), b"a\nb\n").expect("write lf");
    git_with_system_config(&src, &system_cfg, &["add", "f.txt"]);
    git_with_system_config(&src, &system_cfg, &["commit", "-qm", "base"]);
    git_with_system_config(tmp.path(), &system_cfg, &["clone", "-q", "src", "c"]);
    let work_tree = tmp.path().join("c");

    let porcelain = git_with_system_config(&work_tree, &system_cfg, &["status", "--porcelain"]);
    assert!(
        porcelain.trim().is_empty(),
        "git must see a clean checkout, got:\n{porcelain}"
    );

    let file = work_tree.join("f.txt");
    let bytes = std::fs::read(&file).expect("read worktree file");
    assert_eq!(bytes, b"a\r\nb\r\n", "checkout must smudge LF to CRLF");
    filetime::set_file_mtime(&file, FileTime::from_unix_time(1_900_000_000, 0)).expect("touch");
    std::thread::sleep(Duration::from_millis(50));

    CrlfCheckout {
        _tmp: tmp,
        system_cfg,
        work_tree,
    }
}

fn unstaged_paths(work_tree: &std::path::Path, options: &RepositoryOptions) -> Vec<String> {
    let repo = Repository::discover(Some(work_tree), options).expect("discover");
    let model = status(
        &repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("status");
    model
        .unstaged
        .iter()
        .map(|entry| entry.path().to_string())
        .collect()
}

/// The machine's own `/etc/gitconfig` still loads with an empty environment on Unix.
fn host_system_config_sets_autocrlf() -> bool {
    let path = grit_lib::config::system_config_path(&Environment::empty());
    std::fs::read_to_string(path).is_ok_and(|text| text.to_ascii_lowercase().contains("autocrlf"))
}

#[test]
fn status_is_clean_when_system_config_sets_autocrlf() {
    let checkout = crlf_checkout();
    let mut env = Environment::empty();
    env.git_config_system = Some(checkout.system_cfg.display().to_string());
    env.git_config_global = Some("/dev/null".into());

    let unstaged = unstaged_paths(
        &checkout.work_tree,
        &RepositoryOptions::with_environment(env),
    );
    assert!(
        unstaged.is_empty(),
        "status must agree with git when the system config sets autocrlf, got {unstaged:?}"
    );
}

#[test]
fn empty_options_do_not_read_the_system_autocrlf() {
    if host_system_config_sets_autocrlf() {
        return;
    }
    let checkout = crlf_checkout();
    let unstaged = unstaged_paths(&checkout.work_tree, &RepositoryOptions::empty());
    assert_eq!(
        unstaged,
        vec!["f.txt".to_string()],
        "RepositoryOptions::empty() reads no system config, so the CRLF file differs"
    );
}
