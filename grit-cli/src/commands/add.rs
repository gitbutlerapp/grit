//! `grit add` — stage changes. With no paths, stages everything.
//!
//! Staging is driven by the same status model the dashboard uses, so `grit add`
//! stages exactly what `grit status` reports as changed — including deletions and
//! untracked files — without reimplementing worktree walking or ignore rules.

use anyhow::{Context, Result};
use grit_lib::diff::{mode_from_metadata, DiffStatus};
use grit_lib::index::{entry_from_stat, Index};
use grit_lib::objects::ObjectKind;
use grit_lib::porcelain::status::{status, StatusOptions, UntrackedMode};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
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
            0 => println!("Nothing to stage matched the given paths."),
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
    // Enumerate untracked *files* (not collapsed directories like `sub/`), so we
    // can hash each one rather than trying to read a directory as a blob.
    let opts = StatusOptions {
        untracked: UntrackedMode::All,
        ..StatusOptions::default()
    };
    let model = status(repo, &opts, &mut NullProgress).context("could not compute status")?;
    let mut index = repo.load_index().context("could not load the index")?;

    let matches =
        |path: &str| selectors.is_empty() || selectors.iter().any(|s| path_matches(s, path));

    let mut staged = 0;
    for entry in &model.unstaged {
        let path = entry_path(entry);
        if !matches(&path) {
            continue;
        }
        if entry.status == DiffStatus::Deleted {
            if index.remove(path.as_bytes()) {
                staged += 1;
            }
        } else {
            stage_worktree_file(repo, &work_tree, &path, &mut index)?;
            staged += 1;
        }
    }
    for path in &model.untracked {
        if !matches(path) {
            continue;
        }
        stage_worktree_file(repo, &work_tree, path, &mut index)?;
        staged += 1;
    }

    if staged > 0 {
        index.sort();
        repo.write_index(&mut index)
            .context("could not write the index")?;
    }
    Ok(staged)
}

/// Hash a working-tree file into a blob and (re)stage it in the index.
fn stage_worktree_file(
    repo: &Repository,
    work_tree: &std::path::Path,
    rel_path: &str,
    index: &mut Index,
) -> Result<()> {
    let abs = work_tree.join(rel_path);
    let meta =
        std::fs::symlink_metadata(&abs).with_context(|| format!("could not read {rel_path}"))?;
    let mode = mode_from_metadata(&meta);

    let data = if meta.file_type().is_symlink() {
        let target = std::fs::read_link(&abs)
            .with_context(|| format!("could not read symlink {rel_path}"))?;
        target.to_string_lossy().into_owned().into_bytes()
    } else {
        std::fs::read(&abs).with_context(|| format!("could not read {rel_path}"))?
    };

    let oid = repo
        .odb
        .write(ObjectKind::Blob, &data)
        .with_context(|| format!("could not store {rel_path}"))?;
    let entry = entry_from_stat(&abs, rel_path.as_bytes(), oid, mode)
        .with_context(|| format!("could not stage {rel_path}"))?;
    index.add_or_replace(entry);
    Ok(())
}

/// Whether a worktree `path` is covered by a user-provided `selector`.
fn path_matches(selector: &str, path: &str) -> bool {
    let selector = selector.trim_start_matches("./").trim_end_matches('/');
    if selector.is_empty() || selector == "." {
        return true;
    }
    path == selector || path.starts_with(&format!("{selector}/"))
}
