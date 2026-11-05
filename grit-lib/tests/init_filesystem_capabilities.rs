//! Init-time filesystem capability config (issue #916 / #910).

use grit_lib::config::init_filesystem::FilesystemCapabilities;
use grit_lib::diff::{diff_index_to_worktree, mode_from_metadata};
use grit_lib::index::{Index, MODE_EXECUTABLE};
use grit_lib::objects::odb::Odb;
use grit_lib::objects::ObjectKind;
use grit_lib::repo::{InitOptions, Repository};
use std::fs;
use std::process::Command;
use tempfile::TempDir;

fn config_bool(text: &str, key: &str) -> Option<bool> {
    let needle = format!("{key} = ");
    text.lines()
        .find(|line| line.trim().starts_with(&needle))
        .map(|line| {
            let v = line.split('=').nth(1).unwrap_or("").trim();
            matches!(v, "true" | "yes" | "on" | "1")
        })
}

fn git_config_bool(repo: &TempDir, key: &str) -> bool {
    let out = Command::new("git")
        .args(["-C", repo.path().to_str().expect("utf8"), "config", key])
        .output()
        .expect("git config");
    assert!(
        out.status.success(),
        "git config {key}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    matches!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "true" | "yes" | "on" | "1"
    )
}

#[test]
fn init_writes_detected_filemode() {
    let root = TempDir::new().expect("tempdir");
    Repository::init(
        root.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
    )
    .expect("init");
    let git_dir = root.path().join(".git");
    let detected = FilesystemCapabilities::detect_trust_filemode(&git_dir).expect("detect");
    let text = fs::read_to_string(git_dir.join("config")).expect("config");
    assert_eq!(
        config_bool(&text, "filemode"),
        Some(detected),
        "config must match detect_trust_filemode ({detected}):\n{text}"
    );
    assert_eq!(git_config_bool(&root, "core.filemode"), detected);
}

#[test]
fn init_config_matches_full_capability_detection() {
    let root = TempDir::new().expect("tempdir");
    Repository::init(
        root.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
    )
    .expect("init");
    let git_dir = root.path().join(".git");
    let caps = FilesystemCapabilities::detect(&git_dir).expect("detect");
    let text = fs::read_to_string(git_dir.join("config")).expect("config");

    assert_eq!(config_bool(&text, "filemode"), Some(caps.trust_filemode));
    assert_eq!(git_config_bool(&root, "core.filemode"), caps.trust_filemode);

    if text.contains("symlinks = false") {
        assert!(!caps.symlinks);
        assert!(!git_config_bool(&root, "core.symlinks"));
    }

    if text.contains("ignorecase = true") {
        assert!(caps.case_insensitive);
        assert!(git_config_bool(&root, "core.ignorecase"));
    }

    if text.contains("precomposeunicode = true") {
        assert!(caps.precomposes_unicode);
        assert!(git_config_bool(&root, "core.precomposeunicode"));
    }
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
    index.entries_mut().push(entry);

    let diff = diff_index_to_worktree(&odb, &mut index, root.path(), false, true).expect("diff");
    assert!(
        diff.is_empty(),
        "mode-only executable mismatch must be ignored when core.filemode=false: {diff:?}"
    );
}

#[test]
fn trust_filemode_detection_is_consistent_on_repeat() {
    let td = TempDir::new().expect("tempdir");
    let git_dir = td.path().join(".git");
    fs::create_dir_all(&git_dir).expect("mkdir");
    fs::write(git_dir.join("config"), "[core]\n").expect("config");
    let first = FilesystemCapabilities::detect_trust_filemode(&git_dir).expect("detect");
    let second = FilesystemCapabilities::detect_trust_filemode(&git_dir).expect("detect");
    assert_eq!(
        first, second,
        "detection must be deterministic for the same git directory"
    );
}

#[test]
fn init_local_filemode_follows_detection_not_global() {
    let global = TempDir::new().expect("global config dir");
    fs::write(global.path().join("config"), "[core]\n\tfilemode = false\n").expect("global config");

    let root = TempDir::new().expect("tempdir");
    std::env::set_var("GIT_CONFIG_GLOBAL", global.path().join("config"));
    std::env::set_var(
        "GIT_CONFIG_SYSTEM",
        if cfg!(windows) { "NUL" } else { "/dev/null" },
    );
    Repository::init(
        root.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
    )
    .expect("init");
    std::env::remove_var("GIT_CONFIG_GLOBAL");
    std::env::remove_var("GIT_CONFIG_SYSTEM");

    let git_dir = root.path().join(".git");
    let detected = FilesystemCapabilities::detect_trust_filemode(&git_dir).expect("detect");
    let text = fs::read_to_string(git_dir.join("config")).expect("config");
    assert_eq!(
        config_bool(&text, "filemode"),
        Some(detected),
        "local init must write detected filemode even when global disables it:\n{text}"
    );
}

#[test]
fn separate_git_dir_honors_filemode_false_in_external_config() {
    use grit_lib::diff::{diff_index_to_worktree_with_options, DiffIndexToWorktreeOptions};
    use grit_lib::repo::{InitOptions, Repository};

    let work = TempDir::new().expect("worktree");
    let git_dir = TempDir::new().expect("git dir");
    Repository::init(
        work.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files)
            .with_separate_git_dir(Some(git_dir.path().to_path_buf())),
    )
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
    index.entries_mut().push(entry);

    let (diff, _) = diff_index_to_worktree_with_options(
        &odb,
        &mut index,
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
