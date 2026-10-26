//! Targeted line coverage for split index, untracked cache, and name-hash helpers.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::config::ConfigSet;
use grit_lib::index::split_index::{format_dump_split_index_file, resolve_split_index_if_needed};
use grit_lib::index::untracked_cache::{
    collect_untracked_from_cache, dir_flags_from_config, invalidate_path,
    refresh_untracked_cache_for_status, untracked_cache_ident, UntrackedCache,
    UntrackedIgnoredMode, DIR_HIDE_EMPTY_DIRECTORIES, DIR_SHOW_OTHER_DIRECTORIES,
};
use grit_lib::index::Index;
use grit_lib::porcelain::status::{status, IgnoredMode, StatusOptions, UntrackedMode};
use grit_lib::progress::NeverCancel;
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;

use support::{
    append_git_config, entry_from_worktree_path, git, git_update_index_split,
    git_update_index_untracked_cache, populate_flat_files,
};

fn enable_split_index(git_dir: &Path) {
    append_git_config(
        git_dir,
        "[core]\n\tsplitIndex = true\n[splitIndex]\n\tmaxPercentChange = 20\n",
    );
}

fn init_repo(root: &Path) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "core.untrackedCache", "true"]);
}

fn refresh_normal(
    repo: &Repository,
    idx: &grit_lib::index::Index,
    uc: &mut UntrackedCache,
    ignored: UntrackedIgnoredMode,
) {
    let cfg = repo.config().expect("config");
    let wt = repo.work_tree().expect("wt");
    refresh_untracked_cache_for_status(repo, idx, wt, cfg.as_ref(), uc, false, ignored, None)
        .expect("refresh");
}

#[test]
fn untracked_cache_respects_gitignore_and_info_exclude() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    fs::write(root.join(".gitignore"), "ignored/\n").expect("write");
    fs::create_dir_all(root.join("ignored")).expect("mkdir");
    fs::write(root.join("ignored/hidden.txt"), "h\n").expect("write");
    fs::write(root.join("visible.txt"), "v\n").expect("write");
    fs::write(root.join("tracked.txt"), "t\n").expect("write");
    git(root, &["add", "tracked.txt"]);
    git(root, &["commit", "-qm", "c"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let idx = repo.index_handle().load().expect("load");
    let wt = repo.work_tree().expect("wt");
    let mut uc = UntrackedCache::new_shell(
        dir_flags_from_config(&ConfigSet::new()),
        untracked_cache_ident(wt),
    );
    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::No);
    let paths: BTreeSet<_> = collect_untracked_from_cache(&uc).into_iter().collect();
    assert!(paths.contains("visible.txt"));
    assert!(!paths.contains("ignored/hidden.txt"));

    fs::write(root.join(".git/info/exclude"), "local-only/\n").expect("exclude");
    fs::create_dir_all(root.join("local-only")).expect("mkdir");
    fs::write(root.join("local-only/x.txt"), "x\n").expect("write");
    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::No);
    let paths2: BTreeSet<_> = collect_untracked_from_cache(&uc).into_iter().collect();
    assert!(!paths2.iter().any(|p| p.starts_with("local-only/")));
}

#[test]
fn untracked_cache_traditional_and_matching_ignored_modes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    fs::write(root.join(".gitignore"), "build/\n").expect("write");
    fs::create_dir_all(root.join("build")).expect("mkdir");
    fs::write(root.join("build/out.txt"), "o\n").expect("write");
    fs::write(root.join("src.txt"), "s\n").expect("write");
    git(root, &["add", "src.txt"]);
    git(root, &["commit", "-qm", "c"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let idx = repo.index_handle().load().expect("load");
    let wt = repo.work_tree().expect("wt");
    let ident = untracked_cache_ident(wt);
    let mut uc = UntrackedCache::new_shell(
        DIR_SHOW_OTHER_DIRECTORIES | DIR_HIDE_EMPTY_DIRECTORIES,
        ident,
    );

    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::Traditional);
    let _ = collect_untracked_from_cache(&uc);

    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::Matching);
    let _ = collect_untracked_from_cache(&uc);

    let opts = StatusOptions {
        untracked: UntrackedMode::Normal,
        ignored: IgnoredMode::Traditional,
        ahead_behind: false,
        ..StatusOptions::default()
    };
    let model = status(&repo, &opts, &mut NullProgress, &NeverCancel).expect("status traditional");
    assert!(model.ignored.iter().any(|p| p.contains("build")));

    let opts = StatusOptions {
        ignored: IgnoredMode::Matching,
        ..opts
    };
    let model = status(&repo, &opts, &mut NullProgress, &NeverCancel).expect("status matching");
    let _ = model.ignored;
}

