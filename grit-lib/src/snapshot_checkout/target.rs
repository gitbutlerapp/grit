//! Changes the server makes to its target branch: merging a checked-in
//! branch ([`integrate`]) and editing the slices file ([`commit_slices`]).
//! Both write a commit and move the target with a compare-and-swap, so a
//! concurrent change is refused rather than lost, and log the move in the
//! target's reflog where the repository keeps one.
//!
//! The target is the branch the served repository's `HEAD` names
//! ([`target_branch`]).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::merge_base::{
    merge_bases_first_vs_rest, walk_commits_reachable_excluding_ancestors_of, ReachableWalkLimit,
};
use crate::merge_file::MergeFavor;
use crate::merge_trees::{
    merge_trees_three_way, TreeMergeConflictPresentation, WhitespaceMergeOptions,
};
use crate::objects::{parse_commit, ObjectId, ObjectKind};
use crate::repo::Repository;

use super::refs::{swap_ref, Swap};
use super::tree::{edit, lookup};
use super::{CheckoutError, Result, Slice};

/// Where the slices live on the target branch.
pub const SLICES_PATH: &str = ".grit/slices.toml";

/// Who a server-made commit is by, and when (seconds since the epoch, UTC).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    /// `Name <email>`.
    pub ident: String,
    /// Seconds since the Unix epoch.
    pub when: i64,
}

impl Signature {
    /// `Name <email> <seconds> +0000`, as commit headers and reflogs have it.
    pub(crate) fn line(&self) -> String {
        format!("{} {} +0000", self.ident, self.when)
    }
}

/// The target branch of a served repository: the branch its `HEAD` names,
/// without `refs/heads/`, or `main` when `HEAD` names no branch.
///
/// # Parameters
///
/// - `repo`: the served repository.
///
/// # Returns
///
/// The branch name; the branch itself may not exist yet.
#[must_use]
pub fn target_branch(repo: &Repository) -> String {
    crate::refs::read_symbolic_ref(repo.git_dir(), "HEAD")
        .ok()
        .flatten()
        .and_then(|head| head.strip_prefix("refs/heads/").map(str::to_owned))
        .unwrap_or_else(|| "main".to_owned())
}

/// Whether branch names `a` and `b` are the same apart from case, so a
/// case-insensitive file system (macOS, Windows) keeps them in one file.
///
/// # Parameters
///
/// - `a`, `b`: branch names without `refs/heads/`.
#[must_use]
pub fn same_branch_name(a: &str, b: &str) -> bool {
    a == b || a.to_lowercase() == b.to_lowercase()
}

/// Why the target could not be changed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TargetRefusal {
    /// The branch does not merge cleanly into the target.
    #[error("{branch} conflicts with {target} in {}; rebase it onto the newest {target} and try again", paths.join(", "))]
    Conflict {
        /// The branch.
        branch: String,
        /// The target.
        target: String,
        /// The conflicting paths.
        paths: Vec<String>,
    },
    /// The target moved while the change was made.
    #[error("{target} moved while it was being changed; try again")]
    Moved {
        /// The target.
        target: String,
    },
}

/// What [`integrate`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Integrated {
    /// The target already has every commit of the branch.
    AlreadyIntegrated,
    /// A merge commit moved the target.
    Merged {
        /// The merge commit.
        head: ObjectId,
        /// The branch's commits it brought in.
        commits: usize,
    },
}

fn write_commit(
    repo: &Repository,
    tree: ObjectId,
    parents: &[ObjectId],
    who: &Signature,
    message: &str,
) -> crate::error::Result<ObjectId> {
    let mut s = format!("tree {}\n", tree.to_hex());
    for p in parents {
        s.push_str(&format!("parent {}\n", p.to_hex()));
    }
    let line = who.line();
    s.push_str(&format!("author {line}\ncommitter {line}\n\n{message}\n"));
    repo.odb().write(ObjectKind::Commit, s.as_bytes())
}

/// Move `target_ref` from `previous` to `new`, unless it moved meanwhile,
/// logging the move by `who` with `message`.
fn move_target(
    repo: &Repository,
    target_ref: &str,
    previous: ObjectId,
    new: ObjectId,
    who: &Signature,
    message: &str,
) -> Result<std::result::Result<(), TargetRefusal>> {
    Ok(
        match swap_ref(repo, target_ref, Some(previous), new, who, message)? {
            Swap::Done => Ok(()),
            Swap::Moved => Err(TargetRefusal::Moved {
                target: target_ref.trim_start_matches("refs/heads/").to_owned(),
            }),
        },
    )
}

