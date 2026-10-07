//! Stage working-tree changes into the index (`git add`).
//!
//! [`stage`] performs a single index-vs-worktree scan (plus an untracked walk when
//! [`StageMode::All`] is selected). It does **not** compute the index-vs-HEAD diff
//! that full [`super::status::status`] requires.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::Path;

use crate::config::ConfigSet;
use crate::crlf;
use crate::diff::{
    materialize_worktree_blob, mode_from_metadata, read_submodule_head_oid, DiffEntry,
    DiffIndexToWorktreeOptions, DiffStatus,
};
use crate::error::{Error, Result};
use crate::hash::{
    index_parallelism_from_config, try_par_hash_with, ParallelHashError, Parallelism,
};
use crate::index::{
    entry_from_metadata, index_file_mtime, Index, IndexEntry, MODE_GITLINK, MODE_TREE,
};
use crate::objects::{parse_commit, parse_tree, ObjectId};
use crate::odb::WriteOptions;
use crate::path_icase::{paths_equal, worktree_path_for_index_entry, Stage0IcasePathMap};
use crate::pathspec::{has_glob_chars, matches_pathspec_list, pathspec_is_exclude};
use crate::porcelain::status::{collect_untracked_and_ignored, IgnoredMode};
use crate::progress::ProgressSink;
use crate::repo::Repository;
use crate::state::resolve_head;
use crate::unicode_normalization::resolve_worktree_path_for_staging;
use crate::worktree_batch::{prepare_worktree_blobs_parallel, WorktreeBlobReadInput};

/// What paths [`stage`] should update (`git add` vs `git add -u`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StageMode {
    /// Stage tracked changes and untracked files (default `git add`).
    #[default]
    All,
    /// Stage tracked changes only (`git add -u` / `--update`); untracked paths are ignored.
    Update,
}

/// Inputs for [`stage`].
///
/// Pathspec strings must already be resolved against the work tree (the CLI performs
/// `resolve_pathspec_in_worktree`). When `pathspec_sources` is non-empty it must align
/// with `pathspecs` and supplies the user-facing spelling for errors and exclude magic.
#[derive(Debug, Clone, Default)]
pub struct StageOptions {
    /// Resolved pathspecs used for matching (empty = entire tree).
    pub pathspecs: Vec<String>,
    /// Original pathspec argv fragments; defaults to `pathspecs` when empty.
    pub pathspec_sources: Vec<String>,
    /// Whether to include untracked paths ([`StageMode::All`]) or only tracked changes ([`StageMode::Update`]).
    pub mode: StageMode,
}

/// Counts returned by [`stage`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StageOutcome {
    /// New untracked paths staged at stage 0.
    pub added: usize,
    /// Tracked paths updated in the index (including conflict resolution and gitlinks).
    pub modified: usize,
    /// Tracked paths removed from the index because they are absent from the work tree.
    pub removed: usize,
}

impl StageOutcome {
    /// Total number of index paths updated.
    #[must_use]
    pub fn total(&self) -> usize {
        self.added
            .saturating_add(self.modified)
            .saturating_add(self.removed)
    }
}

