//! Stash apply/pop with `core.autocrlf=true` and CRLF worktree bytes (issue #958).
//! Issue #1111: inherited system `core.autocrlf` without a repo-local override.
//! Issue #1305: apply/pop must smudge restored bytes (tracked and `-u` untracked).

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::repo::Repository;
use grit_lib::stash::{StashApplyOptions, StashPushOptions};
use grit_test_support::{git, git_cmd as git_shell};

fn git_cmd(repo: &std::path::Path, args: &[&str]) -> String {
    git(repo, args)
}

fn init_repo(root: &std::path::Path) {
    git_cmd(root, &["init", "-q", "-b", "main", "."]);
    git_cmd(root, &["config", "user.email", "t@e.com"]);
    git_cmd(root, &["config", "user.name", "T"]);
    git_cmd(root, &["config", "core.autocrlf", "true"]);
}

fn stash_options() -> StashPushOptions {
    StashPushOptions {
        message: None,
        include_untracked: false,
        identity: "T <t@e.com> 1000000000 +0000".to_owned(),
        selection: None,
        pathspecs: Vec::new(),
        dry_run: false,
        mode: grit_lib::porcelain::stash::StashMode::default(),
    }
}

fn stash_options_untracked() -> StashPushOptions {
    StashPushOptions {
        message: None,
        include_untracked: true,
        identity: "T <t@e.com> 1000000000 +0000".to_owned(),
        selection: None,
        pathspecs: Vec::new(),
        dry_run: false,
        mode: grit_lib::porcelain::stash::StashMode::default(),
    }
}

fn assert_worktree_bytes(path: &std::path::Path, expected: &[u8]) {
    let got = std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    assert_eq!(
        got,
        expected,
        "worktree bytes for {}",
        path.file_name().unwrap().to_string_lossy()
    );
}

fn open_grit(root: &std::path::Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open grit repo")
}

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

fn ensure_crlf_worktree_lf_index_with_system(root: &std::path::Path, system_cfg: &std::path::Path) {
    let path = root.join("a.txt");
    let crlf = b"one\r\ntwo\r\nthree\r\n";
    std::fs::write(&path, crlf).expect("crlf worktree");
    let porcelain = git_with_system_config(root, system_cfg, &["status", "--porcelain"]);
    assert!(
        porcelain.trim().is_empty(),
        "git must treat CRLF worktree as clean with inherited system autocrlf, got:\n{porcelain}"
    );
}

/// After stash reset, simulate Git for Windows: LF blob in the index, CRLF on disk, clean status.
fn ensure_crlf_worktree_lf_index(root: &std::path::Path) {
    let path = root.join("a.txt");
    let crlf = b"one\r\ntwo\r\nthree\r\n";
    std::fs::write(&path, crlf).expect("crlf worktree");
    let porcelain = git_cmd(root, &["status", "--porcelain"]);
    assert!(
        porcelain.trim().is_empty(),
        "git must treat CRLF worktree as clean with autocrlf and LF index, got:\n{porcelain}"
    );
}

/// After stash push reset, HEAD version of `f.txt` should be CRLF on disk and clean in status.
fn ensure_stash_reset_f_txt_crlf_clean(root: &std::path::Path) {
    assert_worktree_bytes(&root.join("f.txt"), b"a\r\nb\r\n");
    let porcelain = git_cmd(root, &["status", "--porcelain"]);
    assert!(
        porcelain.trim().is_empty(),
        "worktree must be clean after stash push with autocrlf, got:\n{porcelain}"
    );
}

