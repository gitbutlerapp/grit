//! Build [`FileDiff`] lists for patch, stat, and name-only detail levels.

use crate::config::ConfigSet;
use crate::error::Result;
use crate::odb::Odb;
use crate::parallel::ThreadPolicy;

use super::detail::{DiffDetail, DiffTotals, FileDiff, LineStat};
use super::tree_content::{
    tree_diff_stat_parallel, tree_diff_structured_parallel, TreeDiffStructured,
};
use super::{detect_renames_with_policy, DiffEntry, DiffOptions, DiffStatus};

fn entries_with_options_renames(
    odb: &Odb,
    entries: Vec<DiffEntry>,
    diff_options: &DiffOptions,
    config: &ConfigSet,
    thread_policy: &ThreadPolicy,
) -> Result<Vec<DiffEntry>> {
    if let Some(percent) = diff_options.rename_similarity_threshold() {
        if percent <= 100 {
            return detect_renames_with_policy(
                odb,
                None,
                entries,
                percent,
                config,
                thread_policy,
                None,
            );
        }
    }
    Ok(entries)
}

fn path_from_entry(entry: &DiffEntry) -> String {
    entry
        .new_path
        .clone()
        .or_else(|| entry.old_path.clone())
        .unwrap_or_default()
}

fn old_path_from_entry(entry: &DiffEntry, path: &str) -> Option<String> {
    entry
        .old_path
        .as_ref()
        .filter(|op| op.as_str() != path)
        .cloned()
}

impl FileDiff {
    /// Metadata-only row for name-only and name-status output.
    ///
    /// Moves the paths and modes out of `entry` without reading any blob, so converting a
    /// list entry by entry costs no more than the entries themselves. This is the row
    /// [`build_file_diffs`] produces for [`DiffDetail::NameOnly`] and [`DiffDetail::NameStatus`].
    ///
    /// # Parameters
    ///
    /// - `entry`: one tree or index diff record.
    ///
    /// # Returns
    ///
    /// A [`FileDiff`] whose `path` is the new path (old path for deletions), with `old_path`
    /// set only when it differs, absent-side modes (`000000`) as `None`, and no stat or hunks.
    #[must_use]
    pub fn from_metadata(entry: DiffEntry) -> Self {
        let similarity = entry.rename_similarity();
        let (path, old_path) = match (entry.new_path, entry.old_path) {
            (Some(path), old) => {
                let old = old.filter(|old| old != &path);
                (path, old)
            }
            (None, old) => (old.unwrap_or_default(), None),
        };
        Self {
            path,
            old_path,
            status: entry.status,
            old_mode: entry.old_mode.octal_if_present(),
            new_mode: entry.new_mode.octal_if_present(),
            binary: false,
            similarity,
            stat: None,
            structured: None,
            combined: None,
            combined_status: None,
        }
    }
}

fn file_diff_from_structured(t: TreeDiffStructured) -> FileDiff {
    let path = path_from_entry(&t.entry);
    FileDiff {
        path: path.clone(),
        old_path: old_path_from_entry(&t.entry, &path),
        status: t.entry.status,
        old_mode: t.structured.old_mode.clone(),
        new_mode: t.structured.new_mode.clone(),
        binary: t.binary,
        similarity: t.entry.rename_similarity(),
        stat: None,
        structured: Some(t.structured),
        combined: None,
        combined_status: None,
    }
}

fn file_diff_from_stat(entry: DiffEntry, binary: bool, stat: Option<LineStat>) -> FileDiff {
    let path = path_from_entry(&entry);
    FileDiff {
        path: path.clone(),
        old_path: old_path_from_entry(&entry, &path),
        status: entry.status,
        old_mode: entry.old_mode.octal_if_present(),
        new_mode: entry.new_mode.octal_if_present(),
        binary,
        similarity: entry.rename_similarity(),
        stat,
        structured: None,
        combined: None,
        combined_status: None,
    }
}

/// Output of [`build_file_diffs`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffFilesOutput {
    /// Per-file results in path order.
    pub files: Vec<FileDiff>,
    /// Aggregate counts for stat detail; `None` for patch and name-only modes.
    pub totals: Option<DiffTotals>,
}

