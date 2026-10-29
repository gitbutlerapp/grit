//! Diff the change one merge commit introduced (first-parent or combined `--cc`).

use std::path::Path;

use crate::config::ConfigSet;
use crate::diff::combined_diff_patch::{
    format_combined_structured_body, structured_combined_diff_body, CombinedDiffWsOptions,
    CombinedStructuredTextDiff,
};
use crate::diff::combined_tree_diff::{
    combined_diff_paths_trees, combined_parent_status_char, CombinedDiffPath,
    CombinedTreeDiffOptions,
};
use crate::diff::detail::{DiffDetail, DiffTotals, FileDiff, LineStat};
use crate::diff::files::{build_file_diffs, DiffFilesOutput};
use crate::diff::merge_diff::{
    blob_text_for_diff, enrich_combined_path_renames, format_combined_binary, is_binary_for_diff,
    read_blob_at_path,
};
use crate::diff::{DiffEntry, DiffOptions, DiffStatus};
use crate::error::Result;
use crate::objects::pack_store::PackReadContextGuard;
use crate::objects::{parse_commit, ObjectId};
use crate::odb::Odb;
use crate::parallel::{try_par_map_with_local, ParallelHashError, ParallelOp, ThreadPolicy};
use crate::repo::Repository;
use std::sync::Arc;

/// How to diff a merge commit's tree against its parents.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiffMerges {
    /// Diff merge result against the first parent only (non-merge commits always use this).
    FirstParent,
    /// Dense combined diff against every parent (`git show --cc`).
    #[default]
    Combined,
}

impl DiffMerges {
    /// Parse `first-parent` or `combined` (Git `--diff-merges` spelling).
    ///
    /// # Errors
    ///
    /// Returns an error when `value` is not a known mode name.
    pub fn parse(value: &str) -> std::result::Result<Self, String> {
        match value {
            "first-parent" | "first_parent" => Ok(Self::FirstParent),
            "combined" | "dense-combined" | "dense_combined" => Ok(Self::Combined),
            other => Err(format!(
                "unknown --diff-merges mode '{other}' (expected first-parent or combined)"
            )),
        }
    }
}

fn combined_ws(_options: &DiffOptions) -> CombinedDiffWsOptions {
    CombinedDiffWsOptions::default()
}

fn combined_status_letters(path: &CombinedDiffPath) -> String {
    path.parents
        .iter()
        .map(|s| combined_parent_status_char(s.status))
        .collect()
}

fn file_diff_from_combined_path(path: CombinedDiffPath) -> FileDiff {
    let combined_status = combined_status_letters(&path);
    FileDiff {
        path: path.path,
        old_path: None,
        status: DiffStatus::Modified,
        old_mode: None,
        new_mode: None,
        binary: false,
        similarity: None,
        stat: None,
        structured: None,
        combined: None,
        combined_status: Some(combined_status),
    }
}

fn filter_combined_paths(paths: Vec<String>, pathspecs: &[String]) -> Vec<String> {
    if pathspecs.is_empty() {
        return paths;
    }
    paths
        .into_iter()
        .filter(|p| crate::pathspec::path_allowed_by_pathspec_list(pathspecs, p))
        .collect()
}

/// Build per-file diffs for one commit, using combined merge diff when requested.
///
/// # Errors
///
/// Propagates object read, tree walk, and diff errors.
#[allow(clippy::too_many_arguments)]
pub fn commit_diff_files(
    repo: &Repository,
    commit_oid: &ObjectId,
    merges: DiffMerges,
    detail: DiffDetail,
    options: &DiffOptions,
    pathspecs: &[String],
    config: &ConfigSet,
    thread_policy: &ThreadPolicy,
) -> Result<DiffFilesOutput> {
    let data = repo.odb().read(commit_oid)?.data;
    let commit = parse_commit(&data)?;
    if commit.parents.len() < 2 || merges == DiffMerges::FirstParent {
        let old_tree = match commit.parents.first() {
            Some(parent) => Some(repo.odb().commit_tree(parent)?),
            None => None,
        };
        let entries = tree_diff_entries(
            repo.odb(),
            old_tree.as_ref(),
            Some(&commit.tree),
            "",
            config,
            thread_policy,
        )?;
        let filtered = filter_entries(repo, entries, pathspecs)?;
        let mut out = build_file_diffs(
            repo.odb(),
            filtered,
            detail,
            options.clone(),
            config,
            thread_policy,
        )?;
        out.files.sort_by(|a, b| a.path.cmp(&b.path));
        return Ok(out);
    }

    combined_commit_diff_files(
        repo,
        &commit.tree,
        &commit.parents,
        detail,
        options,
        pathspecs,
        config,
        thread_policy,
    )
}

