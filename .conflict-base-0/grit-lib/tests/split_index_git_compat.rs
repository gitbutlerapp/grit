//! Split index (`link` + `sharedindex.<oid>`) compatibility with system `git` (t1700, t1701).

mod support;

use filetime::FileTime;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::diff_index_to_worktree;
use grit_lib::index::split_index::{
    format_dump_split_index_file, resolve_split_index_if_needed, WriteSplitIndexRequest,
};
use grit_lib::index::Index;
use grit_lib::repo::Repository;
use std::fs;
use std::path::Path;

use support::{
    append_git_config, assert_grit_index_matches_git_ls_files, assert_porcelain_v2_matches_git,
    entry_from_worktree_path, fsck_strict, git, git_update_index_split, index_contains_extension,
    list_sharedindex_paths, populate_flat_files, read_index_bytes,
};

fn init_repo_with_files(root: &Path, count: usize) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "core.untrackedCache", "false"]);
    populate_flat_files(root, "f", count);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "initial"]);
}

fn enable_split_index(git_dir: &Path) {
    append_git_config(
        git_dir,
        "[core]\n\tsplitIndex = true\n[splitIndex]\n\tmaxPercentChange = 20\n",
    );
}

fn assert_index_matches_git(worktree: &Path, repo: &Repository) {
    let index = repo
        .index_handle()
        .load()
        .expect("load index for ls-files oracle");
    assert_grit_index_matches_git_ls_files(worktree, &index);
}

/// Regression for <https://github.com/gitbutlerapp/grit/issues/989>: `grit status` must merge
/// Git's split index before diffing, and switch/merge guards must not false-positive on a clean tree.
#[test]
fn issue_989_minimal_split_index_status_and_worktree_guards() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.name", "a"]);
    git(root, &["config", "user.email", "a@b"]);
    std::fs::write(root.join("a"), "one\n").expect("write a");
    std::fs::write(root.join("b"), "two\n").expect("write b");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "1"]);
    git_update_index_split(root);

    assert!(
        git(root, &["status", "--short"]).is_empty(),
        "git must report a clean tree after split-index"
    );

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    assert_porcelain_v2_matches_git(root, &repo);

    grit_lib::porcelain::worktree_guard::ensure_worktree_clean_for_pick(&repo)
        .expect("pick guard on clean split-index repo");
    grit_lib::porcelain::worktree_guard::ensure_worktree_clean_for_merge(&repo)
        .expect("merge guard on clean split-index repo");

    std::fs::write(root.join("c"), "three\n").expect("write c");
    git(root, &["add", "c"]);
    git(root, &["commit", "-qm", "2"]);
    git_update_index_split(root);

    assert!(
        git(root, &["status", "--short"]).is_empty(),
        "git must stay clean after a second split-index rewrite"
    );
    assert_porcelain_v2_matches_git(root, &repo);
    grit_lib::porcelain::worktree_guard::ensure_worktree_clean_for_merge(&repo)
        .expect("merge guard after second split-index rewrite");
}

#[test]
fn t1700_split_index_written_by_grit_read_by_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo_with_files(root, 40);
    enable_split_index(&root.join(".git"));

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let mut idx = repo.index_handle().load().expect("load");
    repo.index_handle()
        .write(&mut idx)
        .expect("write split index");

    let bytes = read_index_bytes(&repo);
    assert!(
        index_contains_extension(&bytes, b"link"),
        "grit index must contain link extension"
    );
    assert_eq!(
        list_sharedindex_paths(repo.git_dir()).len(),
        1,
        "expected one sharedindex file"
    );

    assert_index_matches_git(root, &repo);
    fsck_strict(root);

    // Mutate one tracked file and rewrite split index; git must still read merged view.
    std::fs::write(root.join("f0020.txt"), "changed\n").expect("write");
    let mut idx = repo.index_handle().load().expect("reload");
    let pos = idx
        .entries()
        .iter()
        .position(|e| e.path == b"f0020.txt")
        .expect("entry");
    let mut entry = idx.entries()[pos].clone();
    entry.size = 8;
    entry.oid = repo
        .odb()
        .write(grit_lib::objects::ObjectKind::Blob, b"changed\n")
        .expect("blob");
    idx.add_or_replace(entry);
    repo.index_handle().write(&mut idx).expect("rewrite split");
    assert_index_matches_git(root, &repo);
    fsck_strict(root);
}