/// Fill per-file diff bodies for tree/index diff entries.
///
/// Applies [`DiffOptions::renames`] to the entry list first (Git's default `-M50`), pairing
/// added and deleted paths before stat or patch work. Use [`RenameDetection::Off`] when the
/// caller already ran [`detect_renames`](crate::diff::detect_renames) or
/// [`diff_trees_with_renames`](crate::diff::diff_trees_with_renames). Metadata-only
/// [`DiffDetail::NameOnly`] and [`DiffDetail::NameStatus`] still skip line-diff blob reads,
/// but rename detection may read blob sides for similarity scoring when renames are enabled.
///
/// # Errors
///
/// Propagates object database and parallel diff errors.
pub fn build_file_diffs(
    odb: &Odb,
    entries: Vec<DiffEntry>,
    detail: DiffDetail,
    diff_options: DiffOptions,
    config: &ConfigSet,
    thread_policy: &ThreadPolicy,
) -> Result<DiffFilesOutput> {
    let entries = entries_with_options_renames(odb, entries, &diff_options, config, thread_policy)?;
    match detail {
        DiffDetail::NameOnly | DiffDetail::NameStatus => {
            let files: Vec<FileDiff> = entries.into_iter().map(FileDiff::from_metadata).collect();
            Ok(DiffFilesOutput {
                files,
                totals: None,
            })
        }
        DiffDetail::Stat => {
            let rows = tree_diff_stat_parallel(odb, entries, config, thread_policy, diff_options)?;
            let files: Vec<FileDiff> = rows
                .into_iter()
                .map(|(entry, binary, stat)| file_diff_from_stat(entry, binary, stat))
                .collect();
            let totals = Some(DiffTotals::from_files(&files));
            Ok(DiffFilesOutput { files, totals })
        }
        DiffDetail::Patch => {
            let structured =
                tree_diff_structured_parallel(odb, entries, config, thread_policy, diff_options)?;
            let files: Vec<FileDiff> = structured
                .into_iter()
                .map(file_diff_from_structured)
                .collect();
            Ok(DiffFilesOutput {
                files,
                totals: None,
            })
        }
    }
}

/// Build patch [`FileDiff`] rows one entry at a time without retaining every hunk list.
///
/// The callback receives each [`FileDiff`] in path order; drop it before the next entry is
/// computed so peak memory stays near one file's diff.
///
/// # Errors
///
/// Propagates object database and parallel diff errors, and callback errors.
pub fn build_file_diffs_streaming(
    odb: &Odb,
    entries: Vec<DiffEntry>,
    diff_options: DiffOptions,
    config: &ConfigSet,
    thread_policy: &ThreadPolicy,
    mut emit: impl FnMut(FileDiff) -> Result<()>,
) -> Result<()> {
    use super::tree_content::for_each_tree_diff_structured;
    let entries = entries_with_options_renames(odb, entries, &diff_options, config, thread_policy)?;
    for_each_tree_diff_structured(
        odb,
        &entries,
        diff_options,
        config,
        thread_policy,
        |structured| emit(file_diff_from_structured(structured)),
    )
}

