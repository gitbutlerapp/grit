//! Outcome types and renderers for `grit remote`.

use std::io::Write;

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use crate::render::table::escape_table_cell;

/// Result of `grit remote`, tagged by `action` (`list` / `add` / `refs`).
#[derive(Serialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum RemoteOutcome {
    List {
        remotes: Vec<RemoteEntry>,
        #[serde(skip)]
        #[schemars(skip)]
        verbose_lines: Vec<RemoteVerboseLine>,
    },
    Add {
        name: String,
        url: String,
        fetched: bool,
    },
    SetUrl {
        name: String,
        kind: String,
        old_url: Option<String>,
        url: String,
        dry_run: bool,
    },
    Remove {
        name: String,
        refs_deleted: Vec<String>,
        branches_untracked: Vec<String>,
        dry_run: bool,
    },
    Rename {
        old: String,
        new: String,
        refs_renamed: usize,
        refspecs_updated: Vec<String>,
        branches_updated: Vec<String>,
        dry_run: bool,
    },
    Refs {
        refs: Vec<RemoteRefEntry>,
        #[serde(skip)]
        #[schemars(skip)]
        lines: Vec<RemoteRefLine>,
    },
    Show {
        #[serde(flatten)]
        summary: RemoteShowJson,
    },
    Prune {
        name: String,
        refs_pruned: Vec<String>,
        dry_run: bool,
    },
    SetHead {
        name: String,
        target: Option<String>,
        previous_target: Option<String>,
        dry_run: bool,
    },
}

/// JSON view of a remote configuration summary for `grit remote show --json`.
#[derive(Serialize, JsonSchema, Clone)]
pub struct RemoteShowJson {
    pub name: String,
    pub fetch_url: String,
    pub push_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub head_branch: Option<String>,
    pub branches: Vec<RemoteShowBranchJson>,
    pub pull: Vec<RemoteShowPullJson>,
}

/// One branch row in a `show` JSON outcome.
#[derive(Serialize, JsonSchema, Clone)]
pub struct RemoteShowBranchJson {
    pub name: String,
    pub state: RemoteShowBranchStateJson,
}

/// One pull-config row in a `show` JSON outcome.
#[derive(Serialize, JsonSchema, Clone)]
pub struct RemoteShowPullJson {
    pub local: String,
    pub merges: String,
}

/// One remote in a `list` outcome.
#[derive(Serialize, JsonSchema)]
pub struct RemoteEntry {
    pub name: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub fetch_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub push_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub refspecs: Option<Vec<String>>,
}

/// One line of `grit remote -v` output.
#[derive(Clone)]
pub struct RemoteVerboseLine {
    pub name: String,
    pub url: String,
    pub kind: RemoteVerboseKind,
}

/// One ref in a `refs` JSON outcome (annotated tags carry `peeled`; no separate `^{}` row).
#[derive(Serialize, JsonSchema, Clone)]
pub struct RemoteRefEntry {
    pub name: String,
    pub oid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub peeled: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub symref_target: Option<String>,
}

/// One line of human/`git ls-remote`-style output (`oid` then `name`).
#[derive(Clone)]
pub struct RemoteRefLine {
    pub oid: String,
    pub name: String,
    pub symref: bool,
}

/// Branch tracking state in JSON (`tracked` / `new` / `stale`).
#[derive(Serialize, JsonSchema, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum RemoteShowBranchStateJson {
    Tracked,
    New,
    Stale,
}

#[derive(Clone, Copy)]
pub enum RemoteVerboseKind {
    Fetch,
    Push,
}

