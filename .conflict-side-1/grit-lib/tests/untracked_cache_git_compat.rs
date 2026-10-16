//! Untracked cache (UNTR index extension) compatibility with system `git` (t7063).

mod support;

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::config::ConfigSet;
use grit_lib::index::untracked_cache::{
    collect_untracked_from_cache, dir_flags_from_config, parse_untracked_extension,
    refresh_untracked_cache_for_status, untracked_cache_ident, UntrackedCache,
    UntrackedIgnoredMode, DIR_HIDE_EMPTY_DIRECTORIES, DIR_SHOW_OTHER_DIRECTORIES,
};
use grit_lib::porcelain::status::{status, StatusOptions, UntrackedMode};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;

use support::{
    append_git_config, entry_from_worktree_path, extension_payload, fsck_strict, git,
    git_porcelain_untracked, git_status_porcelain_normal_with_trace,
    git_update_index_untracked_cache, read_index_bytes, trace2_read_directory_counter,
};

fn init_repo(root: &Path) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "core.untrackedCache", "true"]);
}

fn grit_untracked_paths(repo: &Repository) -> Vec<String> {
    let opts = StatusOptions {
        untracked: UntrackedMode::All,
        ..StatusOptions::default()
    };
    let model = status(repo, &opts, &mut NullProgress).expect("status");
    model.untracked
}

#[test]
fn t7063_untracked_cache_reused_by_git_status() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    fs::create_dir_all(root.join("dir")).expect("mkdir");
    fs::write(root.join("dir/tracked.txt"), "t\n").expect("write");
    fs::write(root.join("dir/untracked.txt"), "u\n").expect("write");
    git(root, &["add", "dir/tracked.txt"]);
    git(root, &["commit", "-qm", "c"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let mut idx = repo.index_handle().load().expect("load");
    let cfg = repo.config().expect("config");
    let wt = repo.work_tree().expect("work tree");
    let mut uc = UntrackedCache::new_shell(
        DIR_SHOW_OTHER_DIRECTORIES | DIR_HIDE_EMPTY_DIRECTORIES,
        untracked_cache_ident(wt),
    );
    refresh_untracked_cache_for_status(
        &repo,
        &idx,
        wt,
        cfg.as_ref(),
        &mut uc,
        false,
        UntrackedIgnoredMode::No,
    )
    .expect("refresh cache");
    idx.untracked_cache = Some(uc);
    repo.index_handle().write(&mut idx).expect("persist UNTR");

    let before = extension_payload(&read_index_bytes(&repo), b"UNTR").expect("UNTR");
    assert!(!before.is_empty(), "grit must write UNTR extension payload");

    let trace_warm = tempfile::NamedTempFile::new().expect("trace warm");
    git_status_porcelain_normal_with_trace(root, trace_warm.path());

    let stable = extension_payload(&read_index_bytes(&repo), b"UNTR").expect("UNTR after warm");
    assert!(
        !stable.is_empty(),
        "git must accept grit-written UNTR in normal untracked mode"
    );

    let trace_reuse = tempfile::NamedTempFile::new().expect("trace reuse");
    git_status_porcelain_normal_with_trace(root, trace_reuse.path());

    let after_reuse =
        extension_payload(&read_index_bytes(&repo), b"UNTR").expect("UNTR after second status");
    assert_eq!(
        stable, after_reuse,
        "second git status must reuse the UNTR payload without rebuilding"
    );
    assert_eq!(
        trace2_read_directory_counter(trace_reuse.path(), "node-creation"),
        Some(0),
        "reused UNTR must not allocate new directory nodes on the second status"
    );
    assert_eq!(
        trace2_read_directory_counter(trace_warm.path(), "gitignore-invalidation"),
        Some(0),
        "warm status should not invalidate gitignore after grit populated UNTR"
    );
    assert_eq!(
        stable.len(),
        before.len(),
        "UNTR payload size stable after grit write"
    );

    fsck_strict(root);
}

#[test]
fn t7063_git_untracked_cache_read_by_grit_status() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    fs::write(root.join("a.txt"), "a\n").expect("write");
    fs::write(root.join("u.txt"), "u\n").expect("write");
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-qm", "c"]);
    git_update_index_untracked_cache(root);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let idx = repo.index_handle().load().expect("load");
    assert!(
        idx.untracked_cache.is_some(),
        "git UNTR must parse into grit index"
    );

    let grit_paths = grit_untracked_paths(&repo);
    let git_paths = git_porcelain_untracked(root);
    assert_eq!(
        BTreeSet::from_iter(grit_paths),
        BTreeSet::from_iter(git_paths),
        "grit status must match git porcelain untracked list"
    );
}

#[test]
fn t7063_invalidation_matches_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    fs::write(root.join("tracked.txt"), "t\n").expect("write");
    git(root, &["add", "tracked.txt"]);
    git(root, &["commit", "-qm", "c"]);
    git_update_index_untracked_cache(root);

    let before_git = git_porcelain_untracked(root);

    fs::write(root.join("new.txt"), "n\n").expect("add untracked file");
    let after_add_git = git_porcelain_untracked(root);
    assert!(after_add_git.contains(&"new.txt".to_string()));
    assert_ne!(before_git, after_add_git);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let mut idx = repo.index_handle().load().expect("load");
    idx.invalidate_untracked_cache_for_path("new.txt");
    repo.index_handle()
        .write(&mut idx)
        .expect("persist invalidation");
    let grit_after = grit_untracked_paths(&repo);
    assert!(grit_after.iter().any(|p| p == "new.txt"));

    fs::remove_file(root.join("new.txt")).expect("remove");
    let mut idx = repo.index_handle().load().expect("reload");
    idx.invalidate_untracked_cache_for_path("new.txt");
    repo.index_handle()
        .write(&mut idx)
        .expect("persist after remove");
    let grit_removed = grit_untracked_paths(&repo);
    assert!(!grit_removed.iter().any(|p| p == "new.txt"));
}

