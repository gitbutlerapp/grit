//! Diff the change one merge commit introduced (first-parent or combined `--cc`).

use std::path::Path;

use crate::config::ConfigSet;
use crate::diff::combined_diff_patch::{
    format_combined_structured_body, structured_combined_diff_body, CombinedDiffWsOptions,
    CombinedStructuredTextDiff,
};
use crate::diff::combined_tree_diff::{combined_diff_paths_filtered, CombinedTreeDiffOptions};
use crate::diff::detail::{DiffDetail, DiffTotals, FileDiff, LineStat};
use crate::diff::files::{build_file_diffs, DiffFilesOutput};
use crate::diff::merge_diff::{
    blob_text_for_diff, enrich_combined_path_renames, format_combined_binary,
    format_combined_textconv_patch, is_binary_for_diff, read_blob_at_path,
};
use crate::diff::{DiffEntry, DiffOptions, DiffStatus};
use crate::error::Result;
use crate::objects::{parse_commit, ObjectId};
use crate::odb::Odb;
use crate::parallel::ThreadPolicy;
use crate::repo::Repository;

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

fn line_stat_from_combined(body: &CombinedStructuredTextDiff) -> LineStat {
    let mut insertions = 0usize;
    let mut deletions = 0usize;
    for hunk in &body.hunks {
        for line in &hunk.lines {
            if line.parents.contains(&'+') && !line.text.is_empty() {
                insertions += 1;
            }
            if line.parents.contains(&'-') {
                deletions += 1;
            }
        }
    }
    LineStat {
        insertions,
        deletions,
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
            Some(parent) => {
                Some(crate::objects::parse_commit(&repo.odb().read(parent)?.data)?.tree)
            }
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
        let filtered = filter_entries(entries, pathspecs);
        return build_file_diffs(
            repo.odb(),
            filtered,
            detail,
            *options,
            config,
            thread_policy,
        );
    }

    combined_commit_diff_files(
        repo,
        &commit.tree,
        &commit.parents,
        detail,
        options,
        pathspecs,
        config,
    )
}

fn filter_entries(entries: Vec<DiffEntry>, pathspecs: &[String]) -> Vec<DiffEntry> {
    if pathspecs.is_empty() {
        return entries;
    }
    entries
        .into_iter()
        .filter(|e| {
            let path = e
                .new_path
                .as_deref()
                .or(e.old_path.as_deref())
                .unwrap_or("");
            pathspecs.iter().any(|spec| path_matches(path, spec))
        })
        .collect()
}

fn path_matches(path: &str, spec: &str) -> bool {
    crate::pathspec::path_allowed_by_pathspec_list(&[spec.to_owned()], path)
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

fn combined_commit_diff_files(
    repo: &Repository,
    result_tree: &ObjectId,
    parent_commits: &[ObjectId],
    detail: DiffDetail,
    options: &DiffOptions,
    pathspecs: &[String],
    config: &ConfigSet,
) -> Result<DiffFilesOutput> {
    let odb = repo.odb();
    let mut parent_trees = Vec::with_capacity(parent_commits.len());
    for p in parent_commits {
        parent_trees.push(parse_commit(&odb.read(p)?.data)?.tree);
    }

    let walk = CombinedTreeDiffOptions::default();
    let mut paths = combined_diff_paths_filtered(odb, result_tree, parent_commits, &walk, None)?;
    let path_names: Vec<String> = paths.iter().map(|p| p.path.clone()).collect();
    let filtered_names = filter_combined_paths(path_names, pathspecs);
    paths.retain(|p| filtered_names.iter().any(|n| n == &p.path));

    let git_dir = repo.git_dir();
    let ws = combined_ws(options);
    let context = options.context_lines;
    let mut files = Vec::new();
    let mut insertions = 0usize;
    let mut deletions = 0usize;

    for mut path in paths {
        enrich_combined_path_renames(odb, &mut path, &parent_trees, result_tree, 50);

        if detail == DiffDetail::NameOnly || detail == DiffDetail::NameStatus {
            files.push(FileDiff {
                path: path.path.clone(),
                old_path: None,
                status: DiffStatus::Modified,
                old_mode: None,
                new_mode: None,
                binary: false,
                similarity: None,
                stat: None,
                structured: None,
                combined: None,
            });
            continue;
        }

        let patch = format_combined_textconv_patch(
            git_dir,
            config,
            odb,
            &path.path,
            &parent_trees,
            result_tree,
            7,
            context,
            true,
            false,
            ws,
            false,
            None,
            &path.parents,
            false,
        );

        let binary = combined_path_is_binary(git_dir, odb, &path.path, &parent_trees, result_tree)
            || patch
                .as_ref()
                .is_some_and(|p| p.contains("Binary files differ"));
        if binary {
            let parent_oids: Vec<ObjectId> = path.parents.iter().map(|s| s.oid).collect();
            let _patch = format_combined_binary(&path.path, &parent_oids, &path.merge_oid, 7, true);
            files.push(FileDiff {
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
            });
            continue;
        }

        let parent_texts =
            parent_blob_texts(git_dir, config, odb, &path, &parent_trees, result_tree)?;
        let result_blob = read_blob_at_path(odb, result_tree, &path.path)
            .ok_or_else(|| crate::error::Error::ObjectNotFound(path.path.clone()))?;
        let result_text = blob_text_for_diff(git_dir, config, &path.path, &result_blob, false);
        let combined =
            structured_combined_diff_body(&parent_texts, &result_text, context, true, ws);
        let stat = match detail {
            DiffDetail::Stat => Some(line_stat_from_combined(&combined)),
            _ => None,
        };
        if let Some(s) = stat {
            insertions += s.insertions;
            deletions += s.deletions;
        }
        files.push(FileDiff {
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
        });
    }

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

fn combined_path_is_binary(
    git_dir: &Path,
    odb: &Odb,
    path: &str,
    parent_trees: &[ObjectId],
    result_tree: &ObjectId,
) -> bool {
    let Some(result_blob) = read_blob_at_path(odb, result_tree, path) else {
        return false;
    };
    if is_binary_for_diff(git_dir, path, &result_blob) {
        return true;
    }
    for tree in parent_trees {
        if let Some(oid) = crate::merge_diff::blob_oid_at_path(odb, tree, path) {
            if let Ok(obj) = odb.read(&oid) {
                if is_binary_for_diff(git_dir, path, &obj.data) {
                    return true;
                }
            }
        }
    }
    false
}

fn parent_blob_texts(
    git_dir: &Path,
    config: &ConfigSet,
    odb: &Odb,
    path: &crate::diff::combined_tree_diff::CombinedDiffPath,
    _parent_trees: &[ObjectId],
    _result_tree: &ObjectId,
) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(path.parents.len());
    for side in &path.parents {
        let text = if side.oid.is_zero() {
            String::new()
        } else {
            let blob = odb.read(&side.oid)?.data;
            blob_text_for_diff(git_dir, config, &path.path, &blob, false)
        };
        out.push(text);
    }
    Ok(out)
}

/// Format a [`CombinedStructuredTextDiff`] as Git combined patch body bytes.
#[must_use]
pub fn combined_structured_patch_body(diff: &CombinedStructuredTextDiff) -> String {
    format_combined_structured_body(diff)
}