/// Merge `branch` (named `name`) into the target branch `target_ref`, at
/// `previous`, with a merge commit by `who`, when the merge is clean. The
/// move is logged as `integrate: <merge message>` where the repository keeps
/// a reflog for the target.
///
/// # Returns
///
/// `Ok(Err(..))` when the merge conflicts or the target moved.
///
/// # Errors
///
/// Object and ref failures.
pub fn integrate(
    repo: &Repository,
    target_ref: &str,
    previous: ObjectId,
    branch: ObjectId,
    name: &str,
    who: &Signature,
) -> Result<std::result::Result<Integrated, TargetRefusal>> {
    if branch == previous || crate::merge_base::is_ancestor(repo, branch, previous)? {
        return Ok(Ok(Integrated::AlreadyIntegrated));
    }
    let odb = repo.odb();
    let target_tree = parse_commit(&odb.read(&previous)?.data)?.tree;
    let branch_tree = parse_commit(&odb.read(&branch)?.data)?.tree;
    let base_tree = match merge_bases_first_vs_rest(repo, previous, &[branch])?.first() {
        Some(base) => parse_commit(&odb.read(base)?.data)?.tree,
        None => edit(odb, None, &[])?,
    };
    let target = target_ref.trim_start_matches("refs/heads/");
    let merged = merge_trees_three_way(
        repo,
        base_tree,
        target_tree,
        branch_tree,
        MergeFavor::default(),
        WhitespaceMergeOptions::default(),
        None,
        TreeMergeConflictPresentation::default(),
    )?;
    let mut conflicts: Vec<String> = merged
        .index
        .entries()
        .iter()
        .filter(|e| e.stage() != 0)
        .map(|e| String::from_utf8_lossy(&e.path).into_owned())
        .collect();
    conflicts.dedup();
    if !conflicts.is_empty() {
        return Ok(Err(TargetRefusal::Conflict {
            branch: name.to_owned(),
            target: target.to_owned(),
            paths: conflicts,
        }));
    }
    let (commits, _) = walk_commits_reachable_excluding_ancestors_of(
        repo,
        &[branch],
        &[previous],
        ReachableWalkLimit::Unlimited,
    )?;
    let message = format!("Merge branch '{name}' into {target}");
    let head = write_commit(repo, merged.result_tree, &[previous, branch], who, &message)?;
    Ok(move_target(
        repo,
        target_ref,
        previous,
        head,
        who,
        &format!("integrate: {message}"),
    )?
    .map(|()| Integrated::Merged { head, commits }))
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SlicesFile {
    #[serde(default)]
    slices: BTreeMap<String, SliceEntry>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SliceEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(default)]
    paths: Vec<String>,
}

/// Parse the slices file.
///
/// # Errors
///
/// [`CheckoutError::Protocol`] for text that is not a valid slices file.
pub fn parse_slices(text: &str) -> Result<Vec<Slice>> {
    let file: SlicesFile = toml::from_str(text).map_err(|e| CheckoutError::Protocol {
        url: SLICES_PATH.to_owned(),
        detail: e.to_string(),
    })?;
    Ok(file
        .slices
        .into_iter()
        .map(|(name, e)| Slice {
            name,
            description: e.description,
            paths: e.paths,
        })
        .collect())
}

/// Write the slices file's text.
#[must_use]
pub fn render_slices(slices: &[Slice]) -> String {
    let file = SlicesFile {
        slices: slices
            .iter()
            .map(|s| {
                (
                    s.name.clone(),
                    SliceEntry {
                        description: s.description.clone(),
                        paths: s.paths.clone(),
                    },
                )
            })
            .collect(),
    };
    let body = toml::to_string_pretty(&file).unwrap_or_default();
    format!("# Named sets of paths to check out together (`grit slice`).\n\n{body}")
}

/// The slices at `commit` (none when it has no slices file).
///
/// # Errors
///
/// Object read failures and a malformed slices file.
pub fn read_slices(repo: &Repository, commit: ObjectId) -> Result<Vec<Slice>> {
    let odb = repo.odb();
    let tree = parse_commit(&odb.read(&commit)?.data)?.tree;
    match lookup(odb, tree, SLICES_PATH.as_bytes())? {
        Some((_, blob)) => parse_slices(&String::from_utf8_lossy(&odb.read(&blob)?.data)),
        None => Ok(Vec::new()),
    }
}

/// Whether `name` can name a slice: letters, digits, `-`, `_` and `.`, not
/// starting with `.` or `-`.
#[must_use]
pub fn valid_slice_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with(['.', '-'])
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Paths among `paths` that `commit`'s tree doesn't have.
///
/// # Errors
///
/// Object read failures.
pub fn missing_paths(repo: &Repository, commit: ObjectId, paths: &[String]) -> Result<Vec<String>> {
    let odb = repo.odb();
    let tree = parse_commit(&odb.read(&commit)?.data)?.tree;
    let mut missing = Vec::new();
    for p in paths {
        if lookup(odb, tree, p.as_bytes())?.is_none() {
            missing.push(p.clone());
        }
    }
    Ok(missing)
}

