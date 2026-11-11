//! Coverage for the `repo.diff()` entry points: metadata diffs between trees, commits, the
//! index and the work tree under each `TreeDiffOptions` knob, plus the content methods.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use grit_lib::diff::{
    BreakRewrites, CopyDetection, DiffDetail, DiffEntry, DiffMerges, DiffOptions, DiffRequest,
    DiffStatus, DiffTarget, RenameDetection, SubmoduleIgnoreMode, TreeDiffOptions,
};
use grit_lib::error::{Error, RefError};
use grit_lib::index::{entry_from_stat, Index, IndexEntry, MODE_REGULAR};
use grit_lib::objects::{serialize_tree, FileMode, ObjectId, ObjectKind, TreeEntry};
use grit_lib::parallel::ThreadPolicy;
use grit_lib::repo::{InitOptions, Repository, RepositoryError};
use grit_lib::worktree::pathspec::{Pathspec, PathspecGlobalFlags};
use tempfile::TempDir;

fn numbered(prefix: &str, edit_line: Option<usize>) -> String {
    (1..=20)
        .map(|i| {
            if Some(i) == edit_line {
                format!("{prefix} line {i} edited\n")
            } else {
                format!("{prefix} line {i}\n")
            }
        })
        .collect()
}

/// Files of the old side of every fixture diff.
fn base_files() -> Vec<(&'static str, String)> {
    vec![
        ("a.txt", numbered("alpha", None)),
        ("dir/b.txt", "b1\nb2\n".to_owned()),
        ("keep.txt", numbered("keep", None)),
    ]
}

/// `a.txt` moves with one edited line, `dir/b.txt` changes, and `keep.txt` gets an unmodified
/// copy.
fn tip_files() -> Vec<(&'static str, String)> {
    vec![
        ("moved.txt", numbered("alpha", Some(10))),
        ("dir/b.txt", "b1\nb2 changed\n".to_owned()),
        ("keep.txt", numbered("keep", None)),
        ("keep-copy.txt", numbered("keep", None)),
    ]
}

fn write_tree(repo: &Repository, files: &[(&str, String)]) -> ObjectId {
    let mut root: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for (path, body) in files {
        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
        root.entry(dir.to_owned())
            .or_default()
            .push((name.to_owned(), body.clone()));
    }
    let mut subtrees: Vec<TreeEntry> = Vec::new();
    for (dir, children) in root.iter().filter(|(dir, _)| !dir.is_empty()) {
        let entries: Vec<TreeEntry> = children
            .iter()
            .map(|(name, body)| blob_entry(repo, name, body))
            .collect();
        let oid = repo
            .odb()
            .write(ObjectKind::Tree, &serialize_tree(&entries))
            .expect("subtree");
        subtrees.push(TreeEntry::new(dir.as_bytes().to_vec(), FileMode::TREE, oid));
    }
    let mut entries: Vec<TreeEntry> = root
        .get("")
        .map(|children| {
            children
                .iter()
                .map(|(name, body)| blob_entry(repo, name, body))
                .collect()
        })
        .unwrap_or_default();
    entries.extend(subtrees);
    entries.sort_by(|a, b| tree_sort_key(a).cmp(&tree_sort_key(b)));
    repo.odb()
        .write(ObjectKind::Tree, &serialize_tree(&entries))
        .expect("root tree")
}

fn tree_sort_key(entry: &TreeEntry) -> Vec<u8> {
    let mut key = entry.name.clone();
    if entry.mode == FileMode::TREE {
        key.push(b'/');
    }
    key
}

fn blob_entry(repo: &Repository, name: &str, body: &str) -> TreeEntry {
    let oid = repo
        .odb()
        .write(ObjectKind::Blob, body.as_bytes())
        .expect("blob");
    TreeEntry::new(name.as_bytes().to_vec(), FileMode::REGULAR, oid)
}

