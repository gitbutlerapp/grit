//! `grit add` — stage changes. With no paths, stages everything.
//!
//! Staging uses the library [`stage_worktree_changes`] engine (index/worktree diff),
//! not a full `status` pass, so large trees with few edits stay fast.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{bail, Context, Result};
use grit_lib::index::MODE_TREE;
use grit_lib::objects::{parse_commit, parse_tree, ObjectId};
use grit_lib::pathspec::{
    has_glob_chars, matches_pathspec_list, pathdiff, pathspec_is_exclude,
    resolve_pathspec_in_worktree,
};
use grit_lib::porcelain::staging::stage_worktree_changes;
use grit_lib::porcelain::status::{status, StatusOptions, UntrackedMode};
use grit_lib::precompose_config::effective_core_precomposeunicode;
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_lib::state::resolve_head;
use grit_lib::unicode_normalization::resolve_worktree_path_for_staging;
use serde::Serialize;

use crate::context;
use crate::output::HumanRender;
use crate::ui::entry_path;

/// Result of `grit add`: how many changes were staged.
#[derive(Serialize)]
pub struct AddOutcome {
    pub staged: usize,
    /// Whether the invocation had no path arguments (stages everything).
    #[serde(skip)]
    no_paths: bool,
}

impl HumanRender for AddOutcome {
    fn render_human(&self) {
        match self.staged {
            0 if self.no_paths => println!("Nothing to stage — working tree clean."),
            0 => {}
            1 => println!("Staged 1 change."),
            n => println!("Staged {n} changes."),
        }
    }
}

pub fn run(paths: &[String]) -> Result<AddOutcome> {
    let repo = context::discover()?;
    let staged = stage(&repo, paths)?;
    Ok(AddOutcome {
        staged,
        no_paths: paths.is_empty(),
    })
}

/// Stage all changes matching `selectors` (empty selectors = everything).
///
/// Returns the number of paths staged. Shared with `grit commit -a`.
pub fn stage(repo: &Repository, selectors: &[String]) -> Result<usize> {
    let work_tree = repo
        .work_tree
        .clone()
        .context("grit add needs a working tree")?;
    let cwd = std::env::current_dir().context("could not read the current directory")?;
    let prefix = pathdiff(&cwd, &work_tree);

    let resolved_specs: Option<Vec<String>> = if selectors.is_empty() {
        None
    } else {
        let mut specs = Vec::with_capacity(selectors.len());
        for sel in selectors {
            if sel.is_empty() {
                bail!("invalid path ''");
            }
            let resolved = resolve_pathspec_in_worktree(sel, sel, &work_tree, prefix.as_deref())
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            specs.push(resolved);
        }
        Some(specs)
    };

    if let Some(specs) = &resolved_specs {
        let positive = selectors
            .iter()
            .zip(specs.iter())
            .filter(|(s, _)| !pathspec_is_exclude(s))
            .collect::<Vec<_>>();
        if !positive.is_empty() {
            let known = known_paths(repo)?;
            for (orig, resolved) in positive {
                if !selector_matches_known(resolved, &known, &work_tree)? {
                    bail!("pathspec '{orig}' did not match any files");
                }
            }
        }
        return stage_worktree_changes(repo, specs).context("could not stage changes");
    }

    stage_worktree_changes(repo, &[]).context("could not stage changes")
}

/// Paths that may satisfy an explicit pathspec (index, HEAD tree, and a status snapshot).
fn known_paths(repo: &Repository) -> Result<Vec<String>> {
    let index = repo.load_index().context("could not load the index")?;
    let opts = StatusOptions {
        untracked: UntrackedMode::All,
        ..StatusOptions::default()
    };
    let model = status(repo, &opts, &mut NullProgress).context("could not compute status")?;
    let mut set = HashSet::<String>::new();
    for entry in &index.entries {
        if entry.stage() == 0 {
            set.insert(String::from_utf8_lossy(&entry.path).into_owned());
        }
    }
    for entry in &model.staged {
        set.insert(entry_path(entry).to_owned());
    }
    for entry in &model.unstaged {
        set.insert(entry_path(entry).to_owned());
    }
    for path in &model.untracked {
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

/// Whether `resolved` matches at least one known path or an explicitly named worktree path.
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