#[test]
fn apply_stash_succeeds_on_clean_tree_with_autocrlf() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("a.txt"), b"one\ntwo\nthree\n").expect("write");
    git_cmd(dir.path(), &["add", "a.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);

    std::fs::write(dir.path().join("a.txt"), b"one\ntwo\nthree\nfour\n").expect("modify");
    let repo = open_grit(dir.path());
    repo.stash()
        .push(&stash_options())
        .expect("push")
        .expect("had changes");

    ensure_crlf_worktree_lf_index(dir.path());

    let stash_oid = grit_lib::refs::resolve_ref(repo.git_dir(), "refs/stash").expect("stash ref");

    let grit_dir = tempfile::tempdir().expect("grit copy");
    std::process::Command::new("cp")
        .args([
            "-a",
            &format!("{}/.", dir.path().display()),
            &grit_dir.path().to_string_lossy(),
        ])
        .status()
        .expect("cp")
        .success()
        .then_some(())
        .expect("cp failed");
    let grit_repo = open_grit(grit_dir.path());
    grit_repo
        .stash()
        .apply_commit(&stash_oid, &StashApplyOptions::default())
        .expect("apply must not treat CRLF smudge as local changes");

    let git_dir = tempfile::tempdir().expect("git copy");
    std::process::Command::new("cp")
        .args([
            "-a",
            &format!("{}/.", dir.path().display()),
            &git_dir.path().to_string_lossy(),
        ])
        .status()
        .expect("cp")
        .success()
        .then_some(())
        .expect("cp failed");
    git_cmd(git_dir.path(), &["stash", "apply", "stash@{0}"]);

    let grit_porcelain = git_cmd(grit_dir.path(), &["status", "--porcelain"]);
    let git_porcelain = git_cmd(git_dir.path(), &["status", "--porcelain"]);
    assert_eq!(
        grit_porcelain, git_porcelain,
        "porcelain after apply must match git stash apply"
    );
    assert!(
        grit_porcelain.contains("a.txt"),
        "expected modified a.txt in porcelain, got:\n{grit_porcelain}"
    );
}

#[test]
fn pop_stash_succeeds_on_clean_tree_with_autocrlf() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("a.txt"), b"one\ntwo\nthree\n").expect("write");
    git_cmd(dir.path(), &["add", "a.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);

    std::fs::write(dir.path().join("a.txt"), b"one\ntwo\nthree\nfour\n").expect("modify");
    let repo = open_grit(dir.path());
    repo.stash()
        .push(&stash_options())
        .expect("push")
        .expect("had changes");

    ensure_crlf_worktree_lf_index(dir.path());

    repo.stash()
        .pop(0, &StashApplyOptions::default())
        .expect("pop");

    let list = git_cmd(dir.path(), &["stash", "list"]);
    assert!(
        list.trim().is_empty(),
        "pop must drop stash entry, got:\n{list}"
    );
    let porcelain = git_cmd(dir.path(), &["status", "--porcelain"]);
    assert!(
        porcelain.contains("a.txt"),
        "pop must restore stashed change, got:\n{porcelain}"
    );
}

#[test]
fn apply_stash_succeeds_with_system_inherited_autocrlf_only() {
    let cfg_dir = tempfile::tempdir().expect("cfg tempdir");
    let system_cfg = write_system_autocrlf_config(cfg_dir.path());

    let dir = tempfile::tempdir().expect("tempdir");
    init_repo_inherited_autocrlf(dir.path(), &system_cfg);
    std::fs::write(dir.path().join("a.txt"), b"one\ntwo\nthree\n").expect("write");
    git_with_system_config(dir.path(), &system_cfg, &["add", "a.txt"]);
    git_with_system_config(dir.path(), &system_cfg, &["commit", "-qm", "init"]);

    std::fs::write(dir.path().join("a.txt"), b"one\ntwo\nthree\nfour\n").expect("modify");
    let repo = open_grit_inherited_autocrlf(dir.path(), &system_cfg);
    repo.stash()
        .push(&stash_options())
        .expect("push")
        .expect("had changes");

    ensure_crlf_worktree_lf_index_with_system(dir.path(), &system_cfg);

    let stash_oid = grit_lib::refs::resolve_ref(repo.git_dir(), "refs/stash").expect("stash ref");
    repo.stash()
        .apply_commit(&stash_oid, &StashApplyOptions::default())
        .expect("apply must honor inherited system autocrlf when comparing worktree to index");

    let porcelain = git_with_system_config(dir.path(), &system_cfg, &["status", "--porcelain"]);
    assert!(
        porcelain.contains("a.txt"),
        "expected modified a.txt after apply, got:\n{porcelain}"
    );
}

#[test]
fn pop_stash_succeeds_with_system_inherited_autocrlf_only() {
    let cfg_dir = tempfile::tempdir().expect("cfg tempdir");
    let system_cfg = write_system_autocrlf_config(cfg_dir.path());

    let dir = tempfile::tempdir().expect("tempdir");
    init_repo_inherited_autocrlf(dir.path(), &system_cfg);
    std::fs::write(dir.path().join("a.txt"), b"one\ntwo\nthree\n").expect("write");
    git_with_system_config(dir.path(), &system_cfg, &["add", "a.txt"]);
    git_with_system_config(dir.path(), &system_cfg, &["commit", "-qm", "init"]);

    std::fs::write(dir.path().join("a.txt"), b"one\ntwo\nthree\nfour\n").expect("modify");
    let repo = open_grit_inherited_autocrlf(dir.path(), &system_cfg);
    repo.stash()
        .push(&stash_options())
        .expect("push")
        .expect("had changes");

    ensure_crlf_worktree_lf_index_with_system(dir.path(), &system_cfg);

    repo.stash()
        .pop(0, &StashApplyOptions::default())
        .expect("pop must honor inherited system autocrlf when comparing worktree to index");

    let list = git_with_system_config(dir.path(), &system_cfg, &["stash", "list"]);
    assert!(
        list.trim().is_empty(),
        "pop must drop stash entry, got:\n{list}"
    );
}

#[test]
fn pop_stash_writes_crlf_worktree_bytes_for_tracked_and_untracked() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("f.txt"), b"a\r\nb\r\n").expect("write");
    git_cmd(dir.path(), &["add", "f.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "base"]);

    std::fs::write(dir.path().join("f.txt"), b"a\r\nb\r\nc\r\n").expect("modify tracked");
    std::fs::write(dir.path().join("u.txt"), b"x\r\ny\r\n").expect("untracked");

    let repo = open_grit(dir.path());
    repo.stash()
        .push(&stash_options_untracked())
        .expect("push")
        .expect("had changes");

    ensure_stash_reset_f_txt_crlf_clean(dir.path());

    repo.stash()
        .pop(0, &StashApplyOptions::default())
        .expect("pop");

    assert_worktree_bytes(&dir.path().join("f.txt"), b"a\r\nb\r\nc\r\n");
    assert_worktree_bytes(&dir.path().join("u.txt"), b"x\r\ny\r\n");
    let eol = git_cmd(dir.path(), &["ls-files", "--eol", "f.txt"]);
    assert!(
        eol.contains("i/lf") && eol.contains("w/crlf"),
        "git ls-files --eol must show LF index and CRLF worktree, got:\n{eol}"
    );
}

#[test]
fn apply_stash_writes_crlf_worktree_bytes_matching_git_stash_apply() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("f.txt"), b"a\r\nb\r\n").expect("write");
    git_cmd(dir.path(), &["add", "f.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "base"]);

    std::fs::write(dir.path().join("f.txt"), b"a\r\nb\r\nc\r\n").expect("modify tracked");
    std::fs::write(dir.path().join("u.txt"), b"x\r\ny\r\n").expect("untracked");

    let repo = open_grit(dir.path());
    repo.stash()
        .push(&stash_options_untracked())
        .expect("push")
        .expect("had changes");

    ensure_stash_reset_f_txt_crlf_clean(dir.path());

    let stash_oid = grit_lib::refs::resolve_ref(repo.git_dir(), "refs/stash").expect("stash ref");

    let grit_dir = tempfile::tempdir().expect("grit copy");
    std::process::Command::new("cp")
        .args([
            "-a",
            &format!("{}/.", dir.path().display()),
            &grit_dir.path().to_string_lossy(),
        ])
        .status()
        .expect("cp")
        .success()
        .then_some(())
        .expect("cp failed");
    let grit_repo = open_grit(grit_dir.path());
    grit_repo
        .stash()
        .apply_commit(&stash_oid, &StashApplyOptions::default())
        .expect("apply");

    let git_dir = tempfile::tempdir().expect("git copy");
    std::process::Command::new("cp")
        .args([
            "-a",
            &format!("{}/.", dir.path().display()),
            &git_dir.path().to_string_lossy(),
        ])
        .status()
        .expect("cp")
        .success()
        .then_some(())
        .expect("cp failed");
    git_cmd(git_dir.path(), &["stash", "apply", "stash@{0}"]);

    for name in ["f.txt", "u.txt"] {
        let grit_bytes = std::fs::read(grit_dir.path().join(name)).expect("read grit");
        let git_bytes = std::fs::read(git_dir.path().join(name)).expect("read git");
        assert_eq!(
            grit_bytes, git_bytes,
            "{name} bytes must match git stash apply"
        );
    }
    let grit_eol = git_cmd(grit_dir.path(), &["ls-files", "--eol", "f.txt"]);
    let git_eol = git_cmd(git_dir.path(), &["ls-files", "--eol", "f.txt"]);
    assert_eq!(grit_eol, git_eol, "ls-files --eol for f.txt must match git");
}
