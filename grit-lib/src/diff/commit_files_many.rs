//! Per-commit file diffs for many commits (used by `grit log -p` / `--stat`).

use std::collections::HashMap;

use crate::config::ConfigSet;
use crate::error::Result;
use crate::objects::{parse_commit, ObjectId};
use crate::odb::Odb;
use crate::parallel::{try_par_map, ParallelHashError, ParallelOp, ThreadPolicy};

use super::core::apply_diff_filter;
use super::core::diff_trees_with_renames_and_policy;
use super::detail::DiffDetail;
use super::files::{build_file_diffs, DiffFilesOutput};
use super::options::DiffOptions;
use super::Diff;
use super::DiffEntry;

/// Options for [`Diff::commit_files_many`].
#[derive(Debug, Clone)]
pub struct CommitFilesManyOptions {
    /// Line diff and rename settings.
    pub diff_options: DiffOptions,
    /// How much to compute per changed file.
    pub detail: DiffDetail,
    /// When set with [`DiffDetail::Patch`], merge commits (2+ parents) produce empty file lists.
    pub skip_merge_patches: bool,
}

/// One commit's diff output in [`Diff::commit_files_many`] (same order as input oids).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitFilesResult {
    /// Per-file diff bodies for this commit.
    pub output: DiffFilesOutput,
    /// Number of parents (0 = root).
    pub parent_count: usize,
}

struct CommitWork {
    tree: ObjectId,
    parent_tree: Option<ObjectId>,
    parent_count: usize,
}

fn rename_threshold(options: &DiffOptions) -> u32 {
    options.renames.threshold_percent().unwrap_or(101)
}

fn filter_entries(
    repo: &crate::repo::Repository,
    entries: Vec<DiffEntry>,
    pathspecs: &[String],
) -> Vec<DiffEntry> {
    if pathspecs.is_empty() {
        return entries;
    }
    entries
        .into_iter()
        .filter(|e| entry_matches_pathspecs(repo, e, pathspecs))
        .collect()
}

fn entry_matches_pathspecs(
    repo: &crate::repo::Repository,
    entry: &DiffEntry,
    pathspecs: &[String],
) -> bool {
    let path = entry
        .new_path
        .as_deref()
        .or(entry.old_path.as_deref())
        .unwrap_or("");
    if repo.paths_match_pathspecs(path, pathspecs) {
        return true;
    }
    if let Some(old) = entry.old_path.as_deref() {
        if old != path && repo.paths_match_pathspecs(old, pathspecs) {
            return true;
        }
    }
    false
}

fn parent_tree_for_commit(
    odb: &Odb,
    parent: &ObjectId,
    trees_by_commit: &HashMap<ObjectId, ObjectId>,
) -> Result<ObjectId> {
    if let Some(tree) = trees_by_commit.get(parent) {
        return Ok(*tree);
    }
    let raw = odb.read(parent)?;
    Ok(parse_commit(&raw.data)?.tree)
}

fn diff_one_commit(
    diff: &Diff,
    work: &CommitWork,
    pathspecs: &[String],
    opts: &CommitFilesManyOptions,
    config: &ConfigSet,
    thread_policy: &ThreadPolicy,
) -> Result<CommitFilesResult> {
    if opts.skip_merge_patches && opts.detail == DiffDetail::Patch && work.parent_count > 1 {
        return Ok(CommitFilesResult {
            output: DiffFilesOutput {
                files: Vec::new(),
                totals: None,
            },
            parent_count: work.parent_count,
        });
    }

    let odb = diff.repository().odb();
    let mut entries = diff_trees_with_renames_and_policy(
        odb,
        work.parent_tree.as_ref(),
        Some(&work.tree),
        "",
        rename_threshold(&opts.diff_options),
        config,
        thread_policy,
    )?;
    if rename_threshold(&opts.diff_options) >= 100 {
        entries.sort_by(|a, b| a.path().cmp(b.path()));
    }
    entries = filter_entries(diff.repository(), entries, pathspecs);
    entries = apply_diff_filter(entries, opts.diff_options.filter);
    let output = build_file_diffs(
        odb,
        entries,
        opts.detail,
        opts.diff_options,
        config,
        thread_policy,
    )?;
    Ok(CommitFilesResult {
        output,
        parent_count: work.parent_count,
    })
}