/// Git `--name-status` letter for a diff entry (uppercase status + rename score suffix).
#[must_use]
pub fn name_status_line(entry: &DiffEntry) -> String {
    let path = path_from_entry(entry);
    match entry.status {
        DiffStatus::Renamed => {
            let score = entry.score.unwrap_or(100);
            let old = entry.old_path.as_deref().unwrap_or("");
            format!("R{score:03}\t{old}\t{path}")
        }
        DiffStatus::Copied => {
            let score = entry.score.unwrap_or(100);
            let old = entry.old_path.as_deref().unwrap_or("");
            format!("C{score:03}\t{old}\t{path}")
        }
        DiffStatus::Added => format!("A\t{path}"),
        DiffStatus::Deleted => format!("D\t{path}"),
        DiffStatus::Modified => format!("M\t{path}"),
        DiffStatus::TypeChanged => format!("T\t{path}"),
        DiffStatus::Unmerged => format!("U\t{path}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::{FileMode, ObjectId};

    #[test]
    fn metadata_rows_preserve_selected_paths_modes_and_similarity() {
        for status in [
            DiffStatus::Added,
            DiffStatus::Deleted,
            DiffStatus::Modified,
            DiffStatus::Renamed,
            DiffStatus::Copied,
        ] {
            let entry = DiffEntry {
                status,
                old_path: (status != DiffStatus::Added).then(|| "old".to_owned()),
                new_path: (status != DiffStatus::Deleted).then(|| {
                    if status == DiffStatus::Modified {
                        "old"
                    } else {
                        "new"
                    }
                    .to_owned()
                }),
                old_mode: if status == DiffStatus::Added {
                    FileMode::ABSENT
                } else {
                    FileMode::REGULAR
                },
                new_mode: if status == DiffStatus::Deleted {
                    FileMode::ABSENT
                } else {
                    FileMode::EXECUTABLE
                },
                old_oid: ObjectId::zero(),
                new_oid: ObjectId::zero(),
                score: Some(75),
            };
            let file = FileDiff::from_metadata(entry);
            assert_eq!(file.status, status);
            assert_eq!(
                file.path,
                if matches!(status, DiffStatus::Deleted | DiffStatus::Modified) {
                    "old"
                } else {
                    "new"
                }
            );
            let renamed = matches!(status, DiffStatus::Renamed | DiffStatus::Copied);
            assert_eq!(file.old_path.as_deref(), renamed.then_some("old"));
            assert_eq!(file.similarity, renamed.then_some(75));
            assert_eq!(
                file.old_mode.as_deref(),
                (status != DiffStatus::Added).then_some("100644")
            );
            assert_eq!(
                file.new_mode.as_deref(),
                (status != DiffStatus::Deleted).then_some("100755")
            );
            assert!(file.stat.is_none() && file.structured.is_none());
        }
    }

    #[test]
    fn build_file_diffs_applies_default_rename_detection() {
        use crate::config::ConfigSet;
        use crate::objects::{serialize_commit, CommitData, ObjectKind, TreeEntry};
        use crate::parallel::ThreadPolicy;
        use crate::repo::{InitOptions, Repository};

        fn tree_with(repo: &Repository, entries: &[(&str, &[u8])]) -> ObjectId {
            let mut tes = Vec::new();
            for (name, data) in entries {
                let oid = repo.odb().write(ObjectKind::Blob, data).expect("blob");
                tes.push(TreeEntry {
                    mode: 0o100644,
                    name: name.as_bytes().to_vec(),
                    oid,
                });
            }
            let data = crate::objects::serialize_tree(&tes);
            repo.odb().write(ObjectKind::Tree, &data).expect("tree")
        }

        fn write_commit(repo: &Repository, tree: ObjectId, parents: Vec<ObjectId>) -> ObjectId {
            let ident = "A <a@example.com> 0 +0000";
            let commit = CommitData {
                tree,
                parents,
                author: ident.into(),
                committer: ident.into(),
                author_raw: Vec::new(),
                committer_raw: Vec::new(),
                encoding: None,
                message: "m\n".into(),
                raw_message: None,
                extra_headers: Vec::new(),
            };
            repo.odb()
                .write(ObjectKind::Commit, &serialize_commit(&commit))
                .expect("commit")
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        let shared = b"same line\nsame line\nsame line\n";
        let mut old_body = shared.to_vec();
        old_body.extend_from_slice(b"tail\n");
        let mut new_body = shared.to_vec();
        new_body.extend_from_slice(b"tail2\n");
        let parent_tree = tree_with(&repo, &[("old.txt", &old_body)]);
        let parent = write_commit(&repo, parent_tree, vec![]);
        let child_tree = tree_with(&repo, &[("new.txt", &new_body)]);
        let _child = write_commit(&repo, child_tree, vec![parent]);
        let old_tree = repo.odb().commit_tree(&parent).expect("tree");
        let new_tree = repo.odb().commit_tree(&_child).expect("tree");
        let entries = repo
            .diff()
            .tree_to_tree(Some(&old_tree), Some(&new_tree))
            .expect("diff");
        let config = ConfigSet::default();
        let with_renames = build_file_diffs(
            repo.odb(),
            entries.clone(),
            DiffDetail::Stat,
            DiffOptions::default(),
            &config,
            &ThreadPolicy::default(),
        )
        .expect("stat");
        assert_eq!(with_renames.files.len(), 1);
        assert_eq!(with_renames.files[0].status, DiffStatus::Renamed);
        let without = build_file_diffs(
            repo.odb(),
            entries,
            DiffDetail::Stat,
            DiffOptions::default().with_renames(crate::diff::RenameDetection::Off),
            &config,
            &ThreadPolicy::default(),
        )
        .expect("stat");
        assert_eq!(without.files.len(), 2);
    }
}