fn write_commit(repo: &Repository, tree: ObjectId, parent: Option<ObjectId>) -> ObjectId {
    let mut body = format!("tree {tree}\n");
    if let Some(parent) = parent {
        body.push_str(&format!("parent {parent}\n"));
    }
    body.push_str("author T <t@example.com> 1700000000 +0000\n");
    body.push_str("committer T <t@example.com> 1700000000 +0000\n\nmsg\n");
    repo.odb()
        .write(ObjectKind::Commit, body.as_bytes())
        .expect("commit")
}

struct Fixture {
    _dir: TempDir,
    repo: Repository,
    base_tree: ObjectId,
    tip_tree: ObjectId,
    base_commit: ObjectId,
    tip_commit: ObjectId,
}

/// Repository whose `HEAD` is the tip commit (parent: base) and whose index and work tree
/// match the tip.
fn handle_fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    let base_tree = write_tree(&repo, &base_files());
    let tip_tree = write_tree(&repo, &tip_files());
    let base_commit = write_commit(&repo, base_tree, None);
    let tip_commit = write_commit(&repo, tip_tree, Some(base_commit));
    let head_ref = repo.git_dir().join("refs/heads/main");
    fs::create_dir_all(head_ref.parent().expect("refs dir")).expect("refs dir");
    fs::write(&head_ref, format!("{tip_commit}\n")).expect("main");
    fs::write(repo.git_dir().join("HEAD"), "ref: refs/heads/main\n").expect("HEAD");
    let mut index = checkout(&repo, dir.path(), &tip_files());
    index
        .write_to_path(&repo.index().path(), false)
        .expect("write index");
    Fixture {
        _dir: dir,
        repo,
        base_tree,
        tip_tree,
        base_commit,
        tip_commit,
    }
}

/// Write `files` into `work_tree` and return an index whose stat data matches them.
fn checkout(repo: &Repository, work_tree: &Path, files: &[(&str, String)]) -> Index {
    let mut index = Index::new();
    for (path, body) in files {
        let abs = work_tree.join(path);
        fs::create_dir_all(abs.parent().expect("parent")).expect("mkdir");
        fs::write(&abs, body).expect("write file");
        let oid = repo
            .odb()
            .write(ObjectKind::Blob, body.as_bytes())
            .expect("blob");
        let entry = entry_from_stat(&abs, path.as_bytes(), oid, MODE_REGULAR).expect("entry");
        index.add_or_replace(entry);
    }
    index
}

fn as_refs(files: &[(String, String)]) -> Vec<(&str, String)> {
    files.iter().map(|(p, b)| (p.as_str(), b.clone())).collect()
}

fn summary(entries: &[DiffEntry]) -> Vec<String> {
    let mut rows: Vec<String> = entries
        .iter()
        .map(|e| match e.old_path.as_deref() {
            Some(old) if Some(old) != e.new_path.as_deref() && e.new_path.is_some() => {
                format!("{} {old} -> {}", e.status.letter(), e.path())
            }
            _ => format!("{} {}", e.status.letter(), e.path()),
        })
        .collect();
    rows.sort();
    rows
}

fn pathspec(specs: &[&str]) -> Pathspec {
    Pathspec::scoped(specs, PathspecGlobalFlags::default())
}

fn every_path() -> Pathspec {
    Pathspec::entire_tree(PathspecGlobalFlags::default())
}

fn renames() -> TreeDiffOptions {
    TreeDiffOptions::default().with_renames(RenameDetection::Similarity(50))
}

#[test]
fn tree_to_tree_defaults_list_every_path_without_pairing() {
    let fx = handle_fixture();
    let result = fx
        .repo
        .diff()
        .tree_to_tree(
            Some(&fx.base_tree),
            Some(&fx.tip_tree),
            &TreeDiffOptions::default(),
        )
        .expect("diff");
    assert_eq!(
        summary(&result.entries),
        ["A keep-copy.txt", "A moved.txt", "D a.txt", "M dir/b.txt"]
    );
    assert_eq!(result.inexact_rename_skipped, None);
    assert!(!result.index_stat_refreshed);

    let empty = fx
        .repo
        .diff()
        .tree_to_tree(None, Some(&fx.base_tree), &TreeDiffOptions::default())
        .expect("root diff");
    assert_eq!(empty.entries.len(), base_files().len());
    assert!(empty.entries.iter().all(|e| e.status == DiffStatus::Added));
}