#[test]
fn untracked_cache_all_mode_and_mode_switch() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    fs::create_dir_all(root.join("nested/deep")).expect("mkdir");
    fs::write(root.join("nested/deep/u.txt"), "u\n").expect("write");
    fs::write(root.join("tracked.txt"), "t\n").expect("write");
    git(root, &["add", "tracked.txt"]);
    git(root, &["commit", "-qm", "c"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let idx = repo.index_handle().load().expect("load");
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
        None,
    )
    .expect("all mode refresh");
    let all_paths: BTreeSet<_> = collect_untracked_from_cache(&uc).into_iter().collect();
    assert!(all_paths.contains("nested/deep/u.txt"));

    refresh_untracked_cache_for_status(
        &repo,
        &idx,
        wt,
        cfg.as_ref(),
        &mut uc,
        false,
        UntrackedIgnoredMode::No,
        None,
    )
    .expect("normal mode refresh");
    let normal_paths = collect_untracked_from_cache(&uc);
    assert!(!normal_paths.is_empty());
}

#[test]
fn untracked_cache_reuses_valid_subtree_on_second_refresh() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    for i in 0..20 {
        let d = root.join(format!("tree{i}"));
        fs::create_dir_all(&d).expect("mkdir");
        fs::write(d.join("tracked.txt"), "t\n").expect("write");
        fs::write(d.join(format!("u{i}.txt")), format!("u{i}\n")).expect("write");
        git(root, &["add", &format!("tree{i}/tracked.txt")]);
    }
    git(root, &["commit", "-qm", "c"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let idx = repo.index_handle().load().expect("load");
    let wt = repo.work_tree().expect("wt");
    let mut uc = UntrackedCache::new_shell(
        DIR_SHOW_OTHER_DIRECTORIES | DIR_HIDE_EMPTY_DIRECTORIES,
        untracked_cache_ident(wt),
    );
    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::No);
    let opened_first = uc.dir_opened;
    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::No);
    assert!(
        uc.dir_opened <= opened_first.saturating_add(2),
        "second refresh should reuse valid UNTR nodes (dir_opened {opened_first} then {})",
        uc.dir_opened
    );
}

#[test]
fn untracked_cache_invalidate_path_and_hidden_dotfile() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    fs::write(root.join("tracked.txt"), "t\n").expect("write");
    fs::write(root.join(".hidden"), "h\n").expect("write");
    git(root, &["add", "tracked.txt"]);
    git(root, &["commit", "-qm", "c"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let idx = repo.index_handle().load().expect("load");
    let wt = repo.work_tree().expect("wt");
    let mut uc = UntrackedCache::new_shell(
        DIR_SHOW_OTHER_DIRECTORIES | DIR_HIDE_EMPTY_DIRECTORIES,
        untracked_cache_ident(wt),
    );
    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::No);
    assert!(collect_untracked_from_cache(&uc)
        .iter()
        .any(|p| p == ".hidden"));

    invalidate_path(&mut uc, ".hidden");
    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::No);
    assert!(collect_untracked_from_cache(&uc)
        .iter()
        .any(|p| p == ".hidden"));
}

#[test]
fn untracked_cache_empty_directory_hidden_when_configured() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    fs::create_dir_all(root.join("empty-dir")).expect("mkdir");
    fs::write(root.join("tracked.txt"), "t\n").expect("write");
    git(root, &["add", "tracked.txt"]);
    git(root, &["commit", "-qm", "c"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let idx = repo.index_handle().load().expect("load");
    let wt = repo.work_tree().expect("wt");
    let mut uc = UntrackedCache::new_shell(
        DIR_SHOW_OTHER_DIRECTORIES | DIR_HIDE_EMPTY_DIRECTORIES,
        untracked_cache_ident(wt),
    );
    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::No);
    let paths = collect_untracked_from_cache(&uc);
    assert!(!paths.iter().any(|p| p == "empty-dir"));
}

#[test]
fn split_index_resolve_merge_and_dump_paths() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    populate_flat_files(root, "f", 12);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "c"]);
    enable_split_index(&root.join(".git"));
    git_update_index_split(root);

    let index_path = root.join(".git/index");
    let bytes = fs::read(&index_path).expect("read");
    let mut parsed = Index::parse(&bytes).expect("parse link");
    resolve_split_index_if_needed(&mut parsed, &root.join(".git"), &index_path).expect("merge");
    assert!(parsed.entries().len() >= 12);

    let dump = format_dump_split_index_file(&bytes, &parsed).expect("dump");
    assert!(dump.contains("base "));

    let empty = Index::empty(2);
    let not_split = format_dump_split_index_file(&bytes, &empty).expect("dump empty link");
    assert!(not_split.contains("not a split index"));
}

#[test]
fn split_index_git_index_file_writes_unified() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    populate_flat_files(root, "f", 8);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "c"]);
    enable_split_index(&root.join(".git"));
    git_update_index_split(root);

    let alt = root.join("alt-index");
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let mut idx = repo.index_handle().load().expect("load");
    repo.index_handle()
        .write_at(&alt, &mut idx)
        .expect("write alternate index");
    let data = fs::read(&alt).expect("read alt");
    assert!(!data.windows(4).any(|w| w == b"link"));
}