/// Commit `slices` as the slices file on the target `target_ref` at
/// `previous`, by `who`, with `message`. The move is logged as
/// `slice: <message>` where the repository keeps a reflog for the target.
///
/// # Returns
///
/// The new target commit, `previous` when the file would not change, or
/// `Ok(Err(..))` when the target moved meanwhile.
///
/// # Errors
///
/// Object and ref failures.
pub fn commit_slices(
    repo: &Repository,
    target_ref: &str,
    previous: ObjectId,
    slices: &[Slice],
    who: &Signature,
    message: &str,
) -> Result<std::result::Result<ObjectId, TargetRefusal>> {
    let odb = repo.odb();
    let tree = parse_commit(&odb.read(&previous)?.data)?.tree;
    let entry = if slices.is_empty() {
        None
    } else {
        let blob = odb.write(ObjectKind::Blob, render_slices(slices).as_bytes())?;
        Some((0o100644, blob))
    };
    let new_tree = edit(odb, Some(tree), &[(SLICES_PATH.as_bytes().to_vec(), entry)])?;
    if new_tree == tree {
        return Ok(Ok(previous));
    }
    let head = write_commit(repo, new_tree, &[previous], who, message)?;
    Ok(move_target(
        repo,
        target_ref,
        previous,
        head,
        who,
        &format!("slice: {message}"),
    )?
    .map(|()| head))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slices_round_trip_through_toml() {
        let slices = vec![
            Slice {
                name: "desktop".to_owned(),
                description: Some("The desktop app".to_owned()),
                paths: vec!["apps/desktop".to_owned(), "packages/ui".to_owned()],
            },
            Slice {
                name: "docs".to_owned(),
                description: None,
                paths: vec!["docs".to_owned()],
            },
        ];
        let text = render_slices(&slices);
        assert!(text.contains("[slices.desktop]"), "{text}");
        assert_eq!(parse_slices(&text).ok(), Some(slices));
        assert_eq!(parse_slices("").ok(), Some(Vec::new()));
        assert!(parse_slices("slices = 3").is_err());
    }

    #[test]
    fn slice_names_are_plain() {
        assert!(valid_slice_name("desktop-ui_2.0"));
        assert!(!valid_slice_name(""));
        assert!(!valid_slice_name(".hidden"));
        assert!(!valid_slice_name("a/b"));
        assert!(!valid_slice_name("-x"));
    }

    #[test]
    fn the_target_is_heads_branch_and_names_compare_without_case() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(
            dir.path(),
            &crate::repo::InitOptions {
                initial_branch: Some("trunk".to_owned()),
                ..crate::repo::InitOptions::default()
            },
        )
        .expect("init");
        assert_eq!(target_branch(&repo), "trunk");
        crate::refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/main").expect("head");
        assert_eq!(target_branch(&repo), "main");
        for (a, b, same) in [
            ("main", "main", true),
            ("Main", "main", true),
            ("MAIN", "main", true),
            ("feature", "main", false),
            ("main-2", "main", false),
        ] {
            assert_eq!(same_branch_name(a, b), same, "{a} {b}");
        }
    }

    #[test]
    fn integrating_and_slices_log_the_target_move() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo =
            Repository::init(dir.path(), &crate::repo::InitOptions::default()).expect("init");
        let odb = repo.odb();
        let blob = |t: &str| {
            (
                0o100644,
                odb.write(ObjectKind::Blob, t.as_bytes()).expect("blob"),
            )
        };
        let who = Signature {
            ident: "Alice <alice@example.com>".to_owned(),
            when: 1_700_000_000,
        };
        let base_tree = edit(odb, None, &[(b"a".to_vec(), Some(blob("a")))]).expect("tree");
        let base = write_commit(&repo, base_tree, &[], &who, "base").expect("commit");
        crate::refs::write_ref(repo.git_dir(), "refs/heads/main", &base).expect("ref");
        let work_tree =
            edit(odb, Some(base_tree), &[(b"b".to_vec(), Some(blob("b")))]).expect("tree");
        let work = write_commit(&repo, work_tree, &[base], &who, "work").expect("commit");

        let merged = integrate(&repo, "refs/heads/main", base, work, "topic", &who)
            .expect("integrate")
            .expect("merged");
        let Integrated::Merged { head, commits } = merged else {
            panic!("not merged: {merged:?}");
        };
        assert_eq!(commits, 1);
        assert_eq!(
            integrate(&repo, "refs/heads/main", head, work, "topic", &who).expect("integrate"),
            Ok(Integrated::AlreadyIntegrated)
        );
        // A merge onto a stale `previous` is refused, not lost.
        let other = write_commit(&repo, base_tree, &[base], &who, "other").expect("commit");
        assert_eq!(
            integrate(&repo, "refs/heads/main", base, other, "other", &who).expect("integrate"),
            Err(TargetRefusal::Moved {
                target: "main".to_owned()
            })
        );
        let sliced = commit_slices(
            &repo,
            "refs/heads/main",
            head,
            &[Slice {
                name: "docs".to_owned(),
                description: None,
                paths: vec!["a".to_owned()],
            }],
            &who,
            "Add slice docs",
        )
        .expect("slices")
        .expect("committed");
        let log = crate::reflog::read_reflog(repo.git_dir(), "refs/heads/main").expect("reflog");
        let moves: Vec<(ObjectId, &str)> = log
            .iter()
            .map(|e| (e.new_oid, e.message.as_str()))
            .collect();
        assert!(
            moves.contains(&(head, "integrate: Merge branch 'topic' into main"))
                && moves.contains(&(sliced, "slice: Add slice docs")),
            "{moves:?}"
        );
    }
}