#[test]
fn tree_to_tree_renames_on_pairs_the_moved_file() {
    let fx = handle_fixture();
    let result = fx
        .repo
        .diff()
        .tree_to_tree(Some(&fx.base_tree), Some(&fx.tip_tree), &renames())
        .expect("diff");
    assert_eq!(
        summary(&result.entries),
        ["A keep-copy.txt", "M dir/b.txt", "R a.txt -> moved.txt"]
    );
    let renamed = result
        .entries
        .iter()
        .find(|e| e.status == DiffStatus::Renamed)
        .expect("rename");
    assert!(renamed.score.is_some_and(|s| s >= 90 && s < 100));

    let exact_only = TreeDiffOptions::default().with_renames(RenameDetection::Similarity(100));
    let result = fx
        .repo
        .diff()
        .tree_to_tree(Some(&fx.base_tree), Some(&fx.tip_tree), &exact_only)
        .expect("diff");
    assert!(
        result
            .entries
            .iter()
            .all(|e| e.status != DiffStatus::Renamed),
        "-M100 pairs only identical content: {:?}",
        result.entries
    );
}

#[test]
fn tree_to_tree_copies_need_find_harder_for_unmodified_sources() {
    let fx = handle_fixture();
    let diff = fx.repo.diff();
    let changed_sources = renames().with_copies(CopyDetection::Similarity(50));
    let result = diff
        .tree_to_tree(Some(&fx.base_tree), Some(&fx.tip_tree), &changed_sources)
        .expect("diff");
    assert!(
        summary(&result.entries).contains(&"A keep-copy.txt".to_owned()),
        "an unmodified source is not a -C source: {:?}",
        result.entries
    );

    let harder = renames().with_copies(CopyDetection::FindHarder(50));
    let result = diff
        .tree_to_tree(Some(&fx.base_tree), Some(&fx.tip_tree), &harder)
        .expect("diff");
    assert_eq!(
        summary(&result.entries),
        [
            "C keep.txt -> keep-copy.txt",
            "M dir/b.txt",
            "R a.txt -> moved.txt"
        ]
    );
}

#[test]
fn tree_to_tree_break_rewrites_splits_a_full_rewrite() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    // Git only breaks files of at least 400 bytes.
    let old = write_tree(&repo, &[("f.txt", numbered("old content", None).repeat(4))]);
    let new = write_tree(
        &repo,
        &[("f.txt", numbered("brand new body", None).repeat(4))],
    );
    let plain = repo
        .diff()
        .tree_to_tree(Some(&old), Some(&new), &TreeDiffOptions::default())
        .expect("diff");
    assert_eq!(summary(&plain.entries), ["M f.txt"]);
    assert_eq!(plain.entries[0].score, None);

    let broken = TreeDiffOptions::default().with_break_rewrites(BreakRewrites::DEFAULT);
    let result = repo
        .diff()
        .tree_to_tree(Some(&old), Some(&new), &broken)
        .expect("diff");
    assert_eq!(summary(&result.entries), ["M f.txt"]);
    assert!(
        result.entries[0].score.is_some(),
        "-B annotates the rewrite with its dissimilarity: {:?}",
        result.entries
    );

    let split = broken.with_renames(RenameDetection::Similarity(50));
    let result = repo
        .diff()
        .tree_to_tree(Some(&old), Some(&new), &split)
        .expect("diff");
    assert_eq!(summary(&result.entries), ["A f.txt", "D f.txt"]);
}