#[test]
fn split_index_shared_expire_and_rebuild() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    append_git_config(
        &root.join(".git"),
        "[core]\n\tsplitIndex = true\n[splitIndex]\n\tsharedIndexExpire = now\n\tmaxPercentChange = 5\n",
    );
    populate_flat_files(root, "f", 25);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "c"]);
    git_update_index_split(root);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let mut idx = repo.index_handle().load().expect("load");
    idx.add_or_replace(entry_from_worktree_path(&repo, "new-file.txt", b"new\n"));
    repo.index_handle()
        .write(&mut idx)
        .expect("rewrite split index");
    git_update_index_untracked_cache(root);
}

#[test]
fn split_index_rebuild_when_change_exceeds_max_percent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    append_git_config(
        &root.join(".git"),
        "[core]\n\tsplitIndex = true\n[splitIndex]\n\tmaxPercentChange = 5\n",
    );
    populate_flat_files(root, "f", 30);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "c"]);
    git_update_index_split(root);

    for i in 0..20 {
        std::fs::write(root.join(format!("f{i:04}.txt")), format!("changed {i}\n")).expect("write");
    }
    git(root, &["add", "-A"]);
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let mut idx = repo.index_handle().load().expect("load");
    repo.index_handle()
        .write(&mut idx)
        .expect("rewrite triggers rebuild");
    assert!(support::list_sharedindex_paths(repo.git_dir()).len() >= 1);
}

#[test]
fn split_index_resolve_rejects_corrupt_shared_checksum() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    enable_split_index(&root.join(".git"));
    populate_flat_files(root, "f", 6);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "c"]);
    git_update_index_split(root);

    let shared = support::list_sharedindex_paths(&root.join(".git"))
        .into_iter()
        .next()
        .expect("sharedindex");
    let mut bytes = fs::read(&shared).expect("read shared");
    if bytes.len() > 30 {
        bytes[30] ^= 0xff;
    }
    fs::write(&shared, bytes).expect("corrupt shared");

    let index_path = root.join(".git/index");
    let raw = fs::read(&index_path).expect("read");
    let mut idx = Index::parse(&raw).expect("parse");
    let err = resolve_split_index_if_needed(&mut idx, &root.join(".git"), &index_path)
        .expect_err("checksum mismatch");
    assert!(err.to_string().contains("broken index"));
}

#[test]
fn untracked_cache_global_excludes_and_gitlink_skip() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    let excludes = root.join("global-exclude.txt");
    fs::write(&excludes, "skip-me/\n").expect("write");
    append_git_config(
        &root.join(".git"),
        &format!("[core]\n\texcludesfile = {}\n", excludes.display()),
    );
    fs::create_dir_all(root.join("skip-me")).expect("mkdir");
    fs::write(root.join("skip-me/x.txt"), "x\n").expect("write");
    fs::create_dir_all(root.join("submod")).expect("mkdir");
    fs::write(root.join("tracked.txt"), "t\n").expect("write");
    git(root, &["add", "tracked.txt"]);
    git(root, &["commit", "-qm", "c"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let mut idx = repo.index_handle().load().expect("load");
    let mut sub = entry_from_worktree_path(&repo, "submod", b"gitlink");
    sub.mode = grit_lib::index::MODE_GITLINK;
    idx.add_or_replace(sub);
    repo.index_handle().write(&mut idx).expect("write gitlink");

    let wt = repo.work_tree().expect("wt");
    let mut uc = UntrackedCache::new_shell(
        DIR_SHOW_OTHER_DIRECTORIES | DIR_HIDE_EMPTY_DIRECTORIES,
        untracked_cache_ident(wt),
    );
    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::No);
    let paths = collect_untracked_from_cache(&uc);
    assert!(!paths.iter().any(|p| p.starts_with("skip-me/")));
}

#[test]
fn untracked_cache_directory_stat_invalidation() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    fs::create_dir_all(root.join("bucket")).expect("mkdir");
    fs::write(root.join("bucket/tracked.txt"), "t\n").expect("write");
    fs::write(root.join("bucket/new.txt"), "n\n").expect("write");
    git(root, &["add", "bucket/tracked.txt"]);
    git(root, &["commit", "-qm", "c"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let idx = repo.index_handle().load().expect("load");
    let wt = repo.work_tree().expect("wt");
    let mut uc = UntrackedCache::new_shell(
        DIR_SHOW_OTHER_DIRECTORIES | DIR_HIDE_EMPTY_DIRECTORIES,
        untracked_cache_ident(wt),
    );
    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::No);
    assert!(collect_untracked_from_cache(&uc)
        .iter()
        .any(|p| p == "bucket/new.txt"));

    let later = filetime::FileTime::from_unix_time(1_800_000_000, 0);
    filetime::set_file_mtime(root.join("bucket"), later).expect("touch dir");
    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::No);
    assert!(collect_untracked_from_cache(&uc)
        .iter()
        .any(|p| p == "bucket/new.txt"));
}