#[test]
fn t1700_git_split_index_read_by_grit() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo_with_files(root, 30);
    git_update_index_split(root);

    let bytes = fs::read(root.join(".git/index")).expect("read index");
    assert!(index_contains_extension(&bytes, b"link"));

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    assert_index_matches_git(root, &repo);

    // resolve_split_index_if_needed on raw parse (Git-shaped on-disk index).
    let mut raw = Index::parse(&bytes).expect("parse split index file");
    assert!(
        raw.split_index_base_oid().is_some(),
        "on-disk index keeps link before merge"
    );
    resolve_split_index_if_needed(&mut raw, repo.git_dir(), &repo.index_handle().path())
        .expect("resolve split");
    assert_eq!(raw.entries().len(), 30);
    assert_index_matches_git(root, &repo);
}

#[test]
fn t1700_split_index_delete_and_replace_bitmaps_match_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo_with_files(root, 12);
    git_update_index_split(root);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let mut idx = repo.index_handle().load().expect("load");

    idx.remove_path_all_stages(b"f0005.txt");
    let pos = idx
        .entries()
        .iter()
        .position(|e| e.path == b"f0008.txt")
        .expect("f0008");
    let mut entry = idx.entries()[pos].clone();
    entry.oid = repo
        .odb()
        .write(grit_lib::objects::ObjectKind::Blob, b"replaced\n")
        .expect("blob");
    entry.size = 9;
    idx.add_or_replace(entry);

    enable_split_index(&root.join(".git"));
    repo.index_handle()
        .write(&mut idx)
        .expect("write split after edits");
    assert_index_matches_git(root, &repo);
    fsck_strict(root);
}

#[test]
fn t1700_max_percent_change_rewrites_shared_index() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo_with_files(root, 10);
    enable_split_index(&root.join(".git"));

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let mut idx = repo.index_handle().load().expect("load");
    repo.index_handle().write(&mut idx).expect("initial split");
    let first_shared = list_sharedindex_paths(repo.git_dir());
    assert_eq!(first_shared.len(), 1);
    let first_oid = first_shared[0]
        .file_name()
        .unwrap()
        .to_string_lossy()
        .strip_prefix("sharedindex.")
        .unwrap()
        .to_string();

    append_git_config(
        &root.join(".git"),
        "[splitIndex]\n\tmaxPercentChange = 20\n",
    );

    for i in 0..4 {
        let path = format!("new{i}.txt");
        std::fs::write(root.join(&path), format!("n{i}\n")).expect("write");
        idx.add_or_replace(entry_from_worktree_path(
            &repo,
            &path,
            format!("n{i}\n").as_bytes(),
        ));
    }

    repo.index_handle()
        .write(&mut idx)
        .expect("rewrite after local additions");
    let shared = list_sharedindex_paths(repo.git_dir());
    let second_oid = shared
        .iter()
        .map(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .strip_prefix("sharedindex.")
                .unwrap()
                .to_string()
        })
        .find(|oid| oid != &first_oid)
        .expect("new sharedindex base after maxPercentChange rebuild");
    assert_ne!(
        first_oid, second_oid,
        "maxPercentChange should force a new shared index base"
    );
    assert_index_matches_git(root, &repo);
    fsck_strict(root);
}

#[test]
fn t1700_shared_index_expire_removes_stale_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo_with_files(root, 8);
    enable_split_index(&root.join(".git"));

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let mut idx = repo.index_handle().load().expect("load");
    repo.index_handle().write(&mut idx).expect("split");
    let first = list_sharedindex_paths(repo.git_dir());
    assert_eq!(first.len(), 1);

    // Force a rebuild with a new shared base while leaving the old file on disk.
    append_git_config(&root.join(".git"), "[splitIndex]\n\tmaxPercentChange = 0\n");
    std::fs::write(root.join("force-rebuild.txt"), "x\n").expect("write");
    idx.add_or_replace(entry_from_worktree_path(&repo, "force-rebuild.txt", b"x\n"));
    repo.index_handle().write(&mut idx).expect("rebuild shared");

    let all = list_sharedindex_paths(repo.git_dir());
    assert_eq!(
        all.len(),
        2,
        "stale shared file still present before expire"
    );

    let stale = all
        .iter()
        .find(|p| p != &&first[0])
        .expect("second shared file");
    let ancient = FileTime::from_unix_time(1_000_000, 0);
    filetime::set_file_mtime(stale, ancient).expect("backdate stale shared index");

    append_git_config(
        &root.join(".git"),
        "[splitIndex]\n\tsharedIndexExpire = now\n",
    );
    std::fs::write(root.join("touch.txt"), "y\n").expect("write");
    idx.add_or_replace(entry_from_worktree_path(&repo, "touch.txt", b"y\n"));
    repo.index_handle()
        .write(&mut idx)
        .expect("write triggers cleanup");

    let remaining: Vec<_> = list_sharedindex_paths(repo.git_dir())
        .into_iter()
        .filter(|p| p != stale)
        .collect();
    assert!(
        !stale.exists() || remaining.len() == 1,
        "expired sharedindex file should be removed"
    );
    assert!(!stale.exists(), "stale sharedindex must be deleted");
    fsck_strict(root);
}