#[test]
fn tree_to_tree_pathspec_prefix_and_show_tree_entries() {
    let fx = handle_fixture();
    let diff = fx.repo.diff();
    let only_dir = TreeDiffOptions::default().with_pathspec(Some(pathspec(&["dir"])));
    let result = diff
        .tree_to_tree(Some(&fx.base_tree), Some(&fx.tip_tree), &only_dir)
        .expect("diff");
    assert_eq!(summary(&result.entries), ["M dir/b.txt"]);

    let glob = TreeDiffOptions::default()
        .with_renames(RenameDetection::Similarity(50))
        .with_pathspec(Some(pathspec(&["*.txt"])));
    let result = diff
        .tree_to_tree(Some(&fx.base_tree), Some(&fx.tip_tree), &glob)
        .expect("diff");
    assert_eq!(summary(&result.entries).len(), 3);

    let none = TreeDiffOptions::default().with_pathspec(Some(pathspec(&["missing/"])));
    let result = diff
        .tree_to_tree(Some(&fx.base_tree), Some(&fx.tip_tree), &none)
        .expect("diff");
    assert!(result.entries.is_empty());

    let trees = TreeDiffOptions::default().with_show_tree_entries(true);
    let result = diff
        .tree_to_tree(Some(&fx.base_tree), Some(&fx.tip_tree), &trees)
        .expect("diff");
    let dir = result
        .entries
        .iter()
        .find(|e| e.path() == "dir")
        .expect("changed subtree is reported");
    assert_eq!(dir.status, DiffStatus::Modified);
    assert_eq!(dir.new_mode, FileMode::TREE);
    assert!(result.entries.iter().any(|e| e.path() == "dir/b.txt"));

    let prefixed = TreeDiffOptions::default().with_prefix("sub");
    let result = diff
        .tree_to_tree(Some(&fx.base_tree), Some(&fx.tip_tree), &prefixed)
        .expect("diff");
    assert!(
        result.entries.iter().all(|e| e.path().starts_with("sub/")),
        "{:?}",
        result.entries
    );
    assert_eq!(result.entries.len(), 4);
}

#[test]
fn tree_to_tree_thread_policies_agree() {
    let fx = handle_fixture();
    let serial = renames()
        .with_copies(CopyDetection::FindHarder(50))
        .with_threads(Some(ThreadPolicy::with_threads_override(Some(1))));
    let parallel = serial.clone().with_threads(Some(
        ThreadPolicy::with_threads_override(Some(4))
            .min_items_override(Some(0))
            .min_total_bytes_override(Some(0)),
    ));
    let diff = fx.repo.diff();
    let a = diff
        .tree_to_tree(Some(&fx.base_tree), Some(&fx.tip_tree), &serial)
        .expect("serial");
    let b = diff
        .tree_to_tree(Some(&fx.base_tree), Some(&fx.tip_tree), &parallel)
        .expect("parallel");
    assert_eq!(a, b);
}

#[test]
fn tree_to_tree_submodules_all_drops_gitlinks() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    let sub_commit = write_commit(&repo, write_tree(&repo, &[]), None);
    let gitlink = |oid: ObjectId| {
        let entries = vec![
            blob_entry(&repo, "file.txt", "x\n"),
            TreeEntry::new(b"sub", FileMode::GITLINK, oid),
        ];
        repo.odb()
            .write(ObjectKind::Tree, &serialize_tree(&entries))
            .expect("tree")
    };
    let old = gitlink(sub_commit);
    let new = gitlink(write_commit(
        &repo,
        write_tree(&repo, &[]),
        Some(sub_commit),
    ));
    let shown = repo
        .diff()
        .tree_to_tree(Some(&old), Some(&new), &TreeDiffOptions::default())
        .expect("diff");
    assert_eq!(summary(&shown.entries), ["M sub"]);
    let hidden = TreeDiffOptions::default().with_submodules(SubmoduleIgnoreMode::All);
    let result = repo
        .diff()
        .tree_to_tree(Some(&old), Some(&new), &hidden)
        .expect("diff");
    assert!(result.entries.is_empty());
}