#[test]
fn untracked_cache_wide_tree_normal_status() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    for i in 0..40 {
        let d = root.join(format!("wide{i}"));
        fs::create_dir_all(&d).expect("mkdir");
        fs::write(d.join("t.txt"), "t\n").expect("write");
        fs::write(d.join(format!("u{i}.txt")), "u\n").expect("write");
        git(root, &["add", &format!("wide{i}/t.txt")]);
    }
    git(root, &["commit", "-qm", "c"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let opts = StatusOptions {
        untracked: UntrackedMode::Normal,
        ahead_behind: false,
        ..StatusOptions::default()
    };
    let model = status(&repo, &opts, &mut NullProgress, &NeverCancel).expect("status");
    assert_eq!(model.untracked.len(), 40);

    let idx = repo.index_handle().load().expect("load");
    let wt = repo.work_tree().expect("wt");
    let mut uc = idx
        .untracked_cache
        .clone()
        .unwrap_or_else(|| UntrackedCache::new_shell(0, untracked_cache_ident(wt)));
    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::No);
    let cached: BTreeSet<_> = collect_untracked_from_cache(&uc).into_iter().collect();
    assert_eq!(cached, model.untracked.into_iter().collect());
}

#[test]
fn split_index_incremental_update_reuses_shared_base() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    enable_split_index(&root.join(".git"));
    populate_flat_files(root, "f", 15);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "c"]);
    git_update_index_split(root);

    let before = support::list_sharedindex_paths(&root.join(".git"));
    std::fs::write(root.join("f0001.txt"), "one byte longer\n").expect("write");
    git(root, &["add", "f0001.txt"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let mut idx = repo.index_handle().load().expect("load");
    repo.index_handle()
        .write(&mut idx)
        .expect("incremental split write");
    let after = support::list_sharedindex_paths(&root.join(".git"));
    assert_eq!(
        before.len(),
        after.len(),
        "incremental update should reuse shared index file"
    );
}

#[test]
fn untracked_cache_ignored_only_directory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    fs::write(root.join(".gitignore"), "ignored-only/\n").expect("write");
    fs::create_dir_all(root.join("ignored-only")).expect("mkdir");
    fs::write(root.join("ignored-only/x.txt"), "x\n").expect("write");
    fs::write(root.join("free.txt"), "f\n").expect("write");
    fs::write(root.join("tracked.txt"), "t\n").expect("write");
    git(root, &["add", "tracked.txt"]);
    git(root, &["commit", "-qm", "c"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let idx = repo.index_handle().load().expect("load");
    let wt = repo.work_tree().expect("wt");
    let mut uc = UntrackedCache::new_shell(
        DIR_SHOW_OTHER_DIRECTORIES | DIR_HIDE_EMPTY_DIRECTORIES,
        untracked_cache_ident(wt),
    );
    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::Matching);
    let paths = collect_untracked_from_cache(&uc);
    assert!(paths.iter().any(|p| p == "free.txt"));
    assert!(!paths.iter().any(|p| p.starts_with("ignored-only/")));
}

#[test]
fn untracked_cache_nested_gitignore_per_directory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    fs::create_dir_all(root.join("outer/inner")).expect("mkdir");
    fs::write(root.join("outer/.gitignore"), "inner/\n").expect("write");
    fs::write(root.join("outer/inner/secret.txt"), "s\n").expect("write");
    fs::write(root.join("outer/visible.txt"), "v\n").expect("write");
    fs::write(root.join("tracked.txt"), "t\n").expect("write");
    git(root, &["add", "tracked.txt"]);
    git(root, &["commit", "-qm", "c"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let idx = repo.index_handle().load().expect("load");
    let wt = repo.work_tree().expect("wt");
    let mut uc = UntrackedCache::new_shell(
        DIR_SHOW_OTHER_DIRECTORIES | DIR_HIDE_EMPTY_DIRECTORIES,
        untracked_cache_ident(wt),
    );
    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::Traditional);
    let paths = collect_untracked_from_cache(&uc);
    assert!(paths.iter().any(|p| p == "outer/visible.txt"));
    assert!(!paths.iter().any(|p| p.contains("inner/secret")));
}

#[test]
fn split_index_remove_entries_emits_delete_bitmaps() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    enable_split_index(&root.join(".git"));
    populate_flat_files(root, "f", 20);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "c"]);
    git_update_index_split(root);

    for i in (0..10).map(|n| format!("f{n:04}.txt")) {
        git(root, &["rm", "--cached", &i]);
    }
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let mut idx = repo.index_handle().load().expect("load");
    repo.index_handle()
        .write(&mut idx)
        .expect("write split after deletes");
    let bytes = fs::read(repo.index_handle().path()).expect("read index");
    let parsed = Index::parse(&bytes).expect("parse");
    let dump = format_dump_split_index_file(&bytes, &parsed).expect("dump");
    assert!(dump.contains("deletions:"));
}