fn filter_entries(
    repo: &Repository,
    entries: Vec<DiffEntry>,
    pathspecs: &[String],
) -> Result<Vec<DiffEntry>> {
    if pathspecs.is_empty() {
        return Ok(entries);
    }
    let matcher = repo.pathspec_matcher(pathspecs)?;
    Ok(entries
        .into_iter()
        .filter(|entry| {
            let path = entry
                .new_path
                .as_deref()
                .or(entry.old_path.as_deref())
                .unwrap_or("");
            matcher.matches_change(path, entry.old_path.as_deref())
        })
        .collect())
}

/// Tree diff entries for one commit vs its first parent (rename-aware).
///
/// # Errors
///
/// Propagates ODB and diff errors.
pub(crate) fn commit_first_parent_diff_entries(
    repo: &Repository,
    commit: &crate::objects::CommitData,
    pathspecs: &[String],
    config: &ConfigSet,
    thread_policy: &ThreadPolicy,
) -> Result<Vec<DiffEntry>> {
    let odb = repo.odb();
    let old_tree = match commit.parents.first() {
        Some(parent) => Some(odb.commit_tree(parent)?),
        None => None,
    };
    let entries = tree_diff_entries(
        odb,
        old_tree.as_ref(),
        Some(&commit.tree),
        "",
        config,
        thread_policy,
    )?;
    filter_entries(repo, entries, pathspecs)
}

fn tree_diff_entries(
    odb: &Odb,
    old_tree: Option<&ObjectId>,
    new_tree: Option<&ObjectId>,
    prefix: &str,
    config: &ConfigSet,
    thread_policy: &ThreadPolicy,
) -> Result<Vec<DiffEntry>> {
    let threshold = crate::diff::renames_from_config(config)
        .threshold_percent()
        .unwrap_or(100);
    crate::diff::diff_trees_with_renames_and_policy(
        odb,
        old_tree,
        new_tree,
        prefix,
        threshold,
        config,
        thread_policy,
    )
}

#[allow(clippy::too_many_arguments)]
fn combined_commit_diff_files(
    repo: &Repository,
    result_tree: &ObjectId,
    parent_commits: &[ObjectId],
    detail: DiffDetail,
    options: &DiffOptions,
    pathspecs: &[String],
    config: &ConfigSet,
    thread_policy: &ThreadPolicy,
) -> Result<DiffFilesOutput> {
    let odb = repo.odb();
    let mut parent_trees = Vec::with_capacity(parent_commits.len());
    for p in parent_commits {
        parent_trees.push(odb.commit_tree(p)?);
    }

    let walk = CombinedTreeDiffOptions::default();
    let parent_tree_opts: Vec<Option<ObjectId>> = parent_trees.iter().copied().map(Some).collect();
    let mut paths = combined_diff_paths_trees(odb, result_tree, &parent_tree_opts, &walk, None)?;
    let path_names: Vec<String> = paths.iter().map(|p| p.path.clone()).collect();
    let filtered_names = filter_combined_paths(path_names, pathspecs);
    paths.retain(|p| filtered_names.iter().any(|n| n == &p.path));

    for path in &mut paths {
        enrich_combined_path_renames(odb, path, &parent_trees, result_tree, 50);
    }

    if detail == DiffDetail::NameOnly {
        let files: Vec<FileDiff> = paths
            .into_iter()
            .map(|path| FileDiff {
                path: path.path,
                old_path: None,
                status: DiffStatus::Modified,
                old_mode: None,
                new_mode: None,
                binary: false,
                similarity: None,
                stat: None,
                structured: None,
                combined: None,
                combined_status: None,
            })
            .collect();
        return Ok(DiffFilesOutput {
            files,
            totals: None,
        });
    }
    if detail == DiffDetail::NameStatus {
        let files: Vec<FileDiff> = paths
            .into_iter()
            .map(file_diff_from_combined_path)
            .collect();
        return Ok(DiffFilesOutput {
            files,
            totals: None,
        });
    }

    let git_dir = repo.git_dir();
    let ws = combined_ws(options);
    let context = options.context_lines;
    let par_opts = thread_policy.resolve(ParallelOp::DiffContent, config);
    let store = odb.pack_store_for(odb.objects_dir());
    let result_tree = *result_tree;
    let rows = try_par_map_with_local(
        &paths,
        par_opts,
        0,
        || PackReadContextGuard::install(Arc::clone(&store)),
        |_ctx, path| {
            combined_path_file_diff(git_dir, config, odb, path, result_tree, detail, context, ws)
        },
    );
    let rows = match rows {
        Ok(rows) => rows,
        Err(ParallelHashError::Task(err)) => return Err(err),
    };

    let mut files = Vec::with_capacity(rows.len());
    let mut insertions = 0usize;
    let mut deletions = 0usize;
    for (file, stat) in rows {
        if let Some(s) = stat {
            insertions += s.insertions;
            deletions += s.deletions;
        }
        files.push(file);
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));

    let totals = if detail == DiffDetail::Stat {
        Some(DiffTotals {
            files_changed: files.len(),
            insertions,
            deletions,
        })
    } else {
        None
    };
    Ok(DiffFilesOutput { files, totals })
}

