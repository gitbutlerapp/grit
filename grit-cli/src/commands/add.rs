//! `grit add` — stage changes. With no paths, stages everything.
//!
//! Staging is delegated to [`grit_lib::porcelain::add::stage`] so the CLI stays
//! a thin wrapper over the library.

use crate::pathspec_args::PathspecArgs;
use anyhow::{bail, Result};
use grit_lib::apply::{apply_patch_to_index_and_write, parse_patch, IndexApplyOptions};
use grit_lib::diff::DiffSide;
use grit_lib::error::Error as LibError;
use grit_lib::porcelain::add::{stage as stage_paths, StageMode, StageOptions, StageOutcome};
use grit_lib::porcelain::hunk_select::{
    parse_hunk_id_list, stage_hunks, HunkSelectError, StageHunksOutcome,
};
use grit_lib::porcelain::index_mode::StageExecutableHint;
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use schemars::JsonSchema;
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;

use crate::context;
use crate::json_error;
use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

const HUMAN_PATH_LIMIT: usize = 10;

/// Result of `grit add`: paths staged or that would be staged.
#[derive(Serialize, JsonSchema)]
pub struct AddOutcome {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub removed: Vec<String>,
    pub intent_to_add: Vec<String>,
    pub dry_run: bool,
    /// Total paths touched (same as `staged`; kept for one release).
    pub count: usize,
    /// How many paths were staged (alias of `count` for scripts written before path lists).
    pub staged: usize,
    /// Hunk ids staged per path when `--hunks` was used.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub staged_hunks: BTreeMap<String, Vec<String>>,
    /// Whether the invocation had no path arguments (stages everything).
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) no_paths: bool,
}

impl AddOutcome {
    fn from_stage(outcome: StageOutcome, dry_run: bool, no_paths: bool) -> Self {
        let count = outcome.total();
        Self {
            added: outcome.added,
            modified: outcome.modified,
            removed: outcome.removed,
            intent_to_add: outcome.intent_to_add,
            dry_run,
            count,
            staged: count,
            staged_hunks: BTreeMap::new(),
            no_paths,
        }
    }

    fn from_hunks(outcome: StageHunksOutcome) -> Self {
        let staged_hunks: BTreeMap<String, Vec<String>> = outcome
            .staged_hunks
            .into_iter()
            .map(|(path, ids)| (path, ids.into_iter().map(|id| id.as_str()).collect()))
            .collect();
        let count = outcome.added.len() + outcome.modified.len() + outcome.removed.len();
        Self {
            added: outcome.added,
            modified: outcome.modified,
            removed: outcome.removed,
            intent_to_add: Vec::new(),
            dry_run: outcome.dry_run,
            count,
            staged: count,
            staged_hunks,
            no_paths: false,
        }
    }
}

impl HumanRender for AddOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let prefix = if self.dry_run {
            "Would stage"
        } else {
            "Staged"
        };
        let paths = self.all_paths();
        match paths.len() {
            0 if self.no_paths => outln!(w, "Nothing to stage — working tree clean."),
            0 => {}
            1 => outln!(w, "{prefix} {}", paths[0]),
            n => {
                for path in paths.iter().take(HUMAN_PATH_LIMIT) {
                    outln!(w, "{prefix} {path}");
                }
                if n > HUMAN_PATH_LIMIT {
                    outln!(w, "… and {} more", n - HUMAN_PATH_LIMIT);
                }
            }
        }
        Ok(())
    }
}

impl MarkdownRender for AddOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_add_outcome(w, self)?;
        Ok(())
    }
}

impl AddOutcome {
    fn all_paths(&self) -> Vec<&str> {
        let mut out = Vec::new();
        out.extend(self.added.iter().map(String::as_str));
        out.extend(self.modified.iter().map(String::as_str));
        out.extend(self.removed.iter().map(String::as_str));
        out.extend(self.intent_to_add.iter().map(String::as_str));
        out
    }
}

/// Exit code for `--hunks` failures (unknown, stale, or wrong-side ids).
pub fn hunks_exit_code(err: &anyhow::Error) -> i32 {
    if err.downcast_ref::<HunkSelectError>().is_some() {
        2
    } else {
        1
    }
}

pub fn run_hunks(args: &PathspecArgs, hunks: &[String], dry_run: bool) -> Result<AddOutcome> {
    let repo = context::discover()?;
    let resolved = args.resolve(&repo)?;
    let (pathspecs, _) = resolved.into_pathspec_lists();
    let selection = parse_hunk_id_list(hunks).map_err(map_hunk_error)?;
    let outcome = stage_hunks(&repo, DiffSide::Unstaged, &selection, &pathspecs, dry_run)
        .map_err(map_hunk_error)?;
    Ok(AddOutcome::from_hunks(outcome))
}

