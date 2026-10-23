//! `grit restore` — restore working tree and/or index paths.

use anyhow::{Context, Result};
use grit_lib::porcelain::hunk_select::{
    parse_hunk_id_list, restore_hunks, HunkSelectError, RestoreHunksOutcome,
};
use grit_lib::porcelain::restore::{
    restore_paths, ConflictSide, RestoreOptions, RestoreOutcome, RestoreSource,
};

use crate::conflict_style::{self, ConflictStyleArg};
use grit_lib::revwalk::rev_parse::resolve_revision;
use schemars::JsonSchema;
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::Write;

use crate::context;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use crate::pathspec_args::PathspecArgs;

/// CLI result (mirrors [`RestoreOutcome`] or [`RestoreHunksOutcome`]).
#[derive(Serialize, JsonSchema, Clone)]
pub struct RestoreCliOutcome {
    pub restored: Vec<String>,
    pub removed: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub restored_hunks: BTreeMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub restored_lines: BTreeMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dry_run: bool,
}

impl From<RestoreOutcome> for RestoreCliOutcome {
    fn from(value: RestoreOutcome) -> Self {
        Self {
            restored: value.restored,
            removed: value.removed,
            restored_hunks: BTreeMap::new(),
            restored_lines: BTreeMap::new(),
            dry_run: false,
        }
    }
}

impl From<RestoreHunksOutcome> for RestoreCliOutcome {
    fn from(value: RestoreHunksOutcome) -> Self {
        let restored_hunks: BTreeMap<String, Vec<String>> = value
            .restored_hunks
            .into_iter()
            .map(|(path, ids)| (path, ids.into_iter().map(|id| id.as_str()).collect()))
            .collect();
        Self {
            restored: value.restored,
            removed: value.removed,
            restored_hunks,
            restored_lines: value.restored_lines,
            dry_run: value.dry_run,
        }
    }
}

impl HumanRender for RestoreCliOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let prefix = if self.dry_run {
            "Would restore"
        } else {
            "Restored"
        };
        for path in &self.restored {
            outln!(w, "{prefix} {path}");
        }
        for path in &self.removed {
            outln!(w, "Removed {path}");
        }
        Ok(())
    }
}

impl MarkdownRender for RestoreCliOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "## Restore");
        if self.restored.is_empty() && self.removed.is_empty() {
            outln!(w, "\nNo paths changed.");
            return Ok(());
        }
        if !self.restored.is_empty() {
            outln!(w, "\n### Restored");
            for path in &self.restored {
                outln!(w, "- `{path}`");
            }
        }
        if !self.removed.is_empty() {
            outln!(w, "\n### Removed");
            for path in &self.removed {
                outln!(w, "- `{path}`");
            }
        }
        Ok(())
    }
}

/// Exit code for `--hunks` / `--lines` failures.
pub fn hunks_exit_code(err: &anyhow::Error) -> i32 {
    if err.downcast_ref::<HunkSelectError>().is_some() {
        2
    } else {
        1
    }
}

fn map_hunk_error(err: HunkSelectError) -> anyhow::Error {
    anyhow::Error::new(err)
}

/// Run partial restore via hunk ids and/or line ranges.
pub fn run_hunks(
    args: &PathspecArgs,
    hunks: &[String],
    lines: &[String],
    staged: bool,
    worktree: bool,
    dry_run: bool,
) -> Result<RestoreCliOutcome> {
    let repo = context::discover()?;
    let resolved = args.resolve(&repo)?;
    let (pathspecs, _) = resolved.into_pathspec_lists();
    let mut selection = parse_hunk_id_list(hunks).map_err(map_hunk_error)?;
    selection.ranges = super::add::parse_line_range_list(lines)?;
    let outcome = restore_hunks(&repo, &selection, &pathspecs, staged, worktree, dry_run)
        .map_err(map_hunk_error)?;
    Ok(outcome.into())
}

/// Run `grit restore` with the given flags and path arguments.
pub fn run(
    args: &PathspecArgs,
    staged: bool,
    worktree: bool,
    source: Option<String>,
    conflict_side: Option<ConflictSide>,
    merge: bool,
    conflict_style: Option<ConflictStyleArg>,
) -> Result<RestoreCliOutcome> {
    args.require_non_empty()?;
    let repo = context::discover()?;
    let remerge = if merge || conflict_style.is_some() {
        Some(
            conflict_style
                .map(Into::into)
                .unwrap_or_else(|| conflict_style::conflict_style_from_repo(&repo)),
        )
    } else {
        None
    };
    let resolved = args.resolve(&repo)?;

    let restore_source = match source {
        None => RestoreSource::Index,
        Some(rev) => {
            let oid = resolve_revision(&repo, &rev)
                .with_context(|| format!("could not resolve '{rev}'"))?;
            let obj = repo.odb().read(&oid).context("reading source object")?;
            use grit_lib::objects::{parse_commit, ObjectKind};
            let tree = match obj.kind {
                ObjectKind::Commit => parse_commit(&obj.data)?.tree,
                ObjectKind::Tree => oid,
                ObjectKind::Blob | ObjectKind::Tag => {
                    anyhow::bail!("source must be a commit or tree");
                }
            };
            RestoreSource::Tree(tree)
        }
    };

    let grit_lib::worktree::pathspec::ResolvedPathspecs {
        specs: pathspecs,
        sources: pathspec_sources,
        scoped: _,
    } = resolved;

    let opts = RestoreOptions {
        pathspecs,
        pathspec_sources,
        source: restore_source,
        staged,
        worktree,
        stage: conflict_side,
        remerge,
    };

    let outcome = restore_paths(&repo, &opts).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(outcome.into())
}