#[test]
fn tree_to_tree_reports_rename_limit_skips() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    let config = repo.git_dir().join("config");
    let mut text = fs::read_to_string(&config).expect("config");
    text.push_str("[diff]\n\trenameLimit = 2\n");
    fs::write(&config, text).expect("config");
    let repo = Repository::open(
        repo.git_dir(),
        &grit_lib::prelude::RepositoryOptions::empty(),
    )
    .expect("reopen");
    let old: Vec<(String, String)> = (0..4)
        .map(|i| (format!("old{i}.txt"), numbered(&format!("f{i}"), None)))
        .collect();
    let new: Vec<(String, String)> = (0..4)
        .map(|i| (format!("new{i}.txt"), numbered(&format!("f{i}"), Some(3))))
        .collect();
    let old_tree = write_tree(&repo, &as_refs(&old));
    let new_tree = write_tree(&repo, &as_refs(&new));
    let result = repo
        .diff()
        .tree_to_tree(Some(&old_tree), Some(&new_tree), &renames())
        .expect("diff");
    let skipped = result
        .inexact_rename_skipped
        .expect("renameLimit=2 skips a 4x4 inexact matrix");
    assert_eq!(skipped.configured_limit, Some(2));
    assert!(result
        .entries
        .iter()
        .all(|e| e.status != DiffStatus::Renamed));
}

#[test]
fn commits_diff_their_trees_and_reject_non_commits() {
    let fx = handle_fixture();
    let diff = fx.repo.diff();
    for options in [TreeDiffOptions::default(), renames()] {
        let via_commits = diff
            .commits(&fx.base_commit, &fx.tip_commit, &options)
            .expect("commits");
        let via_trees = diff
            .tree_to_tree(Some(&fx.base_tree), Some(&fx.tip_tree), &options)
            .expect("trees");
        assert_eq!(via_commits, via_trees);
    }
    let err = diff
        .commits(&fx.base_tree, &fx.tip_commit, &TreeDiffOptions::default())
        .expect_err("a tree is not a commit");
    assert!(
        matches!(err, Error::Ref(RefError::Invalid { .. })),
        "{err:?}"
    );
}

#[test]
fn index_to_tree_renames_pathspec_and_submodules() {
    let fx = handle_fixture();
    let index = fx
        .repo
        .index()
        .load(&grit_lib::index::IndexLoadOptions::default())
        .expect("index");
    let diff = fx.repo.diff();
    let plain = diff
        .index_to_tree(&index, Some(&fx.base_tree), &TreeDiffOptions::default())
        .expect("diff");
    assert_eq!(
        summary(&plain.entries),
        ["A keep-copy.txt", "A moved.txt", "D a.txt", "M dir/b.txt"]
    );
    let renamed = diff
        .index_to_tree(&index, Some(&fx.base_tree), &renames())
        .expect("diff");
    assert_eq!(
        summary(&renamed.entries),
        ["A keep-copy.txt", "M dir/b.txt", "R a.txt -> moved.txt"]
    );
    let harder = renames().with_copies(CopyDetection::FindHarder(50));
    let copied = diff
        .index_to_tree(&index, Some(&fx.base_tree), &harder)
        .expect("diff");
    assert!(summary(&copied.entries).contains(&"C keep.txt -> keep-copy.txt".to_owned()));
    let limited = TreeDiffOptions::default().with_pathspec(Some(pathspec(&["keep*"])));
    let result = diff
        .index_to_tree(&index, Some(&fx.base_tree), &limited)
        .expect("diff");
    assert_eq!(summary(&result.entries), ["A keep-copy.txt"]);
    let clean = diff
        .index_to_tree(&index, Some(&fx.tip_tree), &TreeDiffOptions::default())
        .expect("diff");
    assert!(clean.entries.is_empty());

    let mut with_gitlink = index.clone();
    with_gitlink.add_or_replace(IndexEntry::new(
        b"sub".to_vec(),
        fx.base_commit,
        FileMode::GITLINK,
    ));
    let shown = diff
        .index_to_tree(
            &with_gitlink,
            Some(&fx.tip_tree),
            &TreeDiffOptions::default(),
        )
        .expect("diff");
    assert_eq!(summary(&shown.entries), ["A sub"]);
    let hidden = TreeDiffOptions::default().with_submodules(SubmoduleIgnoreMode::All);
    let result = diff
        .index_to_tree(&with_gitlink, Some(&fx.tip_tree), &hidden)
        .expect("diff");
    assert!(result.entries.is_empty());
}