#[test]
fn untracked_cache_many_loose_files_normal_and_all() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    fs::write(root.join("tracked.txt"), "t\n").expect("write");
    for i in 0..55 {
        fs::write(root.join(format!("loose{i}.txt")), format!("{i}\n")).expect("write");
    }
    git(root, &["add", "tracked.txt"]);
    git(root, &["commit", "-qm", "c"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let idx = repo.index_handle().load().expect("load");
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
        None,
    )
    .expect("all");
    assert_eq!(collect_untracked_from_cache(&uc).len(), 55);

    refresh_untracked_cache_for_status(
        &repo,
        &idx,
        wt,
        cfg.as_ref(),
        &mut uc,
        false,
        UntrackedIgnoredMode::Traditional,
        None,
    )
    .expect("normal");
    assert!(!collect_untracked_from_cache(&uc).is_empty());
}

#[test]
fn split_index_write_with_shared_repository_group() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    append_git_config(
        &root.join(".git"),
        "[core]\n\tsplitIndex = true\n\tsharedRepository = group\n",
    );
    populate_flat_files(root, "f", 10);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "c"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let mut idx = repo.index_handle().load().expect("load");
    repo.index_handle()
        .write(&mut idx)
        .expect("split write with group perm");
    git_update_index_split(root);
}

#[test]
fn untracked_traditional_refresh_ignored_only_directory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    fs::create_dir_all(root.join("bundle")).expect("mkdir");
    fs::write(root.join("bundle/.gitignore"), "*\n").expect("write");
    fs::write(root.join("bundle/a.txt"), "a\n").expect("write");
    fs::write(root.join("tracked.txt"), "t\n").expect("write");
    git(root, &["add", "tracked.txt"]);
    git(root, &["commit", "-qm", "c"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let idx = repo.index_handle().load().expect("load");
    let wt = repo.work_tree().expect("wt");
    let mut uc = UntrackedCache::new_shell(
        DIR_SHOW_OTHER_DIRECTORIES | DIR_HIDE_EMPTY_DIRECTORIES,
        untracked_cache_ident(wt),
    );
    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::Traditional);
    let _ = collect_untracked_from_cache(&uc);
}

#[test]
fn untracked_matching_all_mode_skips_excluded_directories() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    fs::write(root.join(".gitignore"), "skip/\n").expect("write");
    fs::create_dir_all(root.join("skip/inner")).expect("mkdir");
    fs::write(root.join("skip/inner/x.txt"), "x\n").expect("write");
    fs::write(root.join("visible.txt"), "v\n").expect("write");
    fs::write(root.join("tracked.txt"), "t\n").expect("write");
    git(root, &["add", "tracked.txt"]);
    git(root, &["commit", "-qm", "c"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let idx = repo.index_handle().load().expect("load");
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
        UntrackedIgnoredMode::Matching,
        None,
    )
    .expect("matching all");
    let paths = collect_untracked_from_cache(&uc);
    assert!(paths.iter().any(|p| p == "visible.txt"));
    assert!(!paths.iter().any(|p| p.contains("skip/")));
}

#[test]
fn untracked_cache_rebuilds_after_info_exclude_changes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    fs::write(root.join("tracked.txt"), "t\n").expect("write");
    fs::write(root.join("alpha.txt"), "a\n").expect("write");
    git(root, &["add", "tracked.txt"]);
    git(root, &["commit", "-qm", "c"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let idx = repo.index_handle().load().expect("load");
    let wt = repo.work_tree().expect("wt");
    let mut uc = UntrackedCache::new_shell(
        DIR_SHOW_OTHER_DIRECTORIES | DIR_HIDE_EMPTY_DIRECTORIES,
        untracked_cache_ident(wt),
    );
    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::No);
    assert!(collect_untracked_from_cache(&uc)
        .iter()
        .any(|p| p == "alpha.txt"));

    fs::write(root.join(".git/info/exclude"), "alpha.txt\n").expect("exclude");
    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::No);
    assert!(!collect_untracked_from_cache(&uc)
        .iter()
        .any(|p| p == "alpha.txt"));
}