impl HumanRender for RemoteOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self {
            RemoteOutcome::List {
                remotes,
                verbose_lines,
            } => {
                if remotes.is_empty() {
                    outln!(w, "No remotes. Add one with: grit remote add <name> <url>");
                    return Ok(());
                }
                if !verbose_lines.is_empty() {
                    for line in verbose_lines {
                        let label = match line.kind {
                            RemoteVerboseKind::Fetch => "fetch",
                            RemoteVerboseKind::Push => "push",
                        };
                        outln!(w, "{}\t{} ({label})", line.name, line.url);
                    }
                } else {
                    for remote in remotes {
                        outln!(w, "{}\t{}", remote.name, remote.url);
                    }
                }
            }
            RemoteOutcome::Add { name, url, fetched } => {
                outln!(w, "Added remote {name} → {url}");
                if *fetched {
                    outln!(w, "Fetched from {name}");
                }
            }
            RemoteOutcome::SetUrl {
                name,
                kind,
                old_url,
                url,
                dry_run,
            } => {
                let prefix = if *dry_run { "[dry-run] " } else { "" };
                if let Some(old) = old_url {
                    outln!(w, "{prefix}Updated remote {name} {kind} URL: {old} → {url}");
                } else {
                    outln!(w, "{prefix}Set remote {name} {kind} URL to {url}");
                }
            }
            RemoteOutcome::Remove {
                name,
                refs_deleted,
                branches_untracked,
                dry_run,
            } => {
                if *dry_run {
                    outln!(w, "Would remove remote {name}");
                } else {
                    outln!(w, "Removed remote {name}");
                }
                if !refs_deleted.is_empty() {
                    outln!(w, "  refs deleted: {}", refs_deleted.len());
                }
                if !branches_untracked.is_empty() {
                    outln!(w, "  branches untracked: {}", branches_untracked.join(", "));
                }
            }
            RemoteOutcome::Rename {
                old,
                new,
                refs_renamed,
                dry_run,
                ..
            } => {
                let prefix = if *dry_run { "[dry-run] " } else { "" };
                outln!(
                    w,
                    "{prefix}Renamed remote {old} → {new} ({refs_renamed} tracking refs updated)"
                );
            }
            RemoteOutcome::Refs { lines, .. } => {
                for line in lines {
                    if line.symref {
                        outln!(w, "ref: {}\t{}", line.oid, line.name);
                    } else {
                        outln!(w, "{}\t{}", line.oid, line.name);
                    }
                }
            }
            RemoteOutcome::Show { summary } => render_show_human(w, summary)?,
            RemoteOutcome::Prune {
                name,
                refs_pruned,
                dry_run,
            } => {
                let prefix = if *dry_run { "[dry-run] " } else { "" };
                if refs_pruned.is_empty() {
                    outln!(w, "{prefix}Nothing to prune for {name}");
                } else {
                    for r in refs_pruned {
                        outln!(w, "{prefix}Pruned {r}");
                    }
                }
            }
            RemoteOutcome::SetHead {
                name,
                target,
                previous_target: _,
                dry_run,
            } => {
                let prefix = if *dry_run { "[dry-run] " } else { "" };
                match target {
                    Some(t) => outln!(w, "{prefix}Set remote {name} HEAD → {t}"),
                    None => outln!(w, "{prefix}Deleted remote {name} HEAD symref"),
                }
            }
        }
        Ok(())
    }
}

impl MarkdownRender for RemoteOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self {
            RemoteOutcome::List {
                remotes,
                verbose_lines: _,
            } => print_remote_list(w, remotes)?,
            RemoteOutcome::Add { name, url, fetched } => {
                outln!(w, "Added remote `{name}` → {url}.");
                if *fetched {
                    outln!(w, "\nFetched from `{name}`.\n");
                }
            }
            RemoteOutcome::SetUrl {
                name,
                kind,
                old_url,
                url,
                dry_run,
            } => {
                let dry = if *dry_run { " (dry-run)" } else { "" };
                if let Some(old) = old_url {
                    outln!(
                        w,
                        "Updated remote `{name}` {kind} URL{dry}: `{old}` → {url}."
                    );
                } else {
                    outln!(w, "Set remote `{name}` {kind} URL{dry} to {url}.");
                }
            }
            RemoteOutcome::Remove {
                name,
                refs_deleted,
                branches_untracked,
                dry_run,
            } => {
                if *dry_run {
                    outln!(w, "Would remove remote `{name}`.");
                } else {
                    outln!(w, "Removed remote `{name}`.");
                }
                if !refs_deleted.is_empty() {
                    outln!(w, "\n**Refs deleted:** {}\n", refs_deleted.len());
                    for r in refs_deleted {
                        outln!(w, "- `{r}`");
                    }
                }
                if !branches_untracked.is_empty() {
                    outln!(
                        w,
                        "\n**Branches untracked:** {}.\n",
                        branches_untracked.join(", ")
                    );
                }
            }
            RemoteOutcome::Rename {
                old,
                new,
                refs_renamed,
                refspecs_updated,
                branches_updated,
                dry_run,
            } => {
                let dry = if *dry_run { " (dry-run)" } else { "" };
                outln!(
                    w,
                    "Renamed remote `{old}` → `{new}`{dry} ({refs_renamed} tracking refs)."
                );
                if !refspecs_updated.is_empty() {
                    outln!(w, "\nUpdated fetch refspecs:");
                    for spec in refspecs_updated {
                        outln!(w, "- `{spec}`");
                    }
                }
                if !branches_updated.is_empty() {
                    outln!(
                        w,
                        "\nUpdated branch upstream: `{}`.",
                        branches_updated.join("`, `")
                    );
                }
            }
            RemoteOutcome::Show { summary } => render_show_markdown(w, summary)?,
            RemoteOutcome::Prune {
                name,
                refs_pruned,
                dry_run,
            } => {
                let dry = if *dry_run { " (dry-run)" } else { "" };
                outln!(w, "## Prune `{name}`{dry}\n");
                if refs_pruned.is_empty() {
                    outln!(w, "Nothing to prune.\n");
                } else {
                    for r in refs_pruned {
                        outln!(w, "- `{r}`");
                    }
                    outln!(w);
                }
            }
            RemoteOutcome::SetHead {
                name,
                target,
                previous_target,
                dry_run,
            } => {
                let dry = if *dry_run { " (dry-run)" } else { "" };
                outln!(w, "## Set HEAD for `{name}`{dry}\n");
                if let Some(prev) = previous_target {
                    outln!(w, "Previous: `{prev}`\n");
                }
                match target {
                    Some(t) => outln!(w, "Target: `{t}`.\n"),
                    None => outln!(w, "Deleted symref.\n"),
                }
            }
            RemoteOutcome::Refs { refs, .. } => {
                outln!(w, "## Remote refs\n");
                if refs.is_empty() {
                    outln!(w, "No refs matched.\n");
                    return Ok(());
                }
                outln!(w, "| Ref | Object | Peeled | Symref target |");
                outln!(w, "| --- | --- | --- | --- |");
                for entry in refs {
                    let peeled = entry.peeled.as_deref().unwrap_or("—");
                    let sym = entry.symref_target.as_deref().unwrap_or("—");
                    let peeled_cell = if peeled == "—" {
                        "—".to_owned()
                    } else {
                        format!("`{peeled}`")
                    };
                    let sym_cell = if sym == "—" {
                        "—".to_owned()
                    } else {
                        format!("`{sym}`")
                    };
                    outln!(
                        w,
                        "| `{}` | `{}` | {} | {} |",
                        entry.name,
                        entry.oid,
                        peeled_cell,
                        sym_cell
                    );
                }
                outln!(w);
            }
        }
        Ok(())
    }
}