#[test]
fn index_to_worktree_modifications_pathspec_status_and_refresh() {
    let fx = handle_fixture();
    let work_tree = fx.repo.work_tree().expect("work tree").to_path_buf();
    let diff = fx.repo.diff();
    let mut index = fx
        .repo
        .index()
        .load(&grit_lib::index::IndexLoadOptions::default())
        .expect("index");
    let clean = diff
        .index_to_worktree(&mut index, &TreeDiffOptions::default())
        .expect("diff");
    assert!(clean.entries.is_empty(), "{:?}", clean.entries);

    fs::write(work_tree.join("dir/b.txt"), "b1\nb2 changed again\n").expect("edit");
    fs::remove_file(work_tree.join("moved.txt")).expect("delete");
    let result = diff
        .index_to_worktree(&mut index, &renames())
        .expect("diff");
    assert_eq!(summary(&result.entries), ["D moved.txt", "M dir/b.txt"]);
    let modified = result
        .entries
        .iter()
        .find(|e| e.status == DiffStatus::Modified)
        .expect("modified");
    assert!(modified.new_oid.is_zero(), "work-tree side is not hashed");

    let limited = TreeDiffOptions::default().with_pathspec(Some(pathspec(&["dir"])));
    let result = diff.index_to_worktree(&mut index, &limited).expect("diff");
    assert_eq!(summary(&result.entries), ["M dir/b.txt"]);

    let status = TreeDiffOptions::default().with_for_status(true);
    let result = diff.index_to_worktree(&mut index, &status).expect("diff");
    assert_eq!(summary(&result.entries), ["D moved.txt", "M dir/b.txt"]);

    // Same content, stale stat: only a refreshing scan adopts the new stat data.
    let mut stale = fx
        .repo
        .index()
        .load(&grit_lib::index::IndexLoadOptions::default())
        .expect("index");
    for entry in stale.entries_mut() {
        if entry.path == b"keep.txt" {
            entry.mtime_sec = 1;
        }
    }
    let result = diff
        .index_to_worktree(&mut stale.clone(), &TreeDiffOptions::default())
        .expect("diff");
    assert!(!result.index_stat_refreshed);
    assert!(!summary(&result.entries).contains(&"M keep.txt".to_owned()));
    let refresh = TreeDiffOptions::default().with_refresh_index_stat(true);
    let result = diff.index_to_worktree(&mut stale, &refresh).expect("diff");
    assert!(result.index_stat_refreshed);
    let keep = stale.get(b"keep.txt", 0).expect("keep.txt");
    assert_ne!(keep.mtime_sec, 1, "refreshed stat data is adopted");
}

#[test]
fn worktree_diffs_require_a_work_tree() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, _) =
        Repository::init(dir.path(), &InitOptions::default().with_bare(true)).expect("init bare");
    let mut index = Index::new();
    let err = repo
        .diff()
        .index_to_worktree(&mut index, &TreeDiffOptions::default())
        .expect_err("bare");
    assert!(
        matches!(err, Error::Repository(RepositoryError::RequiresWorkTree)),
        "{err:?}"
    );
    let err = repo
        .diff()
        .tree_to_worktree(None, &index, &TreeDiffOptions::default())
        .expect_err("bare");
    assert!(
        matches!(err, Error::Repository(RepositoryError::RequiresWorkTree)),
        "{err:?}"
    );
}

