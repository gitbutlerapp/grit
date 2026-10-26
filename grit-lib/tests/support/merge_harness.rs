//! Oracle helpers: compare [`merge_trees_three_way`] with `git merge-tree --write-tree`.

#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::merge_file::MergeFavor;
use grit_lib::diff::merge_trees::{
    merge_trees_three_way, TheirsConflictLabel, TreeMergeConflictPresentation, TreeMergeOutput,
    WhitespaceMergeOptions,
};
use grit_lib::objects::{parse_commit, ObjectId, ObjectKind};
use grit_lib::repo::Repository;

use super::history_harness::{git, git_version_at_least, skip_unless_git_at_least, HistoryRepo};
use super::refs_harness::{git as refs_git, null_device_for_tests};

/// One unmerged index row emitted by `git merge-tree --write-tree` after the result tree line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitUnmergedRow {
    pub mode: u32,
    pub oid: ObjectId,
    pub stage: u32,
    pub path: String,
}

/// Parsed output of `git merge-tree --write-tree`.
#[derive(Debug, Clone)]
pub struct GitMergeTreeOracle {
    pub result_tree: ObjectId,
    pub unmerged: Vec<GitUnmergedRow>,
    pub stdout: String,
}

/// Run `git merge-tree --write-tree` for two commits (optional merge-base commit and `-X`).
pub fn git_merge_tree_oracle(
    worktree: &Path,
    merge_base: Option<&ObjectId>,
    ours: &ObjectId,
    theirs: &ObjectId,
    favor: MergeFavor,
) -> GitMergeTreeOracle {
    git_merge_tree_oracle_with_options(worktree, merge_base, ours, theirs, favor, false)
}

