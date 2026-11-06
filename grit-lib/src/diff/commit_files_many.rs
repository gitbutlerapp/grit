//! Per-commit file diffs for many commits (used by `grit log -p` / `--stat`).

use std::collections::HashMap;

use crate::config::ConfigSet;
use crate::error::Result;
use crate::objects::{parse_commit, ObjectId};
use crate::odb::Odb;
use crate::parallel::{try_par_map, ParallelHashError, ParallelOp, ParallelOptions, ThreadPolicy};

use super::core::diff_trees_with_renames_for_pathspecs_and_policy;
use super::detail::{DiffDetail, DiffTotals, FileDiff, LineStat};
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
    /// When set, merge commits (2+ parents) produce empty file lists for every detail level
    /// (like `git log` without `-m`, `-c`, or `--cc`).
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

fn totals_from_patch_files(files: &[FileDiff]) -> DiffTotals {
    let lines: LineStat = files
        .iter()
        .filter_map(|file| {
            file.structured
                .as_ref()
                .map(|structured| LineStat::from_hunks(&structured.hunks))
        })
        .sum();
    DiffTotals {
        files_changed: files.len(),
        insertions: lines.insertions,
        deletions: lines.deletions,
    }
}

fn totals_for_detail(files: &[FileDiff], detail: DiffDetail) -> Option<DiffTotals> {
    match detail {
        DiffDetail::Stat => Some(DiffTotals::from_files(files)),
        DiffDetail::Patch => Some(totals_from_patch_files(files)),
        DiffDetail::NameOnly | DiffDetail::NameStatus => None,
    }
}

fn empty_merge_output(detail: DiffDetail) -> DiffFilesOutput {
    let totals = match detail {
        DiffDetail::NameOnly | DiffDetail::NameStatus => None,
        DiffDetail::Stat | DiffDetail::Patch => Some(DiffTotals {
            files_changed: 0,
            insertions: 0,
            deletions: 0,
        }),
    };
    DiffFilesOutput {
        files: Vec::new(),
        totals,
        inexact_rename_skipped: None,
    }
}

/// Parallel options for diffing many commits in one batch.
///
/// Honors [`ThreadPolicy::threads_override`] `Some(1)` (serial). When the caller did not
/// cap threads, uses auto worker count so multi-commit batches can run in parallel like
/// Git's `log -p`. Per-commit work still uses the caller's original policy.
fn commit_files_many_parallel_options(
    thread_policy: &ThreadPolicy,
    config: &ConfigSet,
) -> ParallelOptions {
    let batch_policy = if thread_policy.threads_override == Some(1) {
        *thread_policy
    } else {
        ThreadPolicy {
            threads_override: thread_policy.threads_override.or(Some(0)),
            min_items_override: thread_policy.min_items_override,
            min_total_bytes_override: thread_policy.min_total_bytes_override,
        }
    };
    let mut par_opts = batch_policy.resolve(ParallelOp::DiffContent, config);
    par_opts.min_items = par_opts.min_items.min(2);
    par_opts.min_total_bytes = None;
    par_opts
}

fn diff_one_commit(
    diff: &Diff,
    work: &CommitWork,
    pathspecs: &[String],
    opts: &CommitFilesManyOptions,
    config: &ConfigSet,
    thread_policy: &ThreadPolicy,
) -> Result<CommitFilesResult> {
    if opts.skip_merge_patches && work.parent_count > 1 {
        return Ok(CommitFilesResult {
            output: empty_merge_output(opts.detail),
            parent_count: work.parent_count,
        });
    }

    let repo = diff.repository();
    let odb = repo.odb();
    let pathspec_matcher = if pathspecs.is_empty() {
        None
    } else {
        Some(repo.pathspec_matcher(pathspecs)?)
    };
    let mut entries = diff_trees_with_renames_for_pathspecs_and_policy(
        odb,
        work.parent_tree.as_ref(),
        Some(&work.tree),
        "",
        pathspecs,
        pathspec_matcher.as_ref(),
        rename_threshold(&opts.diff_options),
        config,
        thread_policy,
    )?;
    if rename_threshold(&opts.diff_options) >= 100 {
        super::sort_entries_by_path(&mut entries);
    }
    entries = filter_entries(diff.repository(), entries, pathspecs)?;
    let mut output = build_file_diffs(
        odb,
        entries,
        opts.detail,
        opts.diff_options
            .clone()
            .with_renames(crate::diff::RenameDetection::Off),
        config,
        thread_policy,
    )?;
    if output.totals.is_none() {
        output.totals = totals_for_detail(&output.files, opts.detail);
    }
    Ok(CommitFilesResult {
        output,
        parent_count: work.parent_count,
    })
}