#[allow(clippy::too_many_arguments)]
fn combined_path_file_diff(
    git_dir: &Path,
    config: &ConfigSet,
    odb: &Odb,
    path: &CombinedDiffPath,
    result_tree: ObjectId,
    detail: DiffDetail,
    context: usize,
    ws: CombinedDiffWsOptions,
) -> Result<(FileDiff, Option<LineStat>)> {
    let (parent_blobs, result_blob) = load_combined_path_blobs(odb, path, &result_tree)?;
    let binary =
        combined_path_is_binary_from_blobs(git_dir, &path.path, &parent_blobs, &result_blob);
    if binary {
        let parent_oids: Vec<ObjectId> = path.parents.iter().map(|s| s.oid).collect();
        let _patch = format_combined_binary(&path.path, &parent_oids, &path.merge_oid, 7, true);
        return Ok((
            FileDiff {
                path: path.path.clone(),
                old_path: None,
                status: DiffStatus::Modified,
                old_mode: None,
                new_mode: None,
                binary: true,
                similarity: None,
                stat: if detail == DiffDetail::Stat {
                    Some(LineStat::default())
                } else {
                    None
                },
                structured: None,
                combined: None,
                combined_status: Some(combined_status_letters(path)),
            },
            None,
        ));
    }

    let mut parent_texts = Vec::with_capacity(path.parents.len());
    for (i, side) in path.parents.iter().enumerate() {
        let text = if side.oid.is_zero() {
            String::new()
        } else {
            blob_text_for_diff(git_dir, config, &path.path, &parent_blobs[i], false)
        };
        parent_texts.push(text);
    }
    let result_text = blob_text_for_diff(git_dir, config, &path.path, &result_blob, false);
    let combined = structured_combined_diff_body(&parent_texts, &result_text, context, true, ws);
    let stat = match detail {
        DiffDetail::Stat => Some(combined.line_stat()),
        _ => None,
    };
    Ok((
        FileDiff {
            path: path.path.clone(),
            old_path: None,
            status: DiffStatus::Modified,
            old_mode: None,
            new_mode: None,
            binary: false,
            similarity: None,
            stat,
            structured: None,
            combined: Some(combined),
            combined_status: Some(combined_status_letters(path)),
        },
        stat,
    ))
}

fn load_combined_path_blobs(
    odb: &Odb,
    path: &CombinedDiffPath,
    result_tree: &ObjectId,
) -> Result<(Vec<Vec<u8>>, Vec<u8>)> {
    let mut parent_blobs = Vec::with_capacity(path.parents.len());
    for side in &path.parents {
        if side.oid.is_zero() {
            parent_blobs.push(Vec::new());
        } else {
            parent_blobs.push(odb.read(&side.oid)?.data);
        }
    }
    let result_blob = read_blob_at_path(odb, result_tree, &path.path).unwrap_or_default();
    Ok((parent_blobs, result_blob))
}

fn combined_path_is_binary_from_blobs(
    git_dir: &Path,
    path: &str,
    parent_blobs: &[Vec<u8>],
    result_blob: &[u8],
) -> bool {
    if is_binary_for_diff(git_dir, path, result_blob) {
        return true;
    }
    parent_blobs
        .iter()
        .any(|blob| !blob.is_empty() && is_binary_for_diff(git_dir, path, blob))
}

/// Format a [`CombinedStructuredTextDiff`] as Git combined patch body bytes.
#[must_use]
pub fn combined_structured_patch_body(diff: &CombinedStructuredTextDiff) -> String {
    format_combined_structured_body(diff)
}
