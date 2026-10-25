//! In-process L/H worktree and index fixtures for hot-path Criterion benches.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::Path;
use std::sync::OnceLock;

use grit_lib::diff::merge_file::MergeFavor;
use grit_lib::diff::merge_trees::{
    merge_trees_three_way, TreeMergeConflictPresentation, WhitespaceMergeOptions,
};
use grit_lib::diff::{diff_index_to_worktree_with_options, DiffIndexToWorktreeOptions};
use grit_lib::error::Result;
use grit_lib::index::write_tree::{write_tree_update_index, WriteTreeFlags};
use grit_lib::index::{entry_from_stat, Index, IndexEntry, MODE_REGULAR};
use grit_lib::objects::{serialize_commit, CommitData, ObjectId, ObjectKind};
use grit_lib::porcelain::checkout::checkout_between_trees;
use grit_lib::porcelain::reset::{reset_to_commit, ResetMode, ResetOptions};
use grit_lib::porcelain::staging::stage_worktree_changes;
use grit_lib::porcelain::status::{status, StatusOptions};
use grit_lib::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};
use grit_lib::progress::NeverCancel;
use grit_lib::progress::NullProgress;
use grit_lib::refs;
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::stash::{StashApplyOptions, StashMode, StashPushOptions};
use tempfile::TempDir;

const BENCH_IDENT: &str = "Bench <bench@grit-scm.test> 1700000000 +0000";

/// Large shape: 10_000 files across 100 directories.
pub const L_FILES: usize = 10_000;
pub const L_DIRS: usize = 100;

/// Heavy shape: 100_000 files across 1_000 directories.
pub const H_FILES: usize = 100_000;
pub const H_DIRS: usize = 1_000;

struct IndexMutatePlan {
    baseline: Index,
    remove_paths: Vec<Vec<u8>>,
    replacement_entries: Vec<IndexEntry>,
}

pub struct HotPathsFixture {
    pub _dir: TempDir,
    pub repo: Repository,
    pub head_tree: ObjectId,
    pub main_head_oid: ObjectId,
    pub wide_switch_tip: ObjectId,
    pub file_paths: Vec<String>,
    index_mutate_plan: IndexMutatePlan,
}