/// Stage paths matching `opts` into the index.
///
/// Loads the index, compares it to the work tree once, hashes only changed or new
/// blobs, removes deleted paths, and invalidates cache-tree / untracked-cache entries
/// for touched paths via [`Index::stage_file`] / [`Index::remove`].
///
/// # Errors
///
/// Returns [`Error::Message`] when an explicit pathspec matches nothing, or I/O /
/// object-store failures while reading the work tree.
pub fn stage(
    repo: &Repository,
    opts: &StageOptions,
    progress: &mut dyn ProgressSink,
) -> Result<StageOutcome> {
    let work_tree = repo
        .work_tree
        .as_deref()
        .ok_or_else(|| Error::Message("this operation must be run in a work tree".into()))?;

    progress.start("stage", None);

    let index_path = repo.index_path();
    let index_mtime = index_file_mtime(&index_path);
    let mut index = repo.load_index()?;

    let convert = StagingConvertContext::load(repo, work_tree);
    let parallelism = index_parallelism_from_config(&convert.config);
    let ignorecase = convert
        .config
        .get_bool("core.ignorecase")
        .and_then(|r| r.ok())
        .unwrap_or(false);
    let mut icase_map = Stage0IcasePathMap::from_index(&index, ignorecase);

    let repo_config = repo.config().ok();
    let unstaged = if index.entries.is_empty() {
        Vec::new()
    } else {
        let diff_opts = DiffIndexToWorktreeOptions {
            index_mtime,
            repository_git_dir: Some(repo.git_dir.clone()),
            config: repo_config.clone(),
            ..DiffIndexToWorktreeOptions::default()
        };
        let (unstaged, _) = crate::diff::diff_index_to_worktree_for_staging(
            &repo.odb,
            &repo.git_dir,
            &mut index,
            work_tree,
            diff_opts,
        )?;
        unstaged
    };

    let bulk_empty_index_add =
        opts.mode == StageMode::All && index.entries.is_empty() && opts.pathspecs.is_empty();
    let untracked = if opts.mode == StageMode::All {
        let (untracked, _) = if bulk_empty_index_add {
            crate::porcelain::status::collect_untracked_and_ignored_inner(
                repo,
                &index,
                work_tree,
                IgnoredMode::No,
                true,
                &opts.pathspecs,
                false,
            )?
        } else {
            collect_untracked_and_ignored(
                repo,
                &index,
                work_tree,
                IgnoredMode::No,
                true,
                &opts.pathspecs,
            )?
        };
        untracked
    } else {
        Vec::new()
    };

    if !opts.pathspecs.is_empty() {
        validate_pathspecs(repo, &index, work_tree, &unstaged, &untracked, opts)?;
    }

    let matches =
        |path: &str| opts.pathspecs.is_empty() || matches_pathspec_list(path, &opts.pathspecs);

    let indexed_any_stage: HashSet<Vec<u8>> =
        index.entries.iter().map(|e| e.path.clone()).collect();

    let mut outcome = StageOutcome::default();

    let worktree_updates = collect_tracked_stage_plans(&unstaged, &matches);
    for (path, plan) in worktree_updates {
        if plan.remove {
            if index.remove(path.as_bytes()) {
                outcome.removed += 1;
            }
            continue;
        }
        icase_map.remove_alias_of(&mut index, path.as_bytes());
        apply_tracked_stage_plan(repo, work_tree, &path, &plan, &mut index)?;
        outcome.modified += 1;
    }

    if ignorecase {
        outcome.modified +=
            stage_ignorecase_spelling_updates(work_tree, &icase_map, &matches, &mut index)?;
    }

    stage_untracked_paths_parallel(
        repo,
        work_tree,
        &untracked,
        &matches,
        &indexed_any_stage,
        &convert,
        &mut icase_map,
        &mut index,
        parallelism,
        &mut outcome,
    )?;

    if outcome.total() > 0 {
        if !bulk_empty_index_add {
            index.sort();
        }
        repo.write_index(&mut index)?;
    }

    progress.finish();
    Ok(outcome)
}

struct TrackedStagePlan {
    remove: bool,
    oid: ObjectId,
    mode: u32,
    gitlink: bool,
}

struct StagingConvertContext {
    config: ConfigSet,
    conv: crlf::ConversionConfig,
    attrs: crlf::GitAttributes,
}

impl StagingConvertContext {
    fn load(repo: &Repository, work_tree: &Path) -> Self {
        let config = repo.config().map(|c| (*c).clone()).unwrap_or_default();
        let conv = crlf::ConversionConfig::from_config(&config);
        let attrs = crlf::load_gitattributes(work_tree);
        Self {
            config,
            conv,
            attrs,
        }
    }
}

/// Merge unstaged diff rows into one staging action per path (unmerged paths may appear twice).
fn collect_tracked_stage_plans(
    unstaged: &[DiffEntry],
    matches: &impl Fn(&str) -> bool,
) -> BTreeMap<String, TrackedStagePlan> {
    let mut changes = BTreeMap::<String, TrackedStagePlan>::new();
    for entry in unstaged {
        let path = entry.path();
        if !matches(path) {
            continue;
        }
        merge_tracked_stage_plan(&mut changes, path.to_owned(), entry);
    }
    changes
}

fn merge_tracked_stage_plan(
    changes: &mut BTreeMap<String, TrackedStagePlan>,
    path: String,
    entry: &DiffEntry,
) {
    match entry.status {
        DiffStatus::Deleted => {
            changes.insert(
                path,
                TrackedStagePlan {
                    remove: true,
                    oid: ObjectId::zero(),
                    mode: 0,
                    gitlink: false,
                },
            );
        }
        DiffStatus::Modified | DiffStatus::TypeChanged | DiffStatus::Added => {
            let mode = mode_from_diff_octal(&entry.new_mode).unwrap_or(0);
            changes.insert(
                path,
                TrackedStagePlan {
                    remove: false,
                    oid: entry.new_oid,
                    mode,
                    gitlink: mode == MODE_GITLINK,
                },
            );
        }
        DiffStatus::Unmerged if entry.new_mode != "000000" => {
            let mode = mode_from_diff_octal(&entry.new_mode).unwrap_or(0);
            changes.entry(path).or_insert(TrackedStagePlan {
                remove: false,
                oid: entry.new_oid,
                mode,
                gitlink: mode == MODE_GITLINK,
            });
        }
        _ => {}
    }
}