fn map_hunk_error(err: HunkSelectError) -> anyhow::Error {
    anyhow::Error::new(err)
}

pub fn run_patch_file(patch_file: &str, dry_run: bool) -> Result<AddOutcome> {
    let repo = context::discover()?;
    let label = if patch_file == "-" {
        "stdin".to_string()
    } else {
        patch_file.to_string()
    };
    let patch_bytes = read_patch_bytes(patch_file)?;
    let patch_text = std::str::from_utf8(&patch_bytes)
        .map_err(|_| anyhow::anyhow!("patch file must be valid UTF-8"))?;
    let patches =
        parse_patch(patch_text, 1, &label, false, None).map_err(json_error::map_lib_error)?;
    let outcome = apply_patch_to_index_and_write(&repo, &patches, &IndexApplyOptions { dry_run })
        .map_err(map_apply_error)?;
    let count = outcome.added.len() + outcome.modified.len() + outcome.removed.len();
    Ok(AddOutcome {
        added: outcome.added,
        modified: outcome.modified,
        removed: outcome.removed,
        intent_to_add: Vec::new(),
        dry_run: outcome.dry_run,
        count,
        staged: count,
        staged_hunks: BTreeMap::new(),
        no_paths: false,
    })
}

fn read_patch_bytes(patch_file: &str) -> Result<Vec<u8>> {
    if patch_file == "-" {
        let mut buf = Vec::new();
        std::io::stdin()
            .read_to_end(&mut buf)
            .map_err(|e| anyhow::anyhow!("could not read patch from stdin: {e}"))?;
        return Ok(buf);
    }
    std::fs::read(Path::new(patch_file))
        .map_err(|e| anyhow::anyhow!("could not read patch file '{patch_file}': {e}"))
}

pub fn run(
    args: &PathspecArgs,
    update: bool,
    dry_run: bool,
    intent_to_add: bool,
    force: bool,
    executable: bool,
    non_executable: bool,
) -> Result<AddOutcome> {
    if executable && non_executable {
        bail!("--executable and --non-executable cannot be used together");
    }
    let repo = context::discover()?;
    let resolved = args.resolve(&repo)?;
    let no_paths = resolved.is_entire_tree();
    if (executable || non_executable) && no_paths {
        bail!("--executable and --non-executable require at least one path");
    }
    if intent_to_add && no_paths {
        bail!("--intent-to-add requires at least one path");
    }
    if intent_to_add && update {
        bail!("--intent-to-add cannot be used with --update");
    }
    let outcome = stage_with_hint(
        &repo,
        resolved,
        if update {
            StageMode::Update
        } else {
            StageMode::All
        },
        dry_run,
        intent_to_add,
        force,
        if executable {
            StageExecutableHint::SetExecutable
        } else if non_executable {
            StageExecutableHint::SetNonExecutable
        } else {
            StageExecutableHint::None
        },
    )?;
    Ok(AddOutcome::from_stage(outcome, dry_run, no_paths))
}

fn stage_with_hint(
    repo: &Repository,
    resolved: grit_lib::worktree::pathspec::ResolvedPathspecs,
    mode: StageMode,
    dry_run: bool,
    intent_to_add: bool,
    force_ignored: bool,
    executable_hint: StageExecutableHint,
) -> Result<StageOutcome> {
    let (pathspecs, pathspec_sources) = resolved.into_pathspec_lists();
    let opts = StageOptions {
        pathspecs,
        pathspec_sources,
        mode,
        executable_hint,
        staging_thresholds: None,
        dry_run,
        intent_to_add,
        force_ignored,
    };
    stage_paths(repo, &opts, &mut NullProgress).map_err(map_add_error)
}

fn map_apply_error(err: grit_lib::error::ApplyError) -> anyhow::Error {
    anyhow::anyhow!("{err}")
}

fn map_add_error(err: LibError) -> anyhow::Error {
    match err {
        LibError::PathspecIgnored { paths } => {
            let mut msg =
                String::from("the following paths are ignored by one of your ignore files:\n");
            for path in paths {
                msg.push_str("    ");
                msg.push_str(&path);
                msg.push('\n');
            }
            msg.push_str("Use --force if you really want to add them.");
            anyhow::anyhow!("{msg}")
        }
        other => json_error::map_lib_error(other),
    }
}