/// One commit's identity for [`Diff::commit_files_many_work`], avoiding a second ODB read.
#[derive(Debug, Clone)]
pub struct CommitFilesManyInput {
    /// Commit object id (order matches the log walk).
    pub oid: ObjectId,
    /// First-parent chain and other parents from the commit object.
    pub parents: Vec<ObjectId>,
    /// Tree object id for this commit.
    pub tree: ObjectId,
}

impl Diff<'_> {
    /// Build per-commit file diffs for a slice of commit oids, preserving input order.
    ///
    /// Parent trees are resolved once and reused across consecutive commits (same as
    /// [`super::write_commits_log_patch`]). When `oids` are newest-first and history is
    /// linear, each commit's first parent is the next oid and its tree is reused without
    /// an extra read. Work runs in parallel when [`ThreadPolicy`] allows and the batch is
    /// large enough to amortize pool startup.
    ///
    /// # Errors
    ///
    /// Propagates object read, tree diff, and parallel pool errors.
    pub fn commit_files_many(
        &self,
        oids: &[ObjectId],
        pathspec: &crate::pathspec::Pathspec,
        opts: CommitFilesManyOptions,
        config: &ConfigSet,
        thread_policy: &ThreadPolicy,
    ) -> Result<Vec<CommitFilesResult>> {
        if oids.is_empty() {
            return Ok(Vec::new());
        }
        let repo = self.repository();
        let odb = repo.odb();
        let parent_ctx = crate::shallow::EffectiveCommitParents::load(repo);
        let inputs = oids
            .iter()
            .map(|oid| {
                let commit = odb.read_commit(oid)?;
                Ok(CommitFilesManyInput {
                    oid: *oid,
                    parents: parent_ctx.for_commit(*oid, &commit.parents),
                    tree: commit.tree,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        self.commit_files_many_work(&inputs, pathspec, opts, config, thread_policy)
    }

    /// Lines each commit added and removed against its first parent, in input order.
    ///
    /// This is the per-commit summary of `git log --shortstat`: a root commit is compared with
    /// the empty tree, and a merge (two or more parents) is not diffed at all and gets `None`.
    /// Commits that change no lines (or only binary files) get `Some` totals with zero
    /// insertions and deletions, so callers decide whether to show them.
    ///
    /// # Parameters
    ///
    /// - `oids`: commits to summarize; newest-first linear history reuses each parent tree.
    /// - `pathspecs`: resolved pathspecs limiting the counted paths (empty counts every path).
    /// - `diff_options`: rename and line-diff settings, usually [`DiffOptions::from_config`].
    /// - `config`: repository configuration for attributes and parallelism.
    /// - `thread_policy`: worker limits for the batch.
    ///
    /// # Errors
    ///
    /// Propagates object read (including a non-commit `oid`), tree diff, and parallel pool
    /// errors.
    pub fn commit_line_totals(
        &self,
        oids: &[ObjectId],
        pathspec: &crate::pathspec::Pathspec,
        diff_options: DiffOptions,
        config: &ConfigSet,
        thread_policy: &ThreadPolicy,
    ) -> Result<Vec<Option<DiffTotals>>> {
        let repo = self.repository();
        let odb = repo.odb();
        let parent_ctx = crate::shallow::EffectiveCommitParents::load(repo);
        let mut inputs = Vec::with_capacity(oids.len());
        let mut slots = Vec::with_capacity(oids.len());
        for oid in oids {
            let commit = odb.read_commit(oid)?;
            let parents = parent_ctx.for_commit(*oid, &commit.parents);
            if parents.len() > 1 {
                slots.push(None);
                continue;
            }
            slots.push(Some(inputs.len()));
            inputs.push(CommitFilesManyInput {
                oid: *oid,
                parents,
                tree: commit.tree,
            });
        }
        let opts = CommitFilesManyOptions {
            diff_options,
            detail: DiffDetail::Stat,
            skip_merge_patches: true,
        };
        let results =
            self.commit_files_many_work(&inputs, pathspec, opts, config, thread_policy)?;
        Ok(slots
            .into_iter()
            .map(|slot| {
                let output = &results.get(slot?)?.output;
                Some(
                    output
                        .totals
                        .unwrap_or_else(|| DiffTotals::from_files(&output.files)),
                )
            })
            .collect())
    }

    /// Like [`Self::commit_files_many`] when commit trees and parents are already loaded.
    ///
    /// # Errors
    ///
    /// Propagates object read, tree diff, and parallel pool errors.
    pub fn commit_files_many_work(
        &self,
        inputs: &[CommitFilesManyInput],
        pathspec: &crate::pathspec::Pathspec,
        opts: CommitFilesManyOptions,
        config: &ConfigSet,
        thread_policy: &ThreadPolicy,
    ) -> Result<Vec<CommitFilesResult>> {
        let pathspecs = pathspec.specs();
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let repo = self.repository();
        let odb = repo.odb();
        let mut trees_by_commit: HashMap<ObjectId, ObjectId> = HashMap::with_capacity(inputs.len());
        for input in inputs {
            trees_by_commit.insert(input.oid, input.tree);
        }

        let mut work_items: Vec<CommitWork> = Vec::with_capacity(inputs.len());
        for (i, input) in inputs.iter().enumerate() {
            let parent_count = input.parents.len();
            let parent_tree = if parent_count == 0 {
                None
            } else if i + 1 < inputs.len() && input.parents[0] == inputs[i + 1].oid {
                Some(inputs[i + 1].tree)
            } else {
                Some(parent_tree_for_commit(
                    odb,
                    &input.parents[0],
                    &trees_by_commit,
                )?)
            };
            work_items.push(CommitWork {
                tree: input.tree,
                parent_tree,
                parent_count,
            });
        }

        odb.with_pack_read_context(|| {
            let par_opts = commit_files_many_parallel_options(thread_policy, config);
            try_par_map(&work_items, par_opts, 0, |work| {
                diff_one_commit(self, work, pathspecs, &opts, config, thread_policy)
            })
            .map_err(|err| match err {
                ParallelHashError::Task(e) => e,
            })
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
        let (repo, _) = Repository::init(dir.path(), &InitOptions::default())?;
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

        let many =
            repo.diff().commit_files_many(
                &[child],
                &crate::pathspec::Pathspec::entire_tree(
                    crate::pathspec::PathspecGlobalFlags::default(),
                ),
                opts.clone(),
                &config,
                &policy,
            )?;
        assert_eq!(many.len(), 1);

        let entries = diff_trees_with_renames_for_pathspecs_and_policy(
            repo.odb(),
            Some(&tree1),
            Some(&tree2),
            "",
            &[],
            None,
            101,
            &config,
            &policy,
        )?;
        let single = build_file_diffs(
            repo.odb(),
            entries,
            opts.detail,
            opts.diff_options
                .clone()
                .with_renames(crate::diff::RenameDetection::Off),
            &config,
            &policy,
        )?;
        assert_eq!(many[0].output.files, single.files);

        let policy_serial = ThreadPolicy::with_threads_override(Some(1));
        let policy_par = ThreadPolicy::with_threads_override(Some(4))
            .min_items_override(Some(1))
            .min_total_bytes_override(Some(0));
        let tree3 = tree_with(&repo, &[("f.txt", b"c\n")]);
        let mid = write_commit(&repo, tree3, vec![child], "mid");
        let tree4 = tree_with(&repo, &[("f.txt", b"d\n")]);
        let tip = write_commit(&repo, tree4, vec![mid], "tip");
        let batch = [tip, mid, child];
        let many_par =
            repo.diff().commit_files_many(
                &batch,
                &crate::pathspec::Pathspec::entire_tree(
                    crate::pathspec::PathspecGlobalFlags::default(),
                ),
                opts.clone(),
                &config,
                &policy_par,
            )?;
        let many_serial =
            repo.diff().commit_files_many(
                &batch,
                &crate::pathspec::Pathspec::entire_tree(
                    crate::pathspec::PathspecGlobalFlags::default(),
                ),
                opts,
                &config,
                &policy_serial,
            )?;
        assert_eq!(many_par.len(), 3);
        assert_eq!(many_par, many_serial);
        Ok(())
    }

    #[test]
    fn commit_line_totals_skip_merges_and_honor_pathspecs() -> Result<()> {
        let dir = tempfile::tempdir().expect("tempdir");
        let (repo, _) = Repository::init(dir.path(), &InitOptions::default())?;
        let root_tree = tree_with(&repo, &[("a.txt", b"1\n2\n"), ("b.txt", b"x\n")]);
        let root = write_commit(&repo, root_tree, vec![], "root");
        let edit_tree = tree_with(&repo, &[("a.txt", b"1\nchanged\n3\n"), ("b.txt", b"x\n")]);
        let edit = write_commit(&repo, edit_tree, vec![root], "edit a");
        let side_tree = tree_with(&repo, &[("a.txt", b"1\n2\n"), ("b.txt", b"y\n")]);
        let side = write_commit(&repo, side_tree, vec![root], "edit b");
        let merge = write_commit(&repo, edit_tree, vec![edit, side], "merge");
        let binary_tree = tree_with(
            &repo,
            &[
                ("a.txt", b"1\nchanged\n3\n"),
                ("b.txt", b"x\n"),
                ("c.bin", b"\0"),
            ],
        );
        let binary = write_commit(&repo, binary_tree, vec![merge], "add binary");
        let oids = [binary, merge, side, edit, root];

        let config = ConfigSet::default();
        let totals =
            repo.diff().commit_line_totals(
                &oids,
                &crate::pathspec::Pathspec::entire_tree(
                    crate::pathspec::PathspecGlobalFlags::default(),
                ),
                DiffOptions::default(),
                &config,
                &ThreadPolicy::default(),
            )?;
        let lines = |t: Option<DiffTotals>| t.map(|t| (t.files_changed, t.insertions, t.deletions));
        assert_eq!(
            totals.into_iter().map(lines).collect::<Vec<_>>(),
            vec![
                Some((1, 0, 0)),
                None,
                Some((1, 1, 1)),
                Some((1, 2, 1)),
                Some((2, 3, 0)),
            ]
        );

        let only_b = repo.diff().commit_line_totals(
            &oids,
            &crate::pathspec::Pathspec::scoped(
                ["b.txt"],
                crate::pathspec::PathspecGlobalFlags::default(),
            ),
            DiffOptions::default(),
            &config,
            &ThreadPolicy::with_threads_override(Some(1)),
        )?;
        assert_eq!(
            only_b.into_iter().map(lines).collect::<Vec<_>>(),
            vec![
                Some((0, 0, 0)),
                None,
                Some((1, 1, 1)),
                Some((0, 0, 0)),
                Some((1, 1, 0)),
            ]
        );

        assert!(repo
            .diff()
            .commit_line_totals(
                &[root_tree],
                &crate::pathspec::Pathspec::entire_tree(
                    crate::pathspec::PathspecGlobalFlags::default()
                ),
                DiffOptions::default(),
                &config,
                &ThreadPolicy::default(),
            )
            .is_err());
        Ok(())
    }

    #[test]
    fn patch_totals_match_hunk_line_counts() -> Result<()> {
        let dir = tempfile::tempdir().expect("tempdir");
        let (repo, _) = Repository::init(dir.path(), &InitOptions::default())?;
        let tree1 = tree_with(&repo, &[("a.txt", b"one\n")]);
        let root = write_commit(&repo, tree1, vec![], "root");
        let tree2 = tree_with(&repo, &[("a.txt", b"one\ntwo\n")]);
        let child = write_commit(&repo, tree2, vec![root], "add line");

        let config = ConfigSet::default();
        let policy = ThreadPolicy::default();
        let opts = CommitFilesManyOptions {
            diff_options: DiffOptions::default(),
            detail: DiffDetail::Patch,
            skip_merge_patches: true,
        };
        let results =
            repo.diff().commit_files_many(
                &[child],
                &crate::pathspec::Pathspec::entire_tree(
                    crate::pathspec::PathspecGlobalFlags::default(),
                ),
                opts.clone(),
                &config,
                &policy,
            )?;
        let totals = results[0].output.totals.expect("patch totals");
        assert_eq!(
            (totals.files_changed, totals.insertions, totals.deletions),
            (1, 1, 0)
        );

        let name_status_opts = CommitFilesManyOptions {
            detail: DiffDetail::NameStatus,
            ..opts.clone()
        };
        let name_status =
            repo.diff().commit_files_many(
                &[child],
                &crate::pathspec::Pathspec::entire_tree(
                    crate::pathspec::PathspecGlobalFlags::default(),
                ),
                name_status_opts,
                &config,
                &policy,
            )?;
        assert!(name_status[0].output.totals.is_none());
        Ok(())
    }

    #[test]
    fn skip_merge_patches_empties_stat_and_name_modes() -> Result<()> {
        let dir = tempfile::tempdir().expect("tempdir");
        let (repo, _) = Repository::init(dir.path(), &InitOptions::default())?;
        let base_tree = tree_with(&repo, &[("a.txt", b"one\n")]);
        let base = write_commit(&repo, base_tree, vec![], "base");
        let side_tree = tree_with(&repo, &[("a.txt", b"one\n"), ("b.txt", b"side\n")]);
        let side = write_commit(&repo, side_tree, vec![base], "side");
        let main_tree = tree_with(&repo, &[("a.txt", b"one\ntwo\n")]);
        let main = write_commit(&repo, main_tree, vec![base], "main");
        let merge = write_commit(&repo, side_tree, vec![main, side], "merge");

        let config = ConfigSet::default();
        let policy = ThreadPolicy::default();
        let base_opts = CommitFilesManyOptions {
            diff_options: DiffOptions::default(),
            detail: DiffDetail::Stat,
            skip_merge_patches: true,
        };

        for detail in [
            DiffDetail::Patch,
            DiffDetail::Stat,
            DiffDetail::NameOnly,
            DiffDetail::NameStatus,
        ] {
            let opts = CommitFilesManyOptions {
                detail,
                ..base_opts.clone()
            };
            let results = repo.diff().commit_files_many(
                &[merge],
                &crate::pathspec::Pathspec::entire_tree(
                    crate::pathspec::PathspecGlobalFlags::default(),
                ),
                opts,
                &config,
                &policy,
            )?;
            assert!(
                results[0].output.files.is_empty(),
                "merge files should be empty for {detail:?}"
            );
            match detail {
                DiffDetail::NameOnly | DiffDetail::NameStatus => {
                    assert!(results[0].output.totals.is_none());
                }
                DiffDetail::Patch | DiffDetail::Stat => {
                    assert_eq!(
                        results[0].output.totals,
                        Some(DiffTotals {
                            files_changed: 0,
                            insertions: 0,
                            deletions: 0,
                        })
                    );
                }
            }
        }
        Ok(())
    }
}