impl HotPathsFixture {
    fn build(file_count: usize, dir_count: usize) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(
            dir.path(),
            &InitOptions::new()
                .with_initial_branch("main".to_owned())
                .with_ref_storage(grit_lib::RefStorageFormat::Files),
        )
        .expect("init hot-path bench repo");
        populate_worktree(dir.path(), file_count, dir_count);
        let file_paths = list_txt_paths(dir.path());
        let head_tree = commit_all(&repo, "initial").expect("initial commit");
        let main_head_oid = refs::resolve_ref(repo.git_dir(), "HEAD").expect("HEAD");
        let wide_switch_tip = prepare_wide_switch_tip(&repo, &file_paths, head_tree, main_head_oid);
        hard_reset_to_main(&repo, main_head_oid);
        let index_mutate_plan = build_index_mutate_plan(&repo);
        Self {
            _dir: dir,
            repo,
            head_tree,
            main_head_oid,
            wide_switch_tip,
            file_paths,
            index_mutate_plan,
        }
    }

    pub fn large() -> &'static Self {
        static FIX: OnceLock<HotPathsFixture> = OnceLock::new();
        FIX.get_or_init(|| HotPathsFixture::build(L_FILES, L_DIRS))
    }

    pub fn heavy() -> &'static Self {
        static FIX: OnceLock<HotPathsFixture> = OnceLock::new();
        FIX.get_or_init(|| HotPathsFixture::build(H_FILES, H_DIRS))
    }

    /// Tree with `fraction` of paths changed relative to [`Self::head_tree`].
    pub fn tree_with_fraction_changed(&self, fraction: f64) -> ObjectId {
        let n = ((self.file_paths.len() as f64) * fraction).round() as usize;
        let n = n.max(1);
        let work_tree = self.repo.work_tree().expect("work tree");
        let mut index = self.repo.index_handle().load().expect("load index");
        for path in self.file_paths.iter().take(n) {
            let abs = work_tree.join(path);
            std::fs::write(&abs, format!("changed-{path}\n")).expect("write");
            let data = std::fs::read(&abs).expect("read");
            let oid = self
                .repo
                .odb()
                .write(ObjectKind::Blob, &data)
                .expect("blob");
            let entry = entry_from_stat(&abs, path.as_bytes(), oid, MODE_REGULAR).expect("entry");
            index.add_or_replace(entry);
        }
        index.sort();
        write_tree_update_index(self.repo.odb(), &mut index, "", WriteTreeFlags::silent())
            .expect("tree")
    }

    /// Apply a precomputed 10% remove/replace batch (index operations only).
    pub fn apply_index_mutate_batch(&self) -> Index {
        self.apply_index_mutate_batch_inner(false)
    }

    /// Same batch with an `FSMN` token present (lazy dirty-bitmap invalidation on each mutation).
    pub fn apply_index_mutate_batch_fsmn(&self) -> Index {
        self.apply_index_mutate_batch_inner(true)
    }

    fn apply_index_mutate_batch_inner(&self, with_fsmn: bool) -> Index {
        let mut index = self.index_mutate_plan.baseline.clone();
        if with_fsmn {
            index.set_fsmonitor_last_update(Some("bench-fsmn-token".into()));
            for entry in index.entries_mut() {
                entry.set_fsmonitor_valid(true);
            }
        }
        index.remove_paths_and_insert(
            self.index_mutate_plan
                .remove_paths
                .iter()
                .map(|p| p.as_slice()),
            self.index_mutate_plan.replacement_entries.iter().cloned(),
        );
        index
    }

    pub fn modify_worktree_files(&self, count: usize) {
        let work_tree = self.repo.work_tree().expect("work tree");
        for path in self.file_paths.iter().take(count) {
            std::fs::write(work_tree.join(path), format!("modified-{path}\n")).expect("write");
        }
    }

    pub fn reset_worktree_to_head(&self) {
        self.reset_for_switch_benchmark();
    }

    /// Restore symbolic `HEAD` on `main`, index, and worktree to [`Self::head_tree`].
    pub fn reset_for_switch_benchmark(&self) {
        hard_reset_to_main(&self.repo, self.main_head_oid);
    }

    /// Clean tracked tree: index stat cache matches the worktree (for status/diff benches).
    pub fn ensure_clean_stat_cache(&self) {
        self.reset_worktree_to_head();
        let mut index = self.repo.index_handle().load().expect("load index");
        let work_tree = self.repo.work_tree().expect("work tree");
        let diff_index_mtime = grit_lib::diff::index_mtime_for_diff(&index, index.source_mtime);
        let _ = diff_index_to_worktree_with_options(
            self.repo.odb(),
            &mut index,
            work_tree,
            DiffIndexToWorktreeOptions {
                index_mtime: diff_index_mtime,
                refresh_index_stat_in_pass: true,
                for_status: true,
                repository_git_dir: Some(self.repo.git_dir().to_path_buf()),
                config: self.repo.config().ok(),
                thread_policy: Some(self.repo.thread_policy()),
                ..DiffIndexToWorktreeOptions::default()
            },
        )
        .expect("refresh index stat cache");
        let path = self.repo.index_handle().path();
        index.write(&path).expect("write refreshed index");
    }

    pub fn bench_status_clean(&self) {
        let opts = StatusOptions {
            untracked: grit_lib::porcelain::status::UntrackedMode::No,
            ahead_behind: false,
            ..StatusOptions::default()
        };
        let _ = status(&self.repo, &opts, &mut NullProgress, &NeverCancel).expect("status clean");
    }

    pub fn bench_diff_worktree_clean(&self) {
        let work_tree = self.repo.work_tree().expect("work tree");
        let mut index = self.repo.index_handle().load().expect("load index");
        let index_mtime = grit_lib::diff::index_mtime_for_diff(&index, index.source_mtime);
        let _ = diff_index_to_worktree_with_options(
            self.repo.odb(),
            &mut index,
            work_tree,
            DiffIndexToWorktreeOptions {
                index_mtime,
                refresh_index_stat_in_pass: false,
                for_status: true,
                repository_git_dir: Some(self.repo.git_dir().to_path_buf()),
                config: self.repo.config().ok(),
                thread_policy: Some(self.repo.thread_policy()),
                ..DiffIndexToWorktreeOptions::default()
            },
        )
        .expect("diff index worktree clean");
    }

    /// Stash commit whose worktree parent differs on `fraction` of tracked paths (index matches HEAD).
    pub fn build_stash_touching_fraction(&self, fraction: f64) -> ObjectId {
        self.reset_worktree_to_head();
        let stash_tree = self.tree_with_fraction_changed(fraction);
        let head_oid = refs::resolve_ref(self.repo.git_dir(), "HEAD").expect("head");
        let index_parent = write_tree_commit(
            &self.repo,
            self.head_tree,
            &[head_oid],
            "stash index parent",
        );
        write_tree_commit(
            &self.repo,
            stash_tree,
            &[head_oid, index_parent],
            "WIP on main: bench stash",
        )
    }

    /// `(base_tree, head_tree, source_tree)` for a single-parent pick changing `path_count` paths.
    pub fn pick_three_trees(&self, path_count: usize) -> (ObjectId, ObjectId, ObjectId) {
        self.reset_worktree_to_head();
        let head_tree = self.head_tree;
        let head_oid = refs::resolve_ref(self.repo.git_dir(), "HEAD").expect("head");
        let source_tree = self.tree_with_path_count_changed(path_count);
        let _source_commit = write_tree_commit(&self.repo, source_tree, &[head_oid], "pick source");
        (head_tree, head_tree, source_tree)
    }

    fn tree_with_path_count_changed(&self, path_count: usize) -> ObjectId {
        let n = path_count.min(self.file_paths.len()).max(1);
        let work_tree = self.repo.work_tree().expect("work tree");
        let mut index = self.repo.index_handle().load().expect("load index");
        for path in self.file_paths.iter().take(n) {
            let abs = work_tree.join(path);
            std::fs::write(&abs, format!("pick-{path}\n")).expect("write");
            let data = std::fs::read(&abs).expect("read");
            let oid = self
                .repo
                .odb()
                .write(ObjectKind::Blob, &data)
                .expect("blob");
            let entry = entry_from_stat(&abs, path.as_bytes(), oid, MODE_REGULAR).expect("entry");
            index.add_or_replace(entry);
        }
        index.sort();
        write_tree_update_index(self.repo.odb(), &mut index, "", WriteTreeFlags::silent())
            .expect("tree")
    }
}