#[test]
fn t1701_racy_split_index_entries_rechecked() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);

    const CONTENT_A: &str = "same length!!";
    const CONTENT_B: &str = "same length??";
    assert_eq!(CONTENT_A.len(), CONTENT_B.len());

    std::fs::write(root.join("f0001.txt"), CONTENT_A).expect("write indexed content");
    git(root, &["add", "f0001.txt"]);
    git(root, &["commit", "-qm", "initial"]);

    enable_split_index(&root.join(".git"));
    git_update_index_split(root);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let index = repo.index_handle().load().expect("load");
    let entry = index
        .entries()
        .iter()
        .find(|e| e.path == b"f0001.txt")
        .expect("entry");
    assert_eq!(entry.size, CONTENT_A.len() as u32);
    let entry_mtime = (entry.mtime_sec, entry.mtime_nsec);
    let index_mtime = index.source_mtime.expect("index mtime");

    std::fs::write(root.join("f0001.txt"), CONTENT_B).expect("same-size rewrite");
    filetime::set_file_mtime(
        &root.join("f0001.txt"),
        FileTime::from_unix_time(i64::from(entry_mtime.0), entry_mtime.1),
    )
    .expect("pin mtime to indexed stat");

    let git_names: Vec<String> = git(root, &["diff", "--name-only"])
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();
    assert_eq!(
        git_names,
        vec!["f0001.txt".to_owned()],
        "system git must detect racy same-size change under split index"
    );

    let mut grit_index = repo.index_handle().load().expect("reload");
    assert_eq!(grit_index.source_mtime, Some(index_mtime));
    let diff =
        diff_index_to_worktree(repo.odb(), &mut grit_index, root, false, false).expect("diff");
    assert_eq!(
        diff.len(),
        1,
        "split index must still re-verify racy entries"
    );
    assert_eq!(diff[0].path(), "f0001.txt");
}

#[test]
fn split_index_link_extension_corrupt_rejected_without_panic() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo_with_files(root, 3);
    git_update_index_split(root);

    let path = root.join(".git/index");
    let mut bytes = fs::read(&path).expect("read");
    let link_pos = bytes.windows(4).position(|w| w == b"link").expect("link");
    let garbage = link_pos + 8 + 20;
    if garbage < bytes.len() {
        bytes[garbage] = bytes[garbage].wrapping_add(0x55);
    }
    let err = Index::parse(&bytes).expect_err("corrupt link must fail parse");
    let msg = err.to_string();
    assert!(
        msg.contains("link")
            || msg.contains("bitmap")
            || msg.contains("corrupt")
            || msg.contains("checksum"),
        "unexpected error: {msg}"
    );
}

#[test]
fn grit_dump_split_index_reports_base_and_bitmaps() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo_with_files(root, 6);
    git_update_index_split(root);

    let bytes = fs::read(root.join(".git/index")).expect("read index");
    let raw = Index::parse(&bytes).expect("parse split index without merge");
    let dump = format_dump_split_index_file(&bytes, &raw).expect("dump");
    assert!(dump.contains("base "));
    assert!(dump.contains("replacements:"));
    assert!(dump.contains("deletions:"));
}

#[test]
fn split_index_explicit_no_split_writes_unified_index() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo_with_files(root, 5);
    enable_split_index(&root.join(".git"));
    git_update_index_split(root);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let mut idx = repo.index_handle().load().expect("load");
    repo.index_handle()
        .write_at_with_split(
            &repo.index_handle().path(),
            &mut idx,
            WriteSplitIndexRequest {
                explicit: Some(false),
            },
        )
        .expect("unified write");
    let bytes = read_index_bytes(&repo);
    assert!(
        !index_contains_extension(&bytes, b"link"),
        "--no-split-index equivalent must drop link extension"
    );
    assert_index_matches_git(root, &repo);
}
