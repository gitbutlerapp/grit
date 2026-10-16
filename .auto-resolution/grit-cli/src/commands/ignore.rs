//! `grit ignore check` — query whether paths match exclude rules.

use std::io::Write;
use std::path::Path;

use anyhow::{bail, Context, Result};
use grit_lib::repo::Repository;
use grit_lib::worktree::ignore::{normalize_repo_relative, IgnoreMatch, IgnoreSource};
use schemars::JsonSchema;
use serde::Serialize;

use crate::context;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

/// Result of `grit ignore check`.
#[derive(Serialize, JsonSchema)]
pub struct IgnoreCheckOutcome {
    /// Whether at least one queried path is ignored.
    pub any_ignored: bool,
    /// Per-path results (filtered by `--all`; default lists only ignored paths).
    pub entries: Vec<IgnoreCheckEntryJson>,
}

#[derive(Serialize, JsonSchema)]
pub struct IgnoreCheckEntryJson {
    pub path: String,
    pub ignored: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched: Option<IgnoreMatchJson>,
}

#[derive(Serialize, JsonSchema)]
pub struct IgnoreMatchJson {
    pub source: IgnoreSourceJson,
    pub line_number: usize,
    pub pattern_text: String,
    pub negative: bool,
}

#[derive(Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IgnoreSourceJson {
    PerDirectory { path: String },
    InfoExclude,
    CoreExcludesFile { path: String },
}

impl From<&IgnoreSource> for IgnoreSourceJson {
    fn from(value: &IgnoreSource) -> Self {
        match value {
            IgnoreSource::PerDirectory(path) => Self::PerDirectory { path: path.clone() },
            IgnoreSource::InfoExclude => Self::InfoExclude,
            IgnoreSource::CoreExcludesFile(path) => Self::CoreExcludesFile { path: path.clone() },
        }
    }
}

fn match_to_json(m: &IgnoreMatch) -> IgnoreMatchJson {
    IgnoreMatchJson {
        source: (&m.source).into(),
        line_number: m.line_number,
        pattern_text: m.pattern_text.clone(),
        negative: m.negative,
    }
}

impl HumanRender for IgnoreCheckOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        for entry in &self.entries {
            if entry.ignored {
                if let Some(m) = &entry.matched {
                    let source = source_display(&m.source);
                    outln!(
                        w,
                        "{}  ← {}:{} {}",
                        entry.path,
                        source,
                        m.line_number,
                        m.pattern_text
                    );
                } else {
                    outln!(w, "{}", entry.path);
                }
            } else {
                outln!(w, "{}", entry.path);
            }
        }
        Ok(())
    }
}

impl MarkdownRender for IgnoreCheckOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "## Ignore check\n");
        outln!(w, "- **any_ignored:** {}\n", self.any_ignored);
        if self.entries.is_empty() {
            outln!(w, "No matching paths to show.");
            return Ok(());
        }
        for entry in &self.entries {
            if entry.ignored {
                if let Some(m) = &entry.matched {
                    let source = source_display(&m.source);
                    outln!(
                        w,
                        "- `{}` ← `{}:{}` `{}`",
                        entry.path,
                        source,
                        m.line_number,
                        m.pattern_text
                    );
                } else {
                    outln!(w, "- `{}` (ignored)", entry.path);
                }
            } else {
                outln!(w, "- `{}` (not ignored)", entry.path);
            }
        }
        Ok(())
    }
}

fn source_display(source: &IgnoreSourceJson) -> String {
    match source {
        IgnoreSourceJson::PerDirectory { path } => path.clone(),
        IgnoreSourceJson::InfoExclude => ".git/info/exclude".to_owned(),
        IgnoreSourceJson::CoreExcludesFile { path } => path.clone(),
    }
}

pub fn run(paths: &[String], all: bool) -> Result<IgnoreCheckOutcome> {
    if paths.is_empty() {
        bail!("at least one path is required");
    }
    let repo = context::discover()?;
    let cwd = std::env::current_dir().context("could not read the current directory")?;
    let index = repo
        .index_handle()
        .load()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let repo_paths = normalize_paths(&repo, &cwd, paths)?;
    let checks = repo
        .ignore_check(&repo_paths, &index)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let any_ignored = checks.iter().any(|c| c.ignored);
    let entries: Vec<IgnoreCheckEntryJson> = checks
        .iter()
        .filter(|c| all || c.ignored)
        .map(|c| IgnoreCheckEntryJson {
            path: c.path.clone(),
            ignored: c.ignored,
            matched: c.matched.as_ref().map(match_to_json),
        })
        .collect();
    Ok(IgnoreCheckOutcome {
        any_ignored,
        entries,
    })
}

fn normalize_paths(repo: &Repository, cwd: &Path, paths: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        let rel = normalize_repo_relative(repo, cwd, path).map_err(|e| anyhow::anyhow!("{e}"))?;
        out.push(rel);
    }
    Ok(out)
}

/// Exit **0** when at least one path is ignored, **1** when none are.
pub fn exit_code(outcome: &IgnoreCheckOutcome) -> i32 {
    if outcome.any_ignored {
        0
    } else {
        1
    }
}
