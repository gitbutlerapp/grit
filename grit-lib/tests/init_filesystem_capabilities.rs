//! Init-time filesystem capability config (issue #916 / #910).

use grit_lib::diff::{diff_index_to_worktree, mode_from_metadata};
use grit_lib::index::{Index, MODE_EXECUTABLE};
use grit_lib::init_filesystem::probe_trust_filemode;
use grit_lib::objects::ObjectKind;
use grit_lib::odb::Odb;
use grit_lib::repo::init_repository;
use std::fs;
use tempfile::TempDir;

#[test]
fn init_writes_probed_filemode() {
    let root = TempDir::new().expect("tempdir");
    init_repository(root.path(), false, "main", None, "files").expect("init");
    let git_dir = root.path().join(".git");
    let probed = probe_trust_filemode(&git_dir).expect("probe");
    let text = fs::read_to_string(git_dir.join("config")).expect("config");
    let expected = if probed {
        "filemode = true"
    } else {
        "filemode = false"
    };
    assert!(
        text.contains(expected),
        "config must match probe_trust_filemode ({probed}):\n{text}"
    );
}

#[test]
fn filemode_false_suppresses_executable_mode_only_diff() {
    let root = TempDir::new().expect("tempdir");
    let git_dir = root.path().join(".git");
    fs::create_dir_all(git_dir.join("objects")).expect("objects");
    fs::create_dir_all(git_dir.join("refs")).expect("refs");
    fs::write(
        git_dir.join("config"),
        "[core]\n\trepositoryformatversion = 0\n\tfilemode = false\n\tbare = false\n",
    )
    .expect("config");
    fs::write(root.path().join("run.sh"), "x\n").expect("file");

    let odb = Odb::new(&git_dir.join("objects"));
    let oid = odb.write(ObjectKind::Blob, b"x\n").expect("write blob");

    let meta = fs::symlink_metadata(root.path().join("run.sh")).expect("stat");
    let mut entry = grit_lib::index::entry_from_stat(
        &root.path().join("run.sh"),
        b"run.sh",
        oid,
        mode_from_metadata(&meta),
    )
    .expect("entry");
    entry.mode = MODE_EXECUTABLE;
    let mut index = Index::new();
    index.entries.push(entry);

    let diff = diff_index_to_worktree(&odb, &index, root.path(), false, true).expect("diff");
    assert!(
        diff.is_empty(),
        "mode-only executable mismatch must be ignored when core.filemode=false: {diff:?}"
    );
}

#[test]
fn probe_filemode_is_consistent_on_repeat() {
    let td = TempDir::new().expect("tempdir");
    let git_dir = td.path().join(".git");
    fs::create_dir_all(&git_dir).expect("mkdir");
    fs::write(git_dir.join("config"), "[core]\n").expect("config");
    let first = probe_trust_filemode(&git_dir).expect("probe");
    let second = probe_trust_filemode(&git_dir).expect("probe");
    assert_eq!(
        first, second,
        "probe must be deterministic for the same git directory"
    );
}

#[test]
fn init_local_filemode_follows_probe_not_global() {
    let global = TempDir::new().expect("global config dir");
    fs::write(global.path().join("config"), "[core]\n\tfilemode = false\n").expect("global config");

    let root = TempDir::new().expect("tempdir");
    std::env::set_var("GIT_CONFIG_GLOBAL", global.path().join("config"));
    std::env::set_var(
        "GIT_CONFIG_SYSTEM",
        if cfg!(windows) { "NUL" } else { "/dev/null" },
    );
    init_repository(root.path(), false, "main", None, "files").expect("init");
    std::env::remove_var("GIT_CONFIG_GLOBAL");
    std::env::remove_var("GIT_CONFIG_SYSTEM");

    let git_dir = root.path().join(".git");
    let probed = probe_trust_filemode(&git_dir).expect("probe");
    let text = fs::read_to_string(git_dir.join("config")).expect("config");
    let expected = if probed {
        "filemode = true"
    } else {
        "filemode = false"
    };
    assert!(
        text.contains(expected),
        "local init must write probed filemode even when global disables it:\n{text}"
    );
}

#[test]
fn separate_git_dir_honors_filemode_false_in_external_config() {
    use grit_lib::diff::{diff_index_to_worktree_with_options, DiffIndexToWorktreeOptions};
    use grit_lib::repo::init_repository_separate_git_dir;

    let work = TempDir::new().expect("worktree");
    let git_dir = TempDir::new().expect("git dir");
    init_repository_separate_git_dir(work.path(), git_dir.path(), "main", None, "files")
        .expect("init");

    fs::write(
        git_dir.path().join("config"),
        "[core]\n\trepositoryformatversion = 0\n\tfilemode = false\n\tbare = false\n",
    )
    .expect("config");
    fs::write(work.path().join("run.sh"), "x\n").expect("file");

    let odb = Odb::new(&git_dir.path().join("objects"));
    let oid = odb.write(ObjectKind::Blob, b"x\n").expect("write blob");

    let meta = fs::symlink_metadata(work.path().join("run.sh")).expect("stat");
    let mut entry = grit_lib::index::entry_from_stat(
        &work.path().join("run.sh"),
        b"run.sh",
        oid,
        mode_from_metadata(&meta),
    )
    .expect("entry");
    entry.mode = MODE_EXECUTABLE;
    let mut index = Index::new();
    index.entries.push(entry);

    let diff = diff_index_to_worktree_with_options(
        &odb,
        &index,
        work.path(),
        DiffIndexToWorktreeOptions {
            repository_git_dir: Some(git_dir.path().to_path_buf()),
            ignore_submodule_untracked: false,
            simplify_gitlinks: true,
            ..DiffIndexToWorktreeOptions::default()
        },
    )
    .expect("diff");
    assert!(
        diff.is_empty(),
        "separate git dir must load filemode from external config, not worktree/.git: {diff:?}"
    );
}