fn render_show_human(w: &mut dyn Write, summary: &RemoteShowJson) -> Result<()> {
    outln!(w, "* remote {}", summary.name);
    outln!(w, "  Fetch URL: {}", summary.fetch_url);
    outln!(w, "  Push  URL: {}", summary.push_url);
    if let Some(head) = &summary.head_branch {
        outln!(w, "  HEAD branch: {head}");
    }
    outln!(w, "  Remote branches:");
    if summary.branches.is_empty() {
        outln!(w, "    (none)");
    } else {
        for branch in &summary.branches {
            let state = match branch.state {
                RemoteShowBranchStateJson::Tracked => "tracked",
                RemoteShowBranchStateJson::New => "new",
                RemoteShowBranchStateJson::Stale => "stale",
            };
            outln!(w, "    {} {}", branch.name, state);
        }
    }
    if !summary.pull.is_empty() {
        outln!(w, "  Local branches configured for pull:");
        for row in &summary.pull {
            let merge_short = row
                .merges
                .strip_prefix("refs/heads/")
                .unwrap_or(row.merges.as_str());
            outln!(w, "    {} merges with remote {}", row.local, merge_short);
        }
    }
    Ok(())
}

fn render_show_markdown(w: &mut dyn Write, summary: &RemoteShowJson) -> Result<()> {
    outln!(w, "## Remote `{}`\n", summary.name);
    outln!(w, "| | |");
    outln!(w, "| --- | --- |");
    outln!(w, "| Fetch URL | {} |", summary.fetch_url);
    outln!(w, "| Push URL | {} |", summary.push_url);
    if let Some(head) = &summary.head_branch {
        outln!(w, "| HEAD branch | `{head}` |");
    }
    outln!(w);
    outln!(w, "### Remote branches\n");
    if summary.branches.is_empty() {
        outln!(w, "No remote branches.\n");
    } else {
        outln!(w, "| Branch | State |");
        outln!(w, "| --- | --- |");
        for branch in &summary.branches {
            let state = match branch.state {
                RemoteShowBranchStateJson::Tracked => "tracked",
                RemoteShowBranchStateJson::New => "new",
                RemoteShowBranchStateJson::Stale => "stale",
            };
            outln!(w, "| `{}` | {} |", branch.name, state);
        }
        outln!(w);
    }
    if !summary.pull.is_empty() {
        outln!(w, "### Pull configuration\n");
        outln!(w, "| Local branch | Merges |");
        outln!(w, "| --- | --- |");
        for row in &summary.pull {
            outln!(w, "| `{}` | `{}` |", row.local, row.merges);
        }
        outln!(w);
    }
    Ok(())
}

/// Remotes as a two-column table.
fn print_remote_list(w: &mut dyn Write, remotes: &[RemoteEntry]) -> anyhow::Result<()> {
    if remotes.is_empty() {
        outln!(
            w,
            "No remotes. Add one with: `grit remote add <name> <url>`."
        );
        return Ok(());
    }
    outln!(w, "| Name | URL |");
    outln!(w, "| --- | --- |");
    for remote in remotes {
        outln!(
            w,
            "| `{}` | {} |",
            remote.name,
            escape_table_cell(&remote.url)
        );
    }
    Ok(())
}