#[test]
fn tree_to_worktree_pairs_against_the_old_tree() {
    let fx = handle_fixture();
    let work_tree = fx.repo.work_tree().expect("work tree").to_path_buf();
    let index = fx
        .repo
        .index()
        .load(&grit_lib::index::IndexLoadOptions::default())
        .expect("index");
    let diff = fx.repo.diff();
    let clean = diff
        .tree_to_worktree(Some(&fx.tip_tree), &index, &TreeDiffOptions::default())
        .expect("diff");
    assert!(clean.entries.is_empty(), "{:?}", clean.entries);

    let result = diff
        .tree_to_worktree(Some(&fx.base_tree), &index, &renames())
        .expect("diff");
    assert_eq!(
        summary(&result.entries),
        ["A keep-copy.txt", "M dir/b.txt", "R a.txt -> moved.txt"]
    );

    fs::write(work_tree.join("keep.txt"), numbered("keep", Some(1))).expect("edit");
    let limited = TreeDiffOptions::default().with_pathspec(Some(pathspec(&["keep.txt"])));
    let result = diff
        .tree_to_worktree(Some(&fx.tip_tree), &index, &limited)
        .expect("diff");
    assert_eq!(summary(&result.entries), ["M keep.txt"]);

    let broken = TreeDiffOptions::default().with_break_rewrites(BreakRewrites::DEFAULT);
    fs::write(work_tree.join("keep.txt"), numbered("rewritten", None)).expect("rewrite");
    let result = diff
        .tree_to_worktree(Some(&fx.tip_tree), &index, &broken)
        .expect("diff");
    let keep = result
        .entries
        .iter()
        .find(|e| e.path() == "keep.txt")
        .expect("keep.txt");
    let expected = fx
        .repo
        .odb()
        .hash(ObjectKind::Blob, numbered("rewritten", None).as_bytes())
        .expect("hash");
    assert_eq!(keep.new_oid, expected, "-B fills the work-tree blob id");
}

#[test]
fn detect_renames_pairs_entries_listed_elsewhere() {
    let fx = handle_fixture();
    let diff = fx.repo.diff();
    let raw = diff
        .tree_to_tree(
            Some(&fx.base_tree),
            Some(&fx.tip_tree),
            &TreeDiffOptions::default(),
        )
        .expect("raw")
        .entries;
    let paired = diff
        .detect_renames(raw.clone(), None, &renames())
        .expect("renames");
    let direct = diff
        .tree_to_tree(Some(&fx.base_tree), Some(&fx.tip_tree), &renames())
        .expect("direct");
    assert_eq!(summary(&paired.entries), summary(&direct.entries));

    let untouched = diff
        .detect_renames(raw.clone(), None, &TreeDiffOptions::default())
        .expect("no pairing");
    assert_eq!(untouched.entries, raw);

    let harder = renames().with_copies(CopyDetection::FindHarder(50));
    let without_source = diff
        .detect_renames(raw.clone(), None, &harder)
        .expect("copies");
    assert!(summary(&without_source.entries).contains(&"A keep-copy.txt".to_owned()));
    let with_source = diff
        .detect_renames(raw, Some(&fx.base_tree), &harder)
        .expect("copies");
    assert!(summary(&with_source.entries).contains(&"C keep.txt -> keep-copy.txt".to_owned()));
}

#[test]
fn content_methods_agree_on_the_same_entries() {
    let fx = handle_fixture();
    let diff = fx.repo.diff();
    let entries = diff
        .tree_to_tree(Some(&fx.base_tree), Some(&fx.tip_tree), &renames())
        .expect("entries")
        .entries;
    let options = DiffOptions::default().with_renames(RenameDetection::Off);

    let patch = diff
        .files_for_entries(entries.clone(), DiffDetail::Patch, &options)
        .expect("patch");
    assert_eq!(patch.files.len(), entries.len());
    let names = diff
        .files_for_entries(entries.clone(), DiffDetail::NameStatus, &options)
        .expect("names");
    assert_eq!(names.files.len(), entries.len());
    assert!(names.files.iter().all(|f| f.structured.is_none()));

    let mut streamed = Vec::new();
    diff.for_each_file(entries.clone(), &options, |file| {
        streamed.push(file);
        Ok(())
    })
    .expect("stream");
    assert_eq!(streamed.len(), patch.files.len());
    for (a, b) in streamed.iter().zip(&patch.files) {
        assert_eq!(a.path, b.path);
        assert_eq!(a.structured, b.structured);
    }

    let structured = diff
        .structured(entries.clone(), &options)
        .expect("structured");
    let mut visited = Vec::new();
    diff.for_each_structured(&entries, &options, |row| {
        visited.push(row);
        Ok(())
    })
    .expect("visit");
    assert_eq!(structured.len(), entries.len());
    assert_eq!(
        visited.iter().map(|r| r.entry.clone()).collect::<Vec<_>>(),
        structured
            .iter()
            .map(|r| r.entry.clone())
            .collect::<Vec<_>>()
    );

    let stat = diff.stat(entries.clone(), &options).expect("stat");
    let stat_files = diff
        .files_for_entries(entries.clone(), DiffDetail::Stat, &options)
        .expect("stat files");
    assert_eq!(Some(stat.totals()), stat_files.totals);
    assert_eq!(stat.totals().files_changed, entries.len());
    assert!(stat.totals().insertions >= 2);

    let changes = diff.object_changes(&entries).expect("changes");
    assert_eq!(changes.len(), entries.len());

    // Content methods pair renames themselves when asked to.
    let raw = diff
        .tree_to_tree(
            Some(&fx.base_tree),
            Some(&fx.tip_tree),
            &TreeDiffOptions::default(),
        )
        .expect("raw")
        .entries;
    let paired = diff
        .files_for_entries(raw, DiffDetail::NameStatus, &DiffOptions::default())
        .expect("paired");
    assert!(paired.files.iter().any(|f| f.status == DiffStatus::Renamed));
}