fn mode_from_diff_octal(mode: &str) -> Result<u32> {
    if mode.is_empty() || mode == "000000" {
        return Ok(0);
    }
    u32::from_str_radix(mode, 8).map_err(|e| Error::Message(format!("invalid mode '{mode}': {e}")))
}

fn apply_tracked_stage_plan(
    _repo: &Repository,
    work_tree: &Path,
    rel_path: &str,
    plan: &TrackedStagePlan,
    index: &mut Index,
) -> Result<()> {
    let abs = work_tree.join(rel_path);
    let meta = fs::symlink_metadata(&abs).map_err(|e| {
        Error::Io(std::io::Error::new(
            e.kind(),
            format!("could not read {rel_path}: {e}"),
        ))
    })?;

    if plan.gitlink {
        let oid = if plan.oid.is_zero() {
            read_submodule_head_oid(&abs).ok_or_else(|| {
                Error::Message(format!("could not resolve submodule HEAD for '{rel_path}'"))
            })?
        } else {
            plan.oid
        };
        return stage_gitlink_at(rel_path, &meta, oid, index);
    }

    let oid = plan.oid;
    let mut entry = entry_from_metadata(&meta, rel_path.as_bytes(), oid, plan.mode);
    entry.mode = plan.mode;
    index.stage_file(entry);
    mark_fsmonitor_staged(index, rel_path);
    Ok(())
}