#[test]
fn t7063_dir_flags_from_config_modes() {
    assert_eq!(
        dir_flags_from_config(&ConfigSet::new()),
        DIR_SHOW_OTHER_DIRECTORIES | DIR_HIDE_EMPTY_DIRECTORIES
    );
    let cfg_dir = tempfile::tempdir().expect("cfg temp");
    fs::write(
        cfg_dir.path().join("config"),
        "[status]\n\tshowUntrackedFiles = all\n",
    )
    .expect("write cfg");
    let cfg_all = ConfigSet::load(
        &grit_lib::config::environment::Environment::empty(),
        Some(cfg_dir.path()),
        true,
    )
    .expect("load cfg");
    assert_eq!(dir_flags_from_config(&cfg_all), 0);

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    append_git_config(&root.join(".git"), "[status]\n\tshowUntrackedFiles = all\n");
    fs::write(root.join("t.txt"), "t\n").expect("write");
    fs::write(root.join("u.txt"), "u\n").expect("write");
    git(root, &["add", "t.txt"]);
    git(root, &["commit", "-qm", "c"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let mut idx = repo.index_handle().load().expect("load");
    let cfg = repo.config().expect("config");
    let wt = repo.work_tree().expect("wt");
    let mut uc = UntrackedCache::new_shell(0, untracked_cache_ident(wt));
    refresh_untracked_cache_for_status(
        &repo,
        &idx,
        wt,
        cfg.as_ref(),
        &mut uc,
        true,
        UntrackedIgnoredMode::No,
    )
    .expect("refresh all mode");
    let from_cache = collect_untracked_from_cache(&uc);
    assert!(from_cache.iter().any(|p| p == "u.txt"));
    idx.untracked_cache = Some(uc);
    repo.index_handle().write(&mut idx).expect("write");

    let grit_paths = grit_untracked_paths(&repo);
    assert!(grit_paths.iter().any(|p| p == "u.txt"));
}

#[test]
fn t7063_ident_mismatch_discards_stale_cache() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    fs::write(root.join("a.txt"), "a\n").expect("write");
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-qm", "c"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let mut idx = repo.index_handle().load().expect("load");
    let uc = UntrackedCache::new_shell(6, b"wrong-ident\x00".to_vec());
    idx.untracked_cache = Some(uc);
    repo.index_handle()
        .write(&mut idx)
        .expect("write stale cache");

    fs::write(root.join("fresh.txt"), "f\n").expect("write untracked");
    let paths = grit_untracked_paths(&repo);
    assert!(
        paths.iter().any(|p| p == "fresh.txt"),
        "ident mismatch must bypass stale UNTR and scan the worktree"
    );
}

#[test]
fn untracked_extension_corrupt_payload_no_panic() {
    assert!(parse_untracked_extension(&[]).is_none());
    assert!(parse_untracked_extension(&[0]).is_none());
    let mut truncated = write_minimal_untr();
    truncated.truncate(truncated.len() / 2);
    assert!(parse_untracked_extension(&truncated).is_none());

    let mut no_nul = write_minimal_untr();
    no_nul.pop();
    assert!(parse_untracked_extension(&no_nul).is_none());
}

fn write_minimal_untr() -> Vec<u8> {
    let uc = UntrackedCache::new_shell(6, b"id\x00".to_vec());
    grit_lib::index::untracked_cache::write_untracked_extension(&uc)
}

#[test]
fn t7063_collect_untracked_matches_git_deep_tree() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    for d in 0..15 {
        let dir = root.join(format!("tree{d}"));
        fs::create_dir_all(&dir).expect("mkdir");
        fs::write(dir.join("tracked.txt"), "t\n").expect("write");
        fs::write(dir.join(format!("u{d}.txt")), format!("u{d}\n")).expect("write");
        git(root, &["add", &format!("tree{d}/tracked.txt")]);
    }
    git(root, &["commit", "-qm", "c"]);
    git_update_index_untracked_cache(root);

    let git_set: BTreeSet<String> = git_porcelain_untracked(root).into_iter().collect();
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let grit_set: BTreeSet<String> = grit_untracked_paths(&repo).into_iter().collect();
    assert_eq!(git_set, grit_set);

    let mut idx = repo.index_handle().load().expect("load");
    let cfg = repo.config().expect("config");
    let wt = repo.work_tree().expect("wt");
    let mut uc = idx.untracked_cache.take().expect("untr");
    refresh_untracked_cache_for_status(
        &repo,
        &idx,
        wt,
        cfg.as_ref(),
        &mut uc,
        true,
        UntrackedIgnoredMode::No,
    )
    .expect("refresh for collect");
    let from_cache: BTreeSet<String> = collect_untracked_from_cache(&uc).into_iter().collect();
    assert_eq!(git_set, from_cache);
    fsck_strict(root);
}

#[test]
fn index_name_hash_lazy_ignorecase_lookup() {
    use grit_lib::index::index_name_hash_lazy::{
        dump_lazy_init_name_hash, memihash, test_lazy_init_name_hash,
    };
    use grit_lib::index::Index;

    assert_eq!(memihash(b"Path/File"), memihash(b"path/file"));

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let mut idx = Index::empty(2);
    idx.add_or_replace(entry_from_worktree_path(&repo, "Mixed/Case.txt", b"x"));
    let threads = test_lazy_init_name_hash(&idx, false).expect("lazy init");
    assert_eq!(threads, 0, "small index uses single-threaded lazy path");
    dump_lazy_init_name_hash(&idx, false).expect("dump lazy hashes");
}