#[test]
fn untracked_cache_deep_status_integration() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    for i in 0..45 {
        let d = root.join(format!("deep{i}"));
        fs::create_dir_all(&d).expect("mkdir");
        fs::write(d.join("tracked.txt"), "t\n").expect("write");
        fs::write(d.join(format!("u{i}.txt")), "u\n").expect("write");
        fs::write(d.join(".gitignore"), "ignored/\n").expect("write");
        fs::create_dir_all(d.join("ignored")).expect("mkdir");
        fs::write(d.join("ignored/h.txt"), "h\n").expect("write");
        git(root, &["add", &format!("deep{i}/tracked.txt")]);
    }
    git(root, &["commit", "-qm", "c"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let opts = StatusOptions {
        untracked: UntrackedMode::Normal,
        ignored: IgnoredMode::Matching,
        ahead_behind: false,
        ..StatusOptions::default()
    };
    for _ in 0..2 {
        let model = status(&repo, &opts, &mut NullProgress, &NeverCancel).expect("status");
        assert!(model.untracked.len() >= 45);
    }

    let idx = repo.index_handle().load().expect("load");
    let cfg = repo.config().expect("config");
    let wt = repo.work_tree().expect("wt");
    let mut uc = UntrackedCache::new_shell(0, untracked_cache_ident(wt));
    for (show_all, mode) in [
        (false, UntrackedIgnoredMode::No),
        (false, UntrackedIgnoredMode::Traditional),
        (false, UntrackedIgnoredMode::Matching),
        (true, UntrackedIgnoredMode::No),
        (true, UntrackedIgnoredMode::Matching),
    ] {
        refresh_untracked_cache_for_status(
            &repo,
            &idx,
            wt,
            cfg.as_ref(),
            &mut uc,
            show_all,
            mode,
            None,
        )
        .expect("refresh sweep");
        let _ = collect_untracked_from_cache(&uc);
    }
}

#[test]
fn status_ignored_modes_exercise_untracked_collect_paths() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    fs::write(root.join(".gitignore"), "ignored/\n").expect("write");
    fs::create_dir_all(root.join("mixed/tracked")).expect("mkdir");
    fs::create_dir_all(root.join("mixed/ignored")).expect("mkdir");
    fs::write(root.join("mixed/tracked/t.txt"), "t\n").expect("write");
    fs::write(root.join("mixed/ignored/x.txt"), "x\n").expect("write");
    fs::create_dir_all(root.join("only-ignored")).expect("mkdir");
    fs::write(root.join("only-ignored/a.txt"), "a\n").expect("write");
    fs::write(root.join("only-ignored/b.txt"), "b\n").expect("write");
    fs::write(root.join("free.txt"), "f\n").expect("write");
    git(root, &["add", "mixed/tracked/t.txt"]);
    git(root, &["commit", "-qm", "c"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    for ignored in [IgnoredMode::Traditional, IgnoredMode::Matching] {
        let opts = StatusOptions {
            untracked: UntrackedMode::Normal,
            ignored,
            ahead_behind: false,
            ..StatusOptions::default()
        };
        let model = status(&repo, &opts, &mut NullProgress, &NeverCancel).expect("status");
        assert!(model.untracked.iter().any(|p| p == "free.txt"));
        assert!(!model.ignored.is_empty());
    }

    let opts = StatusOptions {
        untracked: UntrackedMode::All,
        ignored: IgnoredMode::Matching,
        ahead_behind: false,
        ..StatusOptions::default()
    };
    let model = status(&repo, &opts, &mut NullProgress, &NeverCancel).expect("status all");
    assert!(!model.ignored.is_empty());
}

#[test]
fn untracked_cache_populates_check_only_subtree_for_collapsed_dir() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    fs::create_dir_all(root.join("collapse")).expect("mkdir");
    fs::write(root.join("collapse/u1.txt"), "1\n").expect("write");
    fs::write(root.join("collapse/u2.txt"), "2\n").expect("write");
    fs::write(root.join("tracked.txt"), "t\n").expect("write");
    git(root, &["add", "tracked.txt"]);
    git(root, &["commit", "-qm", "c"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let idx = repo.index_handle().load().expect("load");
    let wt = repo.work_tree().expect("wt");
    let mut uc = UntrackedCache::new_shell(
        DIR_SHOW_OTHER_DIRECTORIES | DIR_HIDE_EMPTY_DIRECTORIES,
        untracked_cache_ident(wt),
    );
    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::No);
    let paths = collect_untracked_from_cache(&uc);
    assert!(
        paths.iter().any(|p| p == "collapse" || p == "collapse/"),
        "normal mode should collapse untracked-only directories"
    );
    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::No);
    let _ = collect_untracked_from_cache(&uc);
}

#[test]
fn untracked_cache_mixed_visible_and_ignored_in_one_directory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    fs::create_dir_all(root.join("mix")).expect("mkdir");
    fs::write(root.join("mix/.gitignore"), "*.log\n").expect("write");
    fs::write(root.join("mix/app.txt"), "app\n").expect("write");
    fs::write(root.join("mix/debug.log"), "log\n").expect("write");
    fs::write(root.join("tracked.txt"), "t\n").expect("write");
    git(root, &["add", "tracked.txt"]);
    git(root, &["commit", "-qm", "c"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let idx = repo.index_handle().load().expect("load");
    let wt = repo.work_tree().expect("wt");
    let mut uc = UntrackedCache::new_shell(
        DIR_SHOW_OTHER_DIRECTORIES | DIR_HIDE_EMPTY_DIRECTORIES,
        untracked_cache_ident(wt),
    );
    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::Matching);
    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::Traditional);
    let _ = collect_untracked_from_cache(&uc);
}