pub fn git_merge_tree_oracle_with_options(
    worktree: &Path,
    merge_base: Option<&ObjectId>,
    ours: &ObjectId,
    theirs: &ObjectId,
    favor: MergeFavor,
    allow_unrelated: bool,
) -> GitMergeTreeOracle {
    let mut cmd = Command::new("git");
    cmd.current_dir(worktree)
        .args(["merge-tree", "--write-tree"]);
    if allow_unrelated {
        cmd.arg("--allow-unrelated-histories");
    }
    match favor {
        MergeFavor::Ours => {
            cmd.arg("-Xours");
        }
        MergeFavor::Theirs => {
            cmd.arg("-Xtheirs");
        }
        MergeFavor::None | MergeFavor::Union => {}
    }
    if let Some(base) = merge_base {
        cmd.arg(format!("--merge-base={}", base.to_hex()));
    }
    cmd.args([&ours.to_hex(), &theirs.to_hex()]);
    let out = cmd
        .env("GIT_CONFIG_GLOBAL", null_device_for_tests())
        .env("GIT_CONFIG_SYSTEM", null_device_for_tests())
        .output()
        .expect("spawn git merge-tree");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut lines = stdout.lines().filter(|l| !l.is_empty());
    let tree_line = lines.next().unwrap_or_else(|| {
        panic!(
            "merge-tree missing tree oid:\n{stdout}\nstderr={}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    let result_tree: ObjectId = tree_line
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("invalid merge-tree tree oid: {tree_line}"));
    let mut unmerged = Vec::new();
    for line in lines {
        let mut parts = line.split_whitespace();
        let Some(mode_token) = parts.next() else {
            continue;
        };
        let Ok(mode) = u32::from_str_radix(mode_token, 8) else {
            continue;
        };
        if !matches!(mode, 0o100644 | 0o100755 | 0o120000 | 0o160000 | 0o040000) {
            continue;
        }
        let oid: ObjectId = match parts.next() {
            Some(s) => s
                .parse()
                .unwrap_or_else(|_| panic!("invalid oid in merge-tree line: {line}")),
            None => continue,
        };
        let stage: u32 = match parts.next() {
            Some(s) => s
                .parse()
                .unwrap_or_else(|_| panic!("invalid stage in merge-tree line: {line}")),
            None => continue,
        };
        let path = parts.collect::<Vec<_>>().join(" ");
        unmerged.push(GitUnmergedRow {
            mode,
            oid,
            stage,
            path,
        });
    }
    unmerged.sort_by(|a, b| a.path.cmp(&b.path).then(a.stage.cmp(&b.stage)));
    GitMergeTreeOracle {
        result_tree,
        unmerged,
        stdout,
    }
}

/// Build the merged result tree OID from grit output (mirrors `git merge-tree --write-tree`).
pub fn grit_merge_result_tree(
    repo: &Repository,
    merged: &TreeMergeOutput,
) -> grit_lib::error::Result<ObjectId> {
    grit_lib::diff::merge_trees::materialized_merge_result_tree(repo.odb(), merged)
}

pub fn grit_unmerged_rows(merged: &TreeMergeOutput) -> Vec<GitUnmergedRow> {
    let mut rows = Vec::new();
    for entry in merged.index.entries() {
        let stage = entry.stage();
        if stage == 0 {
            continue;
        }
        rows.push(GitUnmergedRow {
            mode: entry.mode,
            oid: entry.oid,
            stage: u32::from(stage),
            path: String::from_utf8_lossy(&entry.path).into_owned(),
        });
    }
    rows.sort_by(|a, b| a.path.cmp(&b.path).then(a.stage.cmp(&b.stage)));
    rows
}

pub fn tree_blob_map(repo: &Repository, tree: &ObjectId) -> BTreeMap<String, ObjectId> {
    let worktree = repo.work_tree().expect("worktree");
    let out = refs_git(worktree, &["ls-tree", "-r", &tree.to_hex()]);
    let mut map = BTreeMap::new();
    for line in out.lines() {
        let mut parts = line.split_whitespace();
        let _mode = parts.next();
        let kind = parts.next().unwrap_or("");
        if kind != "blob" {
            continue;
        }
        let oid: ObjectId = parts.next().unwrap_or("").parse().expect("oid");
        let path = parts.collect::<Vec<_>>().join(" ");
        map.insert(path, oid);
    }
    map
}

pub fn git_fsck_strict(worktree: &Path) {
    let out = Command::new("git")
        .current_dir(worktree)
        .args(["fsck", "--strict"])
        .env("GIT_CONFIG_GLOBAL", null_device_for_tests())
        .env("GIT_CONFIG_SYSTEM", null_device_for_tests())
        .output()
        .expect("git fsck");
    assert!(
        out.status.success(),
        "git fsck --strict failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Compare grit three-way merge against `git merge-tree --write-tree` for commit triples.
pub fn assert_merge_commits_match_git(
    history: &HistoryRepo,
    grit_repo: &Repository,
    merge_base: &ObjectId,
    ours: &ObjectId,
    theirs: &ObjectId,
    presentation: TreeMergeConflictPresentation<'_>,
    context: &str,
) {
    assert_merge_commits_match_git_with_favor(
        history,
        grit_repo,
        merge_base,
        ours,
        theirs,
        presentation,
        MergeFavor::None,
        context,
    );
}

pub fn assert_merge_commits_match_git_with_favor(
    history: &HistoryRepo,
    grit_repo: &Repository,
    merge_base: &ObjectId,
    ours: &ObjectId,
    theirs: &ObjectId,
    presentation: TreeMergeConflictPresentation<'_>,
    favor: MergeFavor,
    context: &str,
) {
    if !git_version_at_least(2, 38) {
        eprintln!("SKIP: git merge-tree --write-tree requires git >= 2.38 ({context})");
        return;
    }
    let oracle = git_merge_tree_oracle(history.worktree(), Some(merge_base), ours, theirs, favor);
    let base_tree = commit_tree(grit_repo, merge_base).expect("base tree");
    let ours_tree = commit_tree(grit_repo, ours).expect("ours tree");
    let theirs_tree = commit_tree(grit_repo, theirs).expect("theirs tree");
    let presentation = TreeMergeConflictPresentation {
        df_directory_ours_commit: presentation.df_directory_ours_commit.or(Some(*ours)),
        df_directory_theirs_commit: presentation.df_directory_theirs_commit.or(Some(*theirs)),
        ..presentation
    };
    let merged = merge_trees_three_way(
        grit_repo,
        base_tree,
        ours_tree,
        theirs_tree,
        favor,
        WhitespaceMergeOptions::default(),
        None,
        presentation,
    )
    .expect("merge_trees_three_way");
    assert_merge_output_matches_oracle(grit_repo, &merged, &oracle, context);
    git_fsck_strict(history.worktree());
}

pub fn assert_merge_output_matches_oracle(
    repo: &Repository,
    merged: &TreeMergeOutput,
    oracle: &GitMergeTreeOracle,
    context: &str,
) {
    let grit_tree = grit_merge_result_tree(repo, merged).expect("grit result tree");
    let grit_rows = grit_unmerged_rows(merged);
    assert_eq!(
        grit_tree, oracle.result_tree,
        "{context}: result tree oid mismatch"
    );
    assert_eq!(
        grit_rows, oracle.unmerged,
        "{context}: unmerged index stages mismatch"
    );
    let grit_blobs = tree_blob_map(repo, &grit_tree);
    let git_blobs = tree_blob_map(repo, &oracle.result_tree);
    assert_eq!(grit_blobs, git_blobs, "{context}: tree blob map mismatch");
    for (path, oid) in &grit_blobs {
        if oracle
            .unmerged
            .iter()
            .any(|row| row.path == *path && row.stage != 0)
        {
            if let Some(marker) = merged.conflict_content.get(path.as_bytes()) {
                let grit_body = repo.odb().read(oid).expect("read grit blob").data;
                let marker_body = repo.odb().read(marker).expect("read marker").data;
                assert_eq!(
                    grit_body, marker_body,
                    "{context}: conflict marker blob for {path}"
                );
            }
        }
    }
}

pub fn commit_tree(repo: &Repository, commit_oid: &ObjectId) -> grit_lib::error::Result<ObjectId> {
    let obj = repo.odb().read(commit_oid)?;
    Ok(parse_commit(&obj.data)?.tree)
}

/// Open the same repo path as `HistoryRepo` for grit.
pub fn open_grit_repo(history: &HistoryRepo) -> Repository {
    Repository::discover(Some(history.worktree()), &RepositoryOptions::empty())
        .expect("discover grit repo")
}

/// Labels matching `git merge-tree` when invoked with raw commit object ids.
pub struct CommitMergeLabels {
    pub ours: String,
    pub theirs: String,
}

impl CommitMergeLabels {
    #[must_use]
    pub fn from_commits(ours: &ObjectId, theirs: &ObjectId) -> Self {
        Self {
            ours: ours.to_hex(),
            theirs: theirs.to_hex(),
        }
    }

    #[must_use]
    pub fn presentation(&self) -> TreeMergeConflictPresentation<'_> {
        TreeMergeConflictPresentation {
            label_ours: &self.ours,
            label_theirs: TheirsConflictLabel::Fixed(&self.theirs),
            label_base: "merged common ancestors",
            style: grit_lib::diff::merge_file::ConflictStyle::Merge,
            checkout_merge: false,
            df_directory_ours_commit: None,
            df_directory_theirs_commit: None,
        }
    }
}

pub fn git_commit_tree(
    worktree: &Path,
    tree: &ObjectId,
    parents: &[ObjectId],
    msg: &str,
) -> ObjectId {
    let tree_hex = tree.to_hex();
    let parent_hex: Vec<String> = parents.iter().map(ObjectId::to_hex).collect();
    let mut args: Vec<String> = vec!["commit-tree".into(), tree_hex, "-m".into(), msg.to_owned()];
    for hex in &parent_hex {
        args.push("-p".into());
        args.push(hex.clone());
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    git(worktree, &arg_refs)
        .trim()
        .parse()
        .expect("commit-tree")
}

pub fn git_hash_object(worktree: &Path, path: &str) -> ObjectId {
    git(worktree, &["hash-object", "-w", "--", path])
        .trim()
        .parse()
        .expect("hash-object")
}

pub fn skip_merge_tree_git() -> bool {
    skip_unless_git_at_least(2, 38)
}

pub fn empty_tree(repo: &Repository) -> ObjectId {
    repo.odb().write(ObjectKind::Tree, &[]).expect("empty tree")
}
