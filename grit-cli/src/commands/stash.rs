//! `grit stash` — save and restore shelved work.

use std::io::IsTerminal;

use anyhow::{Context, Result};
use grit_lib::error::Error;
use grit_lib::ident_resolve::IdentRole;
use grit_lib::porcelain::stash::{
    apply_stash, drop_stash, list_stashes, pop_stash, push_stash, stash_diff, StashCreateOptions,
};
use grit_lib::repo::Repository;
use serde::Serialize;

use crate::commands::diff::{self, DiffOutcome};
use crate::commands::show::{self, DiffStat};
use crate::context;
use crate::output::{HumanRender, MarkdownRender};

/// Result of `grit stash push` (or bare `grit stash`).
#[derive(Serialize)]
pub struct StashPushOutcome {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub stashed: bool,
}

impl HumanRender for StashPushOutcome {
    fn render_human(&self) {
        if !self.stashed {
            println!("No local changes to save");
            return;
        }
        let oid = self.oid.as_deref().unwrap_or("");
        let short = oid.get(..7).unwrap_or(oid);
        let msg = self.message.as_deref().unwrap_or("");
        if msg.is_empty() {
            println!("Saved working directory and index state {short}");
        } else {
            println!("Saved working directory and index state On {msg} {short}");
        }
    }
}

impl MarkdownRender for StashPushOutcome {}

/// Result of `grit stash list`.
#[derive(Serialize)]
pub struct StashListOutcome {
    pub entries: Vec<StashListEntry>,
}

#[derive(Serialize)]
pub struct StashListEntry {
    pub index: usize,
    pub oid: String,
    pub message: String,
}

impl HumanRender for StashListOutcome {
    fn render_human(&self) {
        for entry in &self.entries {
            let short = entry.oid.get(..7).unwrap_or(&entry.oid);
            println!("stash@{{{}}} {}: {}", entry.index, short, entry.message);
        }
    }
}

impl MarkdownRender for StashListOutcome {
    fn render_markdown(&self) {
        if self.entries.is_empty() {
            println!("- _(no stash entries)_");
            return;
        }
        for entry in &self.entries {
            let short = entry.oid.get(..7).unwrap_or(&entry.oid);
            println!(
                "- **stash@{{{}}}** `{short}` — {}",
                entry.index, entry.message
            );
        }
    }
}

/// Result of `grit stash show`.
#[derive(Serialize)]
pub struct StashShowOutcome {
    pub index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stat: Option<DiffStat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch: Option<DiffOutcome>,
}

impl HumanRender for StashShowOutcome {
    fn render_human(&self) {
        if let Some(patch) = &self.patch {
            patch.render_human();
            return;
        }
        if let Some(stat) = &self.stat {
            let color = std::io::stdout().is_terminal()
                && grit_lib::terminal::ansi_supported()
                && std::env::var_os("NO_COLOR").is_none();
            show::render_stat(stat, color);
        }
    }
}

impl MarkdownRender for StashShowOutcome {
    fn render_markdown(&self) {
        if let Some(patch) = &self.patch {
            patch.render_markdown();
            return;
        }
        if let Some(stat) = &self.stat {
            show::render_markdown_stat(stat);
        }
    }
}

/// Result of `grit stash apply`.
#[derive(Serialize)]
pub struct StashApplyOutcome {
    pub index: usize,
    pub conflicts: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub conflict_paths: Vec<String>,
}

impl HumanRender for StashApplyOutcome {
    fn render_human(&self) {
        if self.conflicts {
            eprintln!("Stash apply left conflicts in:");
            for path in &self.conflict_paths {
                eprintln!("  {path}");
            }
        } else {
            println!("Applied stash@{{{}}}", self.index);
        }
    }
}

impl MarkdownRender for StashApplyOutcome {}

/// Result of `grit stash pop`.
#[derive(Serialize)]
pub struct StashPopOutcome {
    pub index: usize,
    pub dropped: bool,
    pub conflicts: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub conflict_paths: Vec<String>,
}

impl HumanRender for StashPopOutcome {
    fn render_human(&self) {
        if self.conflicts {
            eprintln!("Stash pop left conflicts in:");
            for path in &self.conflict_paths {
                eprintln!("  {path}");
            }
            eprintln!("The stash entry was kept.");
        } else {
            println!(
                "Dropped stash@{{{}}} ({})",
                self.index,
                if self.dropped { "applied" } else { "empty" }
            );
        }
    }
}

impl MarkdownRender for StashPopOutcome {}

/// Result of `grit stash drop`.
#[derive(Serialize)]
pub struct StashDropOutcome {
    pub index: usize,
}

impl HumanRender for StashDropOutcome {
    fn render_human(&self) {
        println!("Dropped stash@{{{}}}", self.index);
    }
}

impl MarkdownRender for StashDropOutcome {}

pub fn run_push(message: Option<String>, include_untracked: bool) -> Result<StashPushOutcome> {
    let repo = context::discover()?;
    let env = context::environment();
    let config = repo.config().context("could not load config")?;
    let now = context::wall_clock_now(&env);
    let identity = context::identity(
        &env,
        &config,
        IdentRole::Committer,
        "GIT_COMMITTER_DATE",
        now,
    )?;
    let options = StashCreateOptions {
        message,
        include_untracked,
        identity,
    };
    let pushed = push_stash(&repo, &options).map_err(map_stash_error)?;
    Ok(match pushed {
        Some(oid) => {
            let entries = list_stashes(&repo).map_err(map_stash_error)?;
            let message = entries
                .iter()
                .find(|e| e.oid == oid)
                .map(|e| e.message.clone());
            StashPushOutcome {
                oid: Some(oid.to_hex()),
                message,
                stashed: true,
            }
        }
        None => StashPushOutcome {
            oid: None,
            message: None,
            stashed: false,
        },
    })
}