impl Diff<'_> {
    /// Build per-commit file diffs for a slice of commit oids, preserving input order.
    ///
    /// Parent trees are resolved once and reused across consecutive commits (same as
    /// [`super::write_commits_log_patch`]). Work runs in parallel when [`ThreadPolicy`] allows.
    ///
    /// # Errors
    ///
    /// Propagates object read, tree diff, and parallel pool errors.
    pub fn commit_files_many(
        &self,
        oids: &[ObjectId],
        pathspecs: &[String],
        opts: CommitFilesManyOptions,
        config: &ConfigSet,
        thread_policy: &ThreadPolicy,
    ) -> Result<Vec<CommitFilesResult>> {
        if oids.is_empty() {
            return Ok(Vec::new());
        }
        let repo = self.repository();
        let odb = repo.odb();
        let mut trees_by_commit: HashMap<ObjectId, ObjectId> = HashMap::with_capacity(oids.len());
        let mut parsed: Vec<(ObjectId, Vec<ObjectId>, ObjectId)> = Vec::with_capacity(oids.len());
        for oid in oids {
            let raw = odb.read(oid)?;
            let commit = parse_commit(&raw.data)?;
            trees_by_commit.insert(*oid, commit.tree);
            parsed.push((*oid, commit.parents.clone(), commit.tree));
        }

        let mut work_items: Vec<CommitWork> = Vec::with_capacity(oids.len());
        for (_oid, parents, tree) in parsed {
            let parent_count = parents.len();
            let parent_tree = match parents.first() {
                None => None,
                Some(parent) => Some(parent_tree_for_commit(odb, parent, &trees_by_commit)?),
            };
            work_items.push(CommitWork {
                tree,
                parent_tree,
                parent_count,
            });
        }

        let par_opts = thread_policy.resolve(ParallelOp::DiffContent, config);
        try_par_map(&work_items, par_opts, 0, |work| {
            diff_one_commit(self, work, pathspecs, &opts, config, thread_policy)
        })
        .map_err(|err| match err {
            ParallelHashError::Task(e) => e,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::{serialize_commit, CommitData, ObjectKind, TreeEntry};
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

    fn write_commit(
        repo: &Repository,
        tree: ObjectId,
        parents: Vec<ObjectId>,
        msg: &str,
    ) -> ObjectId {
        let ident = "A <a@example.com> 0 +0000";
        let commit = CommitData {
            tree,
            parents,
            author: ident.into(),
            committer: ident.into(),
            author_raw: Vec::new(),
            committer_raw: Vec::new(),
            encoding: None,
            message: format!("{msg}\n"),
            raw_message: None,
            extra_headers: Vec::new(),
        };
        repo.odb()
            .write(ObjectKind::Commit, &serialize_commit(&commit))
            .expect("commit")
    }

    #[test]
    fn commit_files_many_matches_single_commit_calls() -> Result<()> {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path(), &InitOptions::default())?;
        let tree1 = tree_with(&repo, &[("f.txt", b"a\n")]);
        let root = write_commit(&repo, tree1, vec![], "root");
        let tree2 = tree_with(&repo, &[("f.txt", b"b\n")]);
        let child = write_commit(&repo, tree2, vec![root], "child");

        let config = ConfigSet::default();
        let policy = ThreadPolicy::default();
        let diff_options = DiffOptions::default();
        let opts = CommitFilesManyOptions {
            diff_options,
            detail: DiffDetail::Stat,
            skip_merge_patches: true,
        };

        let many = repo
            .diff()
            .commit_files_many(&[child], &[], opts.clone(), &config, &policy)?;
        assert_eq!(many.len(), 1);

        let entries = diff_trees_with_renames_and_policy(
            repo.odb(),
            Some(&tree1),
            Some(&tree2),
            "",
            101,
            &config,
            &policy,
        )?;
        let single = build_file_diffs(
            repo.odb(),
            entries,
            opts.detail,
            opts.diff_options,
            &config,
            &policy,
        )?;
        assert_eq!(many[0].output.files, single.files);

        let policy_serial = ThreadPolicy::with_threads_override(Some(1));
        let many_par =
            repo.diff()
                .commit_files_many(&[child], &[], opts.clone(), &config, &policy)?;
        let many_serial =
            repo.diff()
                .commit_files_many(&[child], &[], opts, &config, &policy_serial)?;
        assert_eq!(many_par[0].output, many_serial[0].output);
        Ok(())
    }
}