#[test]
fn commit_files_resolve_request_and_has_changes() {
    let fx = handle_fixture();
    let diff = fx.repo.diff();
    let options = DiffOptions::default();
    let shown = diff
        .commit_files(
            &fx.tip_commit,
            DiffMerges::FirstParent,
            DiffDetail::NameStatus,
            &options,
            &every_path(),
        )
        .expect("commit files");
    assert!(shown.files.iter().any(|f| f.status == DiffStatus::Renamed));
    let limited = diff
        .commit_files(
            &fx.tip_commit,
            DiffMerges::FirstParent,
            DiffDetail::NameStatus,
            &options,
            &pathspec(&["dir"]),
        )
        .expect("limited");
    assert_eq!(limited.files.len(), 1);
    let root = diff
        .commit_files(
            &fx.base_commit,
            DiffMerges::FirstParent,
            DiffDetail::NameOnly,
            &options,
            &every_path(),
        )
        .expect("root commit");
    assert_eq!(root.files.len(), base_files().len());

    let request = DiffRequest {
        positionals: vec![fx.base_commit.to_hex(), fx.tip_commit.to_hex()],
        paths: vec!["dir".to_owned()],
        ..DiffRequest::default()
    };
    let args = diff.resolve_request(None, &request).expect("request");
    assert!(
        matches!(args.target, DiffTarget::Trees { .. }),
        "{:?}",
        args.target
    );
    assert_eq!(args.pathspec.specs(), ["dir"]);
    let staged_and_unstaged = DiffRequest {
        staged: true,
        unstaged: true,
        ..DiffRequest::default()
    };
    assert!(diff.resolve_request(None, &staged_and_unstaged).is_err());

    let spec = diff
        .resolve_pathspec(None, &["dir".to_owned()])
        .expect("pathspec");
    assert_eq!(spec.specs(), ["dir"]);

    let target = args.target;
    assert!(diff
        .has_changes(&target, &args.pathspec, &options)
        .expect("dir"));
    assert!(!diff
        .has_changes(&target, &pathspec(&["missing"]), &options)
        .expect("missing"));
    assert!(!diff
        .has_changes(&DiffTarget::WorktreeVsIndex, &every_path(), &options)
        .expect("clean"));
}

#[test]
fn hunk_bundle_lists_unstaged_hunks() {
    let fx = handle_fixture();
    let work_tree = fx.repo.work_tree().expect("work tree").to_path_buf();
    fs::write(work_tree.join("dir/b.txt"), "b1\nb2 changed\nb3\n").expect("edit");
    let bundle = fx
        .repo
        .diff()
        .hunk_bundle(&pathspec(&["dir/b.txt"]), &DiffOptions::default())
        .expect("bundle");
    assert_eq!(bundle.unstaged.len(), 1);
    assert_eq!(bundle.unstaged[0].path, "dir/b.txt");
}