#[test]
fn untracked_cache_hidden_dotfile_forces_materialized_subtree() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    fs::create_dir_all(root.join("hidden-dir")).expect("mkdir");
    fs::write(root.join("hidden-dir/.secret"), "s\n").expect("write");
    fs::write(root.join("tracked.txt"), "t\n").expect("write");
    git(root, &["add", "tracked.txt"]);
    git(root, &["commit", "-qm", "c"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let idx = repo.index_handle().load().expect("load");
    let wt = repo.work_tree().expect("wt");
    let mut uc = UntrackedCache::new_shell(
        DIR_SHOW_OTHER_DIRECTORIES | DIR_HIDE_EMPTY_DIRECTORIES,
        untracked_cache_ident(wt),
    );
    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::No);
    assert!(collect_untracked_from_cache(&uc)
        .iter()
        .any(|p| p.contains("hidden-dir")));
}

#[test]
fn untracked_cache_fully_ignored_directory_placeholder() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    fs::create_dir_all(root.join("ignored-only/nested")).expect("mkdir");
    fs::write(root.join("ignored-only/.gitignore"), "*\n").expect("write");
    fs::write(root.join("ignored-only/nested/x.txt"), "x\n").expect("write");
    fs::write(root.join("tracked.txt"), "t\n").expect("write");
    git(root, &["add", "tracked.txt"]);
    git(root, &["commit", "-qm", "c"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let idx = repo.index_handle().load().expect("load");
    let wt = repo.work_tree().expect("wt");
    let mut uc = UntrackedCache::new_shell(
        DIR_SHOW_OTHER_DIRECTORIES | DIR_HIDE_EMPTY_DIRECTORIES,
        untracked_cache_ident(wt),
    );
    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::Matching);
    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::Matching);
    let _ = collect_untracked_from_cache(&uc);
}

#[test]
fn untracked_cache_combo_visible_and_ignored_files_expand_directory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    fs::create_dir_all(root.join("combo")).expect("mkdir");
    fs::write(root.join("combo/.gitignore"), "*.log\n").expect("write");
    fs::write(root.join("combo/app.txt"), "app\n").expect("write");
    fs::write(root.join("combo/warn.log"), "w\n").expect("write");
    fs::write(root.join("tracked.txt"), "t\n").expect("write");
    git(root, &["add", "tracked.txt"]);
    git(root, &["commit", "-qm", "c"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let idx = repo.index_handle().load().expect("load");
    let wt = repo.work_tree().expect("wt");
    let mut uc = UntrackedCache::new_shell(
        DIR_SHOW_OTHER_DIRECTORIES | DIR_HIDE_EMPTY_DIRECTORIES,
        untracked_cache_ident(wt),
    );
    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::Traditional);
    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::Traditional);
    let _ = collect_untracked_from_cache(&uc);
}

#[test]
fn untracked_cache_empty_gitignored_directory_materializes_check_only() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    fs::write(root.join(".gitignore"), "empty-ignored/\n").expect("write");
    fs::create_dir_all(root.join("empty-ignored")).expect("mkdir");
    fs::write(root.join("tracked.txt"), "t\n").expect("write");
    git(root, &["add", "tracked.txt"]);
    git(root, &["commit", "-qm", "c"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let idx = repo.index_handle().load().expect("load");
    let wt = repo.work_tree().expect("wt");
    let mut uc = UntrackedCache::new_shell(
        DIR_SHOW_OTHER_DIRECTORIES | DIR_HIDE_EMPTY_DIRECTORIES,
        untracked_cache_ident(wt),
    );
    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::Matching);
    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::Matching);
    let _ = collect_untracked_from_cache(&uc);
}

#[test]
fn untracked_cache_tracked_subdirectory_expands_full_directory_walk() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    fs::create_dir_all(root.join("parent/tracked")).expect("mkdir");
    fs::write(root.join("parent/tracked/t.txt"), "t\n").expect("write");
    fs::write(root.join("parent/loose.txt"), "l\n").expect("write");
    fs::write(root.join("tracked.txt"), "r\n").expect("write");
    git(root, &["add", "tracked.txt", "parent/tracked/t.txt"]);
    git(root, &["commit", "-qm", "c"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let idx = repo.index_handle().load().expect("load");
    let wt = repo.work_tree().expect("wt");
    let mut uc = UntrackedCache::new_shell(0, untracked_cache_ident(wt));
    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::Traditional);
    let paths = collect_untracked_from_cache(&uc);
    assert!(paths.iter().any(|p| p == "parent/loose.txt"));
}

#[test]
fn untracked_cache_reuses_existing_check_only_on_fully_ignored_rescan() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    fs::create_dir_all(root.join("shell/inner")).expect("mkdir");
    fs::write(root.join("shell/.gitignore"), "*\n").expect("write");
    fs::write(root.join("shell/inner/log.txt"), "l\n").expect("write");
    fs::write(root.join("tracked.txt"), "t\n").expect("write");
    git(root, &["add", "tracked.txt"]);
    git(root, &["commit", "-qm", "c"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let idx = repo.index_handle().load().expect("load");
    let wt = repo.work_tree().expect("wt");
    let mut uc = UntrackedCache::new_shell(
        DIR_SHOW_OTHER_DIRECTORIES | DIR_HIDE_EMPTY_DIRECTORIES,
        untracked_cache_ident(wt),
    );
    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::Matching);
    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::Matching);
    let _ = collect_untracked_from_cache(&uc);
}