/// Refresh index path spellings when the work tree uses a different ASCII case (ignorecase FS).
fn stage_ignorecase_spelling_updates(
    work_tree: &Path,
    icase_map: &Stage0IcasePathMap,
    matches: &impl Fn(&str) -> bool,
    index: &mut Index,
) -> Result<usize> {
    let mut updated = 0usize;
    let stage0: Vec<Vec<u8>> = index
        .entries
        .iter()
        .filter(|e| e.stage() == 0 && e.mode != MODE_GITLINK && e.mode != MODE_TREE)
        .map(|e| e.path.clone())
        .collect();

    for raw_path in stage0 {
        let index_rel = match std::str::from_utf8(&raw_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if !matches(index_rel) {
            continue;
        }
        let abs = worktree_path_for_index_entry(work_tree, index_rel, true);
        let Ok(rel) = abs.strip_prefix(work_tree) else {
            continue;
        };
        let actual_rel = rel.to_string_lossy().replace('\\', "/");
        if actual_rel == index_rel {
            continue;
        }
        if !paths_equal(raw_path.as_slice(), actual_rel.as_bytes(), true) {
            continue;
        }
        let Some(entry) = index
            .entries
            .iter()
            .find(|e| e.path == raw_path && e.stage() == 0)
            .cloned()
        else {
            continue;
        };
        icase_map.remove_alias_of(index, actual_rel.as_bytes());
        let mut refreshed = entry;
        refreshed.path = actual_rel.as_bytes().to_vec();
        index.stage_file(refreshed);
        mark_fsmonitor_staged(index, &actual_rel);
        updated += 1;
    }
    Ok(updated)
}

#[allow(clippy::too_many_arguments)]
fn stage_untracked_paths_parallel(
    repo: &Repository,
    work_tree: &Path,
    untracked: &[String],
    matches: &impl Fn(&str) -> bool,
    indexed_any_stage: &HashSet<Vec<u8>>,
    ctx: &StagingConvertContext,
    icase_map: &mut Stage0IcasePathMap,
    index: &mut Index,
    parallelism: Parallelism,
    outcome: &mut StageOutcome,
) -> Result<()> {
    let precompose_unicode = ctx
        .config
        .get_bool("core.precomposeunicode")
        .and_then(|r| r.ok())
        .unwrap_or(false);

    let mut batch_inputs: Vec<WorktreeBlobReadInput> = Vec::new();
    let mut deferred: Vec<String> = Vec::new();

    for path in untracked {
        if !matches(path) {
            continue;
        }
        if indexed_any_stage.contains(path.as_bytes()) {
            continue;
        }
        let resolved = resolve_worktree_path_for_staging(work_tree, path, precompose_unicode);
        batch_inputs.push(WorktreeBlobReadInput {
            abs: resolved.abs,
            index_relpath: resolved.index_relpath,
            index_entry: None,
        });
    }

    let prepared = prepare_worktree_blobs_parallel(
        &repo.odb,
        &batch_inputs,
        &ctx.conv,
        &ctx.attrs,
        &ctx.config,
        parallelism,
    )?;

    if !prepared.is_empty() {
        repo.odb.ensure_all_loose_prefix_dirs()?;
    }
    let write_opts = WriteOptions {
        assume_loose_only_existence: true,
        trust_new_loose: true,
        ..WriteOptions::default()
    };
    let write_bytes: usize = prepared.iter().map(|p| p.zlib_store.len()).sum();
    let threads = parallelism.threads();
    let _written: Vec<ObjectId> = try_par_hash_with(&prepared, threads, write_bytes, |prep| {
        repo.odb
            .write_loose_zlib_prehashed(&prep.oid, &prep.zlib_store, write_opts)
            .map_err(|e| Error::Message(format!("could not store {}: {e}", prep.index_relpath)))
    })
    .map_err(|e: ParallelHashError<Error>| match e {
        ParallelHashError::Task(err) => err,
    })?;

    let skip_per_path_cache_invalidate =
        index.cache_tree.is_none() && index.untracked_cache.is_none();
    let mut batch_entries: Vec<IndexEntry> = Vec::with_capacity(prepared.len());
    for prep in prepared {
        let abs = work_tree.join(&prep.index_relpath);
        if prep.meta.is_dir()
            && !prep.meta.file_type().is_symlink()
            && read_submodule_head_oid(&abs).is_some()
        {
            deferred.push(prep.index_relpath);
            continue;
        }
        icase_map.remove_alias_of(index, prep.index_relpath.as_bytes());
        let mut entry = entry_from_metadata(
            &prep.meta,
            prep.index_relpath.as_bytes(),
            prep.oid,
            prep.mode,
        );
        entry.mode = prep.mode;
        if !skip_per_path_cache_invalidate {
            index.invalidate_untracked_cache_for_path(&prep.index_relpath);
            index.invalidate_cache_tree_for_path(&entry.path);
        }
        batch_entries.push(entry);
        outcome.added += 1;
    }
    if !batch_entries.is_empty() {
        for entry in &batch_entries {
            if let Ok(rel) = std::str::from_utf8(&entry.path) {
                mark_fsmonitor_staged(index, rel);
            }
        }
        index.entries.extend(batch_entries);
        if index.fsmonitor_last_update.is_some() {
            index.fsmonitor_validity_changed();
        }
    }

    for path in deferred {
        icase_map.remove_alias_of(index, path.as_bytes());
        stage_untracked_path(repo, work_tree, &path, ctx, index)?;
        outcome.added += 1;
    }

    Ok(())
}

fn stage_untracked_path(
    repo: &Repository,
    work_tree: &Path,
    rel_path: &str,
    ctx: &StagingConvertContext,
    index: &mut Index,
) -> Result<()> {
    let precompose_unicode = ctx
        .config
        .get_bool("core.precomposeunicode")
        .and_then(|r| r.ok())
        .unwrap_or(false);
    let resolved = resolve_worktree_path_for_staging(work_tree, rel_path, precompose_unicode);
    let abs = resolved.abs;
    let index_relpath = resolved.index_relpath;
    let meta = fs::symlink_metadata(&abs).map_err(|e| {
        Error::Io(std::io::Error::new(
            e.kind(),
            format!("could not read {rel_path}: {e}"),
        ))
    })?;

    if meta.is_dir() && !meta.file_type().is_symlink() && read_submodule_head_oid(&abs).is_some() {
        let head_oid = read_submodule_head_oid(&abs).ok_or_else(|| {
            Error::Message(format!(
                "could not resolve submodule HEAD for '{index_relpath}'"
            ))
        })?;
        return stage_gitlink_at(&index_relpath, &meta, head_oid, index);
    }

    let file_attrs = crlf::get_file_attrs(&ctx.attrs, &index_relpath, false, &ctx.config);
    let mode = mode_from_metadata(&meta);
    let oid = materialize_worktree_blob(
        &repo.odb,
        &abs,
        &meta,
        &ctx.conv,
        &file_attrs,
        &index_relpath,
        None,
    )
    .map_err(|e| Error::Message(format!("could not store {index_relpath}: {e}")))?;

    let mut entry = entry_from_metadata(&meta, index_relpath.as_bytes(), oid, mode);
    entry.mode = mode;
    index.stage_file(entry);
    mark_fsmonitor_staged(index, &index_relpath);
    Ok(())
}

fn validate_pathspecs(
    repo: &Repository,
    index: &Index,
    work_tree: &Path,
    unstaged: &[DiffEntry],
    untracked: &[String],
    opts: &StageOptions,
) -> Result<()> {
    let sources = pathspec_sources(opts);
    let positive = sources
        .iter()
        .zip(opts.pathspecs.iter())
        .filter(|(src, _)| !pathspec_is_exclude(src))
        .collect::<Vec<_>>();
    if positive.is_empty() {
        return Ok(());
    }
    let known = known_paths(repo, index, unstaged, untracked)?;
    for (label, resolved) in positive {
        if !selector_matches_known(resolved, &known, work_tree)? {
            return Err(Error::Message(format!(
                "pathspec '{label}' did not match any files"
            )));
        }
    }
    Ok(())
}

fn pathspec_sources(opts: &StageOptions) -> Vec<String> {
    if opts.pathspec_sources.is_empty() {
        opts.pathspecs.clone()
    } else {
        opts.pathspec_sources.clone()
    }
}

fn known_paths(
    repo: &Repository,
    index: &Index,
    unstaged: &[DiffEntry],
    untracked: &[String],
) -> Result<Vec<String>> {
    let mut set = HashSet::<String>::new();
    for entry in &index.entries {
        if entry.stage() == 0 {
            set.insert(String::from_utf8_lossy(&entry.path).into_owned());
        }
    }
    for entry in unstaged {
        set.insert(entry.path().to_owned());
    }
    for path in untracked {
        set.insert(path.clone());
    }
    for path in head_tree_paths(repo)? {
        set.insert(path);
    }
    Ok(set.into_iter().collect())
}

fn head_tree_paths(repo: &Repository) -> Result<Vec<String>> {
    let head = resolve_head(&repo.git_dir)?;
    let Some(head_oid) = head.oid() else {
        return Ok(Vec::new());
    };
    let obj = repo.odb.read(head_oid)?;
    let commit = parse_commit(&obj.data)?;
    let mut paths = HashSet::new();
    collect_tree_paths(repo, commit.tree, "", &mut paths)?;
    Ok(paths.into_iter().collect())
}

fn collect_tree_paths(
    repo: &Repository,
    tree_oid: ObjectId,
    prefix: &str,
    out: &mut HashSet<String>,
) -> Result<()> {
    let obj = repo.odb.read(&tree_oid)?;
    for entry in parse_tree(&obj.data)? {
        let name = String::from_utf8_lossy(&entry.name);
        let path = if prefix.is_empty() {
            name.into_owned()
        } else {
            format!("{prefix}/{name}")
        };
        if entry.mode == MODE_TREE {
            collect_tree_paths(repo, entry.oid, &path, out)?;
        } else {
            out.insert(path);
        }
    }
    Ok(())
}

fn selector_matches_known(resolved: &str, known: &[String], work_tree: &Path) -> Result<bool> {
    let spec = [resolved.to_owned()];
    if known.iter().any(|p| matches_pathspec_list(p, &spec)) {
        return Ok(true);
    }
    if has_glob_chars(resolved) || resolved.starts_with(":(") {
        return Ok(false);
    }
    if !resolved.starts_with(':') {
        let abs = work_tree.join(resolved);
        if abs.exists() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn stage_gitlink_at(
    rel_path: &str,
    meta: &fs::Metadata,
    head_oid: ObjectId,
    index: &mut Index,
) -> Result<()> {
    let mut entry = entry_from_metadata(meta, rel_path.as_bytes(), head_oid, MODE_GITLINK);
    entry.size = 0;
    index.stage_file(entry);
    mark_fsmonitor_staged(index, rel_path);
    Ok(())
}

fn mark_fsmonitor_staged(index: &mut Index, rel_path: &str) {
    if index.fsmonitor_last_update.is_some() {
        if let Some(staged) = index.get_mut(rel_path.as_bytes(), 0) {
            staged.set_fsmonitor_valid(true);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use crate::index::{entry_from_stat, IndexEntry, MODE_GITLINK, MODE_REGULAR};
    use crate::objects::ObjectKind;
    use crate::progress::NullProgress;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    fn init_repo(root: &Path) -> Repository {
        let git = root.join(".git");
        fs::create_dir_all(git.join("objects")).unwrap();
        fs::create_dir_all(git.join("refs/heads")).unwrap();
        fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(
            git.join("config"),
            "[core]\n\trepositoryformatversion = 0\n\tbare = false\n",
        )
        .unwrap();
        Repository::open(&git, Some(root)).unwrap()
    }

    fn stage_file_in_index(repo: &Repository, index: &mut Index, rel: &str, contents: &[u8]) {
        let wt = repo.work_tree.as_ref().unwrap();
        let abs = wt.join(rel);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&abs, contents).unwrap();
        let oid = repo.odb.write(ObjectKind::Blob, contents).unwrap();
        let entry = entry_from_stat(&abs, rel.as_bytes(), oid, MODE_REGULAR).unwrap();
        index.add_or_replace(entry);
    }

    fn write_index(repo: &Repository, index: &mut Index) {
        index.sort();
        repo.write_index(index).unwrap();
    }

    fn cache_tree_node_valid(index: &Index, path: &str) -> Option<bool> {
        let root = index.cache_tree.as_ref()?;
        let mut current = root;
        if !path.is_empty() {
            for component in path.split('/') {
                if component.is_empty() {
                    continue;
                }
                current = current
                    .children
                    .iter()
                    .find(|c| c.name == component.as_bytes())?;
            }
        }
        Some(current.is_valid())
    }

    #[test]
    fn stage_all_adds_modifies_deletes() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        let mut index = Index::new();
        stage_file_in_index(&repo, &mut index, "keep.txt", b"keep\n");
        stage_file_in_index(&repo, &mut index, "modify.txt", b"old\n");
        stage_file_in_index(&repo, &mut index, "remove.txt", b"gone\n");
        write_index(&repo, &mut index);

        fs::write(root.join("modify.txt"), b"new\n").unwrap();
        fs::remove_file(root.join("remove.txt")).unwrap();
        fs::write(root.join("fresh.txt"), b"added\n").unwrap();

        let outcome = stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        assert_eq!(outcome.added, 1);
        assert_eq!(outcome.modified, 1);
        assert_eq!(outcome.removed, 1);

        let index = repo.load_index().unwrap();
        assert!(index.get(b"fresh.txt", 0).is_some());
        assert!(index.get(b"modify.txt", 0).is_some());
        assert!(index.get(b"remove.txt", 0).is_none());
    }

    #[test]
    fn stage_pathspec_limits_paths() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        let mut index = Index::new();
        stage_file_in_index(&repo, &mut index, "a.txt", b"a\n");
        stage_file_in_index(&repo, &mut index, "b.txt", b"b\n");
        write_index(&repo, &mut index);
        fs::write(root.join("a.txt"), b"a2\n").unwrap();
        fs::write(root.join("b.txt"), b"b2\n").unwrap();

        let outcome = stage(
            &repo,
            &StageOptions {
                pathspecs: vec!["a.txt".to_owned()],
                ..StageOptions::default()
            },
            &mut NullProgress,
        )
        .unwrap();
        assert_eq!(outcome.modified, 1);
        assert_eq!(outcome.total(), 1);

        let index = repo.load_index().unwrap();
        let a_oid = index.get(b"a.txt", 0).unwrap().oid;
        let b_oid = index.get(b"b.txt", 0).unwrap().oid;
        let a_blob = repo.odb.read(&a_oid).unwrap();
        let b_blob = repo.odb.read(&b_oid).unwrap();
        assert_eq!(a_blob.data, b"a2\n");
        assert_eq!(b_blob.data, b"b\n");
    }

    #[test]
    fn stage_skips_ignored_untracked() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
        fs::create_dir_all(root.join("ignored")).unwrap();
        fs::write(root.join("ignored/x.txt"), b"nope\n").unwrap();
        fs::write(root.join("tracked-new.txt"), b"yes\n").unwrap();

        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        let index = repo.load_index().unwrap();
        assert!(index.get(b"tracked-new.txt", 0).is_some());
        assert!(index.get(b"ignored/x.txt", 0).is_none());
    }

    #[test]
    fn stage_precomposes_untracked_index_path_while_reading_nfd_disk_path() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(
            root.join(".git/config"),
            "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tprecomposeunicode = true\n",
        )
        .unwrap();
        repo.reload_config().unwrap();
        let nfd = "cafe\u{301}.txt";
        fs::write(root.join(nfd), b"content\n").unwrap();

        let outcome = stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();

        assert_eq!(outcome.added, 1);
        let index = repo.load_index().unwrap();
        let entry = index.get("caf\u{e9}.txt".as_bytes(), 0).unwrap();
        assert!(index.get(nfd.as_bytes(), 0).is_none());
        assert_eq!(repo.odb.read(&entry.oid).unwrap().data, b"content\n");
    }

    #[test]
    fn stage_skips_clean_entries() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        let mut index = Index::new();
        stage_file_in_index(&repo, &mut index, "clean.txt", b"same\n");
        stage_file_in_index(&repo, &mut index, "dirty.txt", b"old\n");
        write_index(&repo, &mut index);
        fs::write(root.join("dirty.txt"), b"new\n").unwrap();

        repo.odb.enable_mem_overlay();
        let outcome = stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        assert_eq!(outcome.modified, 1);
        assert_eq!(outcome.total(), 1);

        let overlay_len = repo
            .odb
            .mem_overlay_len_for_tests()
            .expect("overlay enabled");
        assert_eq!(
            overlay_len, 1,
            "only the dirty blob should be hashed/written"
        );
    }

    #[test]
    fn stage_invalidates_cache_tree_paths_only() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        let mut index = Index::new();
        stage_file_in_index(&repo, &mut index, "alpha/one.txt", b"1\n");
        stage_file_in_index(&repo, &mut index, "beta/two.txt", b"2\n");
        let cache = crate::write_tree::build_cache_tree_from_index(&repo.odb, &index).unwrap();
        index.set_cache_tree(cache);
        write_index(&repo, &mut index);

        fs::write(root.join("beta/two.txt"), b"22\n").unwrap();

        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();

        let index = repo.load_index().unwrap();
        assert_eq!(cache_tree_node_valid(&index, ""), Some(false));
        assert_eq!(cache_tree_node_valid(&index, "alpha"), Some(true));
        assert_eq!(cache_tree_node_valid(&index, "beta"), Some(false));
    }

    fn conflict_index_entry(path: &str, stage: u8, oid: ObjectId, mode: u32) -> IndexEntry {
        let path_bytes = path.as_bytes().to_vec();
        IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode,
            uid: 0,
            gid: 0,
            size: 0,
            oid,
            flags: (path_bytes.len().min(0xFFF) as u16) | ((stage as u16) << 12),
            flags_extended: None,
            path: path_bytes,
            base_index_pos: 0,
        }
    }

    #[test]
    fn stage_resolved_conflict_clears_unmerged_stages() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        let base = repo.odb.write(ObjectKind::Blob, b"base\n").unwrap();
        let ours = repo.odb.write(ObjectKind::Blob, b"ours\n").unwrap();
        let theirs = repo.odb.write(ObjectKind::Blob, b"theirs\n").unwrap();

        let mut index = Index::new();
        index
            .entries
            .push(conflict_index_entry("f.txt", 1, base, MODE_REGULAR));
        index
            .entries
            .push(conflict_index_entry("f.txt", 2, ours, MODE_REGULAR));
        index
            .entries
            .push(conflict_index_entry("f.txt", 3, theirs, MODE_REGULAR));
        write_index(&repo, &mut index);

        fs::write(root.join("f.txt"), b"resolved\n").unwrap();
        let outcome = stage(
            &repo,
            &StageOptions {
                pathspecs: vec!["f.txt".to_owned()],
                ..StageOptions::default()
            },
            &mut NullProgress,
        )
        .unwrap();
        assert_eq!(outcome.total(), 1);

        let index = repo.load_index().unwrap();
        assert!(
            !index
                .entries
                .iter()
                .any(|e| e.path == b"f.txt" && e.stage() != 0),
            "unmerged stages must be cleared"
        );
        let staged = index.get(b"f.txt", 0).expect("stage 0 entry");
        let blob = repo.odb.read(&staged.oid).unwrap();
        assert_eq!(blob.data, b"resolved\n");
    }

    #[test]
    fn stage_linked_worktree_uses_common_repository_config() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let main = root.join("main");
        std::fs::create_dir_all(&main).unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(&main)
            .status()
            .expect("git init")
            .success());
        for args in [
            ["config", "user.email", "t@example.com"],
            ["config", "user.name", "t"],
            ["config", "core.autocrlf", "true"],
        ] {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&main)
                .status()
                .unwrap();
        }
        std::fs::write(main.join("README"), b"seed\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "README"])
            .current_dir(&main)
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-qm", "seed"])
            .current_dir(&main)
            .status()
            .unwrap();

        let linked = root.join("linked");
        assert!(std::process::Command::new("git")
            .args([
                "worktree",
                "add",
                "-b",
                "linked-branch",
                linked.to_str().unwrap(),
                "HEAD",
            ])
            .current_dir(&main)
            .status()
            .expect("worktree add")
            .success());

        let repo = Repository::discover(Some(&linked)).unwrap();
        std::fs::write(linked.join("grit.txt"), b"grit\r\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        let index = repo.load_index().unwrap();
        let oid = index.get(b"grit.txt", 0).unwrap().oid;
        assert_eq!(
            repo.odb.read(&oid).unwrap().data,
            b"grit\n",
            "linked worktree must use common repo autocrlf config"
        );
    }

    #[test]
    fn stage_applies_autocrlf_on_write() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(
            root.join(".git/config"),
            "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tautocrlf = true\n",
        )
        .unwrap();
        repo.reload_config().unwrap();
        fs::write(root.join("f.txt"), b"changed\r\n").unwrap();

        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        let index = repo.load_index().unwrap();
        let oid = index.get(b"f.txt", 0).unwrap().oid;
        assert_eq!(repo.odb.read(&oid).unwrap().data, b"changed\n");
    }

    fn commit_in_repo(repo: &Repository, rel_file: &str, content: &[u8], msg: &str) -> ObjectId {
        let wt = repo.work_tree.as_ref().unwrap();
        let abs = wt.join(rel_file);
        if let Some(p) = abs.parent() {
            fs::create_dir_all(p).unwrap();
        }
        fs::write(&abs, content).unwrap();
        let blob = repo.odb.write(ObjectKind::Blob, content).unwrap();
        let tree_body = format!("100644 {blob}\t{rel_file}\n");
        let tree = repo
            .odb
            .write(ObjectKind::Tree, tree_body.as_bytes())
            .unwrap();
        let commit_body =
            format!("tree {tree}\nauthor t <t@t> 0 +0000\ncommitter t <t@t> 0 +0000\n\n{msg}\n");
        repo.odb
            .write(ObjectKind::Commit, commit_body.as_bytes())
            .unwrap()
    }

    fn open_nested_repo(nested_root: &Path) -> Repository {
        let git = nested_root.join(".git");
        fs::create_dir_all(git.join("objects")).unwrap();
        fs::create_dir_all(git.join("refs/heads")).unwrap();
        fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(
            git.join("config"),
            "[core]\n\trepositoryformatversion = 0\n\tbare = false\n",
        )
        .unwrap();
        Repository::open(&git, Some(nested_root)).unwrap()
    }

    fn set_branch_head(repo: &Repository, head: ObjectId) {
        let main = repo.git_dir.join("refs/heads/main");
        fs::write(main, format!("{head}\n")).unwrap();
    }

    #[test]
    fn stage_gitlink_records_submodule_head() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        let sub = root.join("sub");
        fs::create_dir_all(&sub).unwrap();
        let sub_repo = open_nested_repo(&sub);
        let old_head = commit_in_repo(&sub_repo, "inside.txt", b"old\n", "old");
        set_branch_head(&sub_repo, old_head);

        let mut index = Index::new();
        let mut gitlink = conflict_index_entry("sub", 0, old_head, MODE_GITLINK);
        gitlink.size = 0;
        index.push_entry_unsorted(gitlink);
        write_index(&repo, &mut index);

        let new_head = commit_in_repo(&sub_repo, "inside.txt", b"new\n", "new");
        set_branch_head(&sub_repo, new_head);

        let outcome = stage(
            &repo,
            &StageOptions {
                pathspecs: vec!["sub".to_owned()],
                ..StageOptions::default()
            },
            &mut NullProgress,
        )
        .unwrap();
        assert_eq!(outcome.total(), 1);

        let index = repo.load_index().unwrap();
        let entry = index.get(b"sub", 0).unwrap();
        assert_eq!(entry.mode, MODE_GITLINK);
        assert_eq!(entry.oid, new_head);
    }

    #[test]
    fn stage_mode_update_skips_untracked() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        let mut index = Index::new();
        stage_file_in_index(&repo, &mut index, "tracked.txt", b"old\n");
        write_index(&repo, &mut index);
        fs::write(root.join("tracked.txt"), b"new\n").unwrap();
        fs::write(root.join("brand-new.txt"), b"hi\n").unwrap();

        let outcome = stage(
            &repo,
            &StageOptions {
                mode: StageMode::Update,
                ..StageOptions::default()
            },
            &mut NullProgress,
        )
        .unwrap();
        assert_eq!(outcome.modified, 1);
        assert_eq!(outcome.added, 0);
        assert_eq!(outcome.total(), 1);

        let index = repo.load_index().unwrap();
        assert!(index.get(b"tracked.txt", 0).is_some());
        assert!(index.get(b"brand-new.txt", 0).is_none());
    }
}