pub fn run_list() -> Result<StashListOutcome> {
    let repo = context::discover()?;
    let entries = list_stashes(&repo).map_err(map_stash_error)?;
    Ok(StashListOutcome {
        entries: entries
            .into_iter()
            .map(|e| StashListEntry {
                index: e.index,
                oid: e.oid.to_hex(),
                message: e.message,
            })
            .collect(),
    })
}

pub fn run_show(index: Option<String>, patch: bool) -> Result<StashShowOutcome> {
    let repo = context::discover()?;
    let n = parse_stash_index(index.as_deref())?;
    if patch {
        let entries = stash_diff(&repo, n).map_err(map_stash_error)?;
        let patch = diff::outcome_from_tree_entries(&repo, entries)?;
        return Ok(StashShowOutcome {
            index: n,
            stat: None,
            patch: Some(patch),
        });
    }
    let entries = stash_diff(&repo, n).map_err(map_stash_error)?;
    let full = diff::outcome_from_tree_entries(&repo, entries)?;
    let stat = show::diffstat(&full);
    Ok(StashShowOutcome {
        index: n,
        stat: Some(stat),
        patch: None,
    })
}

pub fn run_apply(index: Option<String>) -> Result<StashApplyOutcome> {
    let repo = context::discover()?;
    let work_tree = repo
        .work_tree
        .as_deref()
        .context("grit stash apply needs a working tree")?;
    let n = parse_stash_index(index.as_deref())?;
    let env = context::environment();
    let config = repo.config().context("could not load config")?;
    let now = context::wall_clock_now(&env);
    let identity = context::reflog_identity(&env, &config, now);
    let conflicts = apply_stash(&repo, work_tree, &stash_oid_at(&repo, n)?, false, &identity)
        .map_err(map_stash_error)?;
    let conflict_paths = if conflicts {
        unmerged_paths(&repo)?
    } else {
        Vec::new()
    };
    Ok(StashApplyOutcome {
        index: n,
        conflicts,
        conflict_paths,
    })
}

pub fn run_pop(index: Option<String>) -> Result<StashPopOutcome> {
    let repo = context::discover()?;
    let work_tree = repo
        .work_tree
        .as_deref()
        .context("grit stash pop needs a working tree")?;
    let n = parse_stash_index(index.as_deref())?;
    let env = context::environment();
    let config = repo.config().context("could not load config")?;
    let now = context::wall_clock_now(&env);
    let identity = context::reflog_identity(&env, &config, now);
    let conflicts = pop_stash(&repo, work_tree, n, false, &identity).map_err(map_stash_error)?;
    let conflict_paths = if conflicts {
        unmerged_paths(&repo)?
    } else {
        Vec::new()
    };
    Ok(StashPopOutcome {
        index: n,
        dropped: !conflicts,
        conflicts,
        conflict_paths,
    })
}

pub fn run_drop(index: Option<String>) -> Result<StashDropOutcome> {
    let repo = context::discover()?;
    let n = parse_stash_index(index.as_deref())?;
    let env = context::environment();
    let config = repo.config().context("could not load config")?;
    let now = context::wall_clock_now(&env);
    let identity = context::reflog_identity(&env, &config, now);
    drop_stash(&repo, n, &identity).map_err(map_stash_error)?;
    Ok(StashDropOutcome { index: n })
}

fn stash_oid_at(repo: &Repository, n: usize) -> Result<grit_lib::objects::ObjectId> {
    list_stashes(repo)
        .map_err(map_stash_error)?
        .into_iter()
        .find(|e| e.index == n)
        .map(|e| e.oid)
        .ok_or_else(|| Error::StashNotFound { n })
        .map_err(map_stash_error)
}

fn unmerged_paths(repo: &Repository) -> Result<Vec<String>> {
    let index = repo.load_index().context("could not load index")?;
    let mut paths: Vec<String> = index
        .entries()
        .iter()
        .filter(|e| e.stage() > 0)
        .map(|e| String::from_utf8_lossy(&e.path).into_owned())
        .collect();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Parse `n` or `stash@{n}` (default `0`).
pub fn parse_stash_index(spec: Option<&str>) -> Result<usize> {
    let raw = spec.unwrap_or("0").trim();
    let digits = if let Some(inner) = raw
        .strip_prefix("stash@{")
        .and_then(|s| s.strip_suffix('}'))
    {
        inner
    } else {
        raw
    };
    digits
        .parse::<usize>()
        .with_context(|| format!("invalid stash index '{raw}'"))
}

fn map_stash_error(err: Error) -> anyhow::Error {
    match err {
        Error::StashNoInitialCommit => {
            anyhow::anyhow!("you do not have the initial commit yet")
        }
        Error::StashNotFound { n } => anyhow::anyhow!("stash@{{{n}}} is not a valid stash ref"),
        Error::CorruptStash(msg) => anyhow::anyhow!("corrupt stash commit: {msg}"),
        Error::StashWouldOverwriteLocalChanges { paths } => {
            anyhow::anyhow!("local changes would be overwritten by stash:\n{paths}")
        }
        Error::PathError(msg) if msg.contains("bare") => anyhow::anyhow!("{msg}"),
        other => anyhow::Error::new(other),
    }
}