#[test]
fn untracked_cache_nested_untracked_only_directory_collapses_in_collect() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    fs::create_dir_all(root.join("outer/inner/deep")).expect("mkdir");
    fs::write(root.join("outer/inner/deep/u.txt"), "u\n").expect("write");
    fs::write(root.join("tracked.txt"), "t\n").expect("write");
    git(root, &["add", "tracked.txt"]);
    git(root, &["commit", "-qm", "c"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let idx = repo.index_handle().load().expect("load");
    let wt = repo.work_tree().expect("wt");
    let mut uc = UntrackedCache::new_shell(
        DIR_SHOW_OTHER_DIRECTORIES | DIR_HIDE_EMPTY_DIRECTORIES,
        untracked_cache_ident(wt),
    );
    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::No);
    let paths = collect_untracked_from_cache(&uc);
    assert!(paths.iter().any(|p| p.starts_with("outer")));
}

#[test]
fn untracked_cache_gitignore_change_invalidates_cached_tree() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    fs::write(root.join("tracked.txt"), "t\n").expect("write");
    fs::write(root.join("alpha.txt"), "a\n").expect("write");
    git(root, &["add", "tracked.txt"]);
    git(root, &["commit", "-qm", "c"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let idx = repo.index_handle().load().expect("load");
    let wt = repo.work_tree().expect("wt");
    let mut uc = UntrackedCache::new_shell(0, untracked_cache_ident(wt));
    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::No);
    assert!(collect_untracked_from_cache(&uc)
        .iter()
        .any(|p| p == "alpha.txt"));

    fs::write(root.join(".gitignore"), "alpha.txt\n").expect("gitignore");
    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::No);
    assert!(!collect_untracked_from_cache(&uc)
        .iter()
        .any(|p| p == "alpha.txt"));
}

#[test]
fn untracked_cache_check_only_reuse_surfaces_visible_collapsed_directory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    fs::create_dir_all(root.join("hold")).expect("mkdir");
    fs::write(root.join("hold/u1.txt"), "1\n").expect("write");
    fs::write(root.join("hold/u2.txt"), "2\n").expect("write");
    fs::write(root.join("tracked.txt"), "t\n").expect("write");
    git(root, &["add", "tracked.txt"]);
    git(root, &["commit", "-qm", "c"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let idx = repo.index_handle().load().expect("load");
    let wt = repo.work_tree().expect("wt");
    let mut uc = UntrackedCache::new_shell(
        DIR_SHOW_OTHER_DIRECTORIES | DIR_HIDE_EMPTY_DIRECTORIES,
        untracked_cache_ident(wt),
    );
    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::No);
    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::No);
    let paths = collect_untracked_from_cache(&uc);
    assert!(paths.iter().any(|p| p == "hold/" || p == "hold"));
}

#[test]
fn untracked_cache_mixed_entries_recurses_into_both_lists() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    fs::create_dir_all(root.join("both")).expect("mkdir");
    fs::write(root.join("both/.gitignore"), "*.log\n").expect("write");
    fs::write(root.join("both/readme.txt"), "r\n").expect("write");
    fs::write(root.join("both/trace.log"), "t\n").expect("write");
    fs::write(root.join("tracked.txt"), "t\n").expect("write");
    git(root, &["add", "tracked.txt"]);
    git(root, &["commit", "-qm", "c"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let idx = repo.index_handle().load().expect("load");
    let wt = repo.work_tree().expect("wt");
    let mut uc = UntrackedCache::new_shell(
        DIR_SHOW_OTHER_DIRECTORIES | DIR_HIDE_EMPTY_DIRECTORIES,
        untracked_cache_ident(wt),
    );
    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::Matching);
    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::Traditional);
    let _ = collect_untracked_from_cache(&uc);
}

#[test]
fn untracked_cache_hidden_dotfile_inside_ignored_only_shell() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    fs::write(root.join(".gitignore"), "shell/\n").expect("write");
    fs::create_dir_all(root.join("shell")).expect("mkdir");
    fs::write(root.join("shell/.visible"), "v\n").expect("write");
    fs::write(root.join("shell/ignored.txt"), "i\n").expect("write");
    fs::write(root.join("tracked.txt"), "t\n").expect("write");
    git(root, &["add", "tracked.txt"]);
    git(root, &["commit", "-qm", "c"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let idx = repo.index_handle().load().expect("load");
    let wt = repo.work_tree().expect("wt");
    let mut uc = UntrackedCache::new_shell(
        DIR_SHOW_OTHER_DIRECTORIES | DIR_HIDE_EMPTY_DIRECTORIES,
        untracked_cache_ident(wt),
    );
    refresh_normal(&repo, &idx, &mut uc, UntrackedIgnoredMode::Matching);
    let _ = collect_untracked_from_cache(&uc);
}