fn build_index_mutate_plan(repo: &Repository) -> IndexMutatePlan {
    const REPLACEMENT: &[u8] = b"bench-index-replacement\n";
    let baseline = repo
        .index_handle()
        .load()
        .expect("load index for mutate plan");
    let n = ((baseline.entries().len() as f64) * 0.10).round() as usize;
    let n = n.max(1);
    let replace_oid = repo
        .odb()
        .write(ObjectKind::Blob, REPLACEMENT)
        .expect("replacement blob");
    let touched: Vec<IndexEntry> = baseline.entries().iter().take(n).cloned().collect();
    let remove_paths: Vec<Vec<u8>> = touched.iter().map(|e| e.path.clone()).collect();
    let replacement_entries: Vec<IndexEntry> = touched
        .into_iter()
        .map(|mut entry| {
            entry.oid = replace_oid;
            entry.size = REPLACEMENT.len() as u32;
            entry
        })
        .collect();
    IndexMutatePlan {
        baseline,
        remove_paths,
        replacement_entries,
    }
}

fn populate_worktree(root: &Path, file_count: usize, dir_count: usize) {
    let files_per_dir = file_count.div_ceil(dir_count.max(1));
    let mut created = 0usize;
    for d in 0..dir_count.max(1) {
        let sub = root.join(format!("d{d:04}"));
        std::fs::create_dir_all(&sub).expect("mkdir");
        for f in 0..files_per_dir {
            if created >= file_count {
                return;
            }
            std::fs::write(sub.join(format!("f{f:04}.txt")), format!("seed {d}/{f}\n"))
                .expect("write file");
            created += 1;
        }
    }
}

fn list_txt_paths(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    walk_txt(root, root, &mut out);
    out.sort();
    out
}

fn walk_txt(base: &Path, dir: &Path, out: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).expect("read_dir") {
        let entry = entry.expect("entry");
        let path = entry.path();
        if path.file_name().is_some_and(|n| n == ".git") {
            continue;
        }
        if path.is_dir() {
            walk_txt(base, &path, out);
        } else if path.extension().is_some_and(|e| e == "txt") {
            out.push(
                path.strip_prefix(base)
                    .expect("strip")
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
}

fn commit_all(repo: &Repository, message: &str) -> Result<ObjectId> {
    let work_tree = repo.work_tree().expect("work tree");
    let mut index = repo.index_handle().load()?;
    for rel in list_txt_paths(work_tree) {
        let abs = work_tree.join(&rel);
        let data = std::fs::read(&abs)?;
        let oid = repo.odb().write(ObjectKind::Blob, &data)?;
        let entry = entry_from_stat(&abs, rel.as_bytes(), oid, MODE_REGULAR)?;
        index.add_or_replace(entry);
    }
    index.sort();
    let tree = write_tree_update_index(repo.odb(), &mut index, "", WriteTreeFlags::silent())?;
    repo.index_handle().write(&mut index)?;
    let parent = refs::resolve_ref(repo.git_dir(), "HEAD").ok();
    let commit_data = CommitData {
        tree,
        parents: parent.into_iter().collect(),
        author: BENCH_IDENT.to_owned(),
        committer: BENCH_IDENT.to_owned(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: format!("{message}\n"),
        raw_message: None,
        extra_headers: Vec::new(),
    };
    let bytes = serialize_commit(&commit_data);
    let oid = repo.odb().write(ObjectKind::Commit, &bytes)?;
    refs::write_ref(repo.git_dir(), "refs/heads/main", &oid)?;
    refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/main")?;
    Ok(tree)
}

pub fn bench_checkout_between_trees(repo: &Repository, from: &ObjectId, to: &ObjectId) {
    checkout_between_trees(repo, Some(from), to).expect("checkout_between_trees");
}

fn hard_reset_to_main(repo: &Repository, main_head_oid: ObjectId) {
    reset_to_commit(
        repo,
        &ResetOptions::new(
            main_head_oid,
            ResetMode::Hard,
            BENCH_IDENT,
            "bench: reset to main",
        ),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("hard reset to main");
}

fn prepare_wide_switch_tip(
    repo: &Repository,
    file_paths: &[String],
    head_tree: ObjectId,
    main_head_oid: ObjectId,
) -> ObjectId {
    let wide_tree = wide_tree_from_fraction(repo, file_paths, 0.10);
    checkout_between_trees(repo, Some(&head_tree), &wide_tree).expect("apply wide tree");
    let tip = write_tree_commit(repo, wide_tree, &[main_head_oid], "bench-wide switch");
    refs::write_ref(repo.git_dir(), "refs/heads/bench-wide", &tip).expect("wide branch ref");
    hard_reset_to_main(repo, main_head_oid);
    tip
}

fn wide_tree_from_fraction(repo: &Repository, file_paths: &[String], fraction: f64) -> ObjectId {
    let n = ((file_paths.len() as f64) * fraction).round() as usize;
    let n = n.max(1);
    let mut index = repo.index_handle().load().expect("load index");
    for path in file_paths.iter().take(n) {
        let data = format!("changed-{path}\n");
        let oid = repo
            .odb()
            .write(ObjectKind::Blob, data.as_bytes())
            .expect("blob");
        let mut entry = index
            .get(path.as_bytes(), 0)
            .cloned()
            .expect("index entry for wide tree path");
        entry.oid = oid;
        index.add_or_replace(entry);
    }
    index.sort();
    write_tree_update_index(repo.odb(), &mut index, "", WriteTreeFlags::silent()).expect("tree")
}

pub fn bench_switch_wide(fx: &HotPathsFixture) {
    switch_repository(
        &fx.repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "bench-wide".into(),
                oid: fx.wide_switch_tip,
            },
            dry_run: false,
            hooks: grit_lib::PostOperationHooks::Run,
        },
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("switch wide");
    switch_repository(
        &fx.repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "main".into(),
                oid: fx.main_head_oid,
            },
            dry_run: false,
            hooks: grit_lib::PostOperationHooks::Run,
        },
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("switch main");
}

pub fn bench_stage_scan(repo: &Repository) {
    let _ = stage_worktree_changes(repo, &[]).expect("stage scan");
}

fn write_tree_commit(
    repo: &Repository,
    tree: ObjectId,
    parents: &[ObjectId],
    message: &str,
) -> ObjectId {
    let commit_data = CommitData {
        tree,
        parents: parents.to_vec(),
        author: BENCH_IDENT.to_owned(),
        committer: BENCH_IDENT.to_owned(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: format!("{message}\n"),
        raw_message: None,
        extra_headers: Vec::new(),
    };
    let bytes = serialize_commit(&commit_data);
    repo.odb()
        .write(ObjectKind::Commit, &bytes)
        .expect("write commit")
}

pub fn bench_push_stash(fx: &HotPathsFixture) {
    fx.modify_worktree_files(50);
    let options = StashPushOptions {
        message: None,
        include_untracked: false,
        identity: BENCH_IDENT.parse().expect("signature"),
        selection: None,
        pathspecs: Vec::new(),
        dry_run: false,
        mode: StashMode::default(),
    };
    let _ = fx.repo.stash().push(&options).expect("push_stash");
}

pub fn bench_apply_stash(fx: &HotPathsFixture, stash_oid: &ObjectId) {
    let wt = fx.repo.work_tree().expect("work tree");
    let _ = fx
        .repo
        .stash()
        .apply_commit(&stash_oid, &StashApplyOptions::default())
        .expect("apply_stash");
}

pub fn bench_pick_path(fx: &HotPathsFixture, path_count: usize) {
    let (base, head, source) = fx.pick_three_trees(path_count);
    let merged = merge_trees_three_way(
        &fx.repo,
        base,
        head,
        source,
        MergeFavor::default(),
        WhitespaceMergeOptions::default(),
        None,
        TreeMergeConflictPresentation::default(),
    )
    .expect("merge");
    let new_tree = merged.result_tree;
    let index_handle = fx.repo.index_handle();
    let mut index = index_handle.load().expect("index");
    grit_lib::diff::merge_trees::apply_tree_merge_output_to_index(&mut index, &merged);
    index_handle.write(&mut index).expect("write index");
    checkout_between_trees(&fx.repo, Some(&head), &new_tree).expect("checkout");
}

pub fn bench_tree_merge_100k(fx: &HotPathsFixture, path_count: usize) {
    let (base, head, source) = fx.pick_three_trees(path_count);
    let _merged = merge_trees_three_way(
        &fx.repo,
        base,
        head,
        source,
        MergeFavor::default(),
        WhitespaceMergeOptions::default(),
        None,
        TreeMergeConflictPresentation::default(),
    )
    .expect("merge");
}
