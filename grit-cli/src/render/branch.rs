//! `grit branch` outcomes and rendering.

use anyhow::Result;
use grit_lib::objects::short_hex;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use crate::render::table::escape_table_cell;

fn is_false(value: &bool) -> bool {
    !*value
}

/// Which branches were listed (human / markdown empty-state text only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchListScope {
    Local,
    Remote,
    All,
}

/// Empty-state message for a branch list scope.
#[must_use]
pub fn empty_list_message(scope: BranchListScope) -> &'static str {
    match scope {
        BranchListScope::Local => "No branches yet.",
        BranchListScope::Remote => "No remote-tracking branches.",
        BranchListScope::All => "No branches.",
    }
}

/// Result of `grit branch`, tagged by `action`.
#[derive(Serialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum BranchOutcome {
    List {
        /// The current branch, or `null` when detached / unborn.
        current: Option<String>,
        branches: Vec<BranchEntry>,
        /// True when more branches matched than returned (`-n` / `--limit`).
        #[serde(default, skip_serializing_if = "is_false")]
        #[schemars(default)]
        truncated: bool,
        #[serde(skip)]
        #[schemars(skip)]
        scope: BranchListScope,
        #[serde(skip)]
        #[schemars(skip)]
        show_current: bool,
    },
    Create {
        name: String,
        oid: String,
        short_oid: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(skip_serializing_if = "Option::is_none")]
        upstream: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        #[schemars(default)]
        dry_run: bool,
    },
    Delete {
        name: String,
        oid: String,
        short_oid: String,
        #[serde(default, skip_serializing_if = "is_false")]
        #[schemars(default)]
        dry_run: bool,
    },
    Rename {
        old: String,
        new: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(skip_serializing_if = "Option::is_none")]
        oid: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(skip_serializing_if = "Option::is_none")]
        short_oid: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(skip_serializing_if = "Option::is_none")]
        replaced: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        #[schemars(default)]
        dry_run: bool,
    },
    SetUpstream {
        branch: String,
        upstream: String,
        #[serde(default, skip_serializing_if = "is_false")]
        #[schemars(default)]
        dry_run: bool,
    },
    UnsetUpstream {
        branch: String,
        #[serde(default, skip_serializing_if = "is_false")]
        #[schemars(default)]
        dry_run: bool,
    },
}

/// Configured upstream for a local branch in a `list` outcome.
#[derive(Serialize, JsonSchema)]
pub struct BranchUpstreamEntry {
    #[serde(rename = "ref")]
    pub ref_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub ahead: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub behind: Option<usize>,
    #[serde(default, skip_serializing_if = "is_false")]
    #[schemars(default)]
    pub gone: bool,
}

/// One branch in a `list` outcome.
#[derive(Serialize, JsonSchema)]
pub struct BranchEntry {
    pub name: String,
    pub current: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    #[schemars(default)]
    pub is_remote: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub full_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub oid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub short_oid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub upstream: Option<BranchUpstreamEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
}

fn format_upstream_marker(up: &BranchUpstreamEntry) -> String {
    if up.gone {
        return "[gone]".to_owned();
    }
    match (up.ahead, up.behind) {
        (Some(ahead), Some(behind)) => {
            format!("[{}: ahead {ahead}, behind {behind}]", up.ref_name)
        }
        _ => format!("[{}]", up.ref_name),
    }
}

fn markdown_branch_list(
    w: &mut dyn Write,
    branches: &[BranchEntry],
    scope: BranchListScope,
) -> Result<()> {
    if branches.is_empty() {
        outln!(w, "{}", empty_list_message(scope));
        return Ok(());
    }
    outln!(w, "## Branches");
    outln!(
        w,
        "| Branch | Current | OID | Subject | Upstream | Worktree |"
    );
    outln!(w, "| --- | --- | --- | --- | --- | --- |");
    for branch in branches {
        let short_oid = branch
            .short_oid
            .as_deref()
            .or(branch.oid.as_deref())
            .map(short_hex)
            .unwrap_or("");
        let current = if branch.current { "yes" } else { "" };
        let subject = branch
            .subject
            .as_deref()
            .map(escape_table_cell)
            .unwrap_or_default();
        let upstream = branch
            .upstream
            .as_ref()
            .map(|up| {
                let label = if up.gone {
                    format!("{} [gone]", up.ref_name)
                } else {
                    match (up.ahead, up.behind) {
                        (Some(a), Some(b)) => format!("{} (+{a}/-{b})", up.ref_name),
                        _ => up.ref_name.clone(),
                    }
                };
                escape_table_cell(&label)
            })
            .unwrap_or_default();
        let worktree = branch
            .worktree
            .as_deref()
            .map(escape_table_cell)
            .unwrap_or_default();
        outln!(
            w,
            "| `{}` | {} | `{}` | {} | {} | {} |",
            escape_table_cell(&branch.name),
            current,
            short_oid,
            subject,
            upstream,
            worktree
        );
    }
    Ok(())
}

impl HumanRender for BranchOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self {
            BranchOutcome::List {
                branches,
                scope,
                show_current,
                current,
                ..
            } => {
                if *show_current {
                    if let Some(name) = current {
                        outln!(w, "{name}");
                    }
                    return Ok(());
                }
                if branches.is_empty() {
                    outln!(w, "{}", empty_list_message(*scope));
                    return Ok(());
                }
                for branch in branches {
                    let marker = branch
                        .upstream
                        .as_ref()
                        .map(format_upstream_marker)
                        .unwrap_or_default();
                    let prefix = if branch.current { "* " } else { "  " };
                    if marker.is_empty() {
                        outln!(w, "{prefix}{}", branch.name);
                    } else {
                        outln!(w, "{prefix}{} {marker}", branch.name);
                    }
                }
            }
            BranchOutcome::Create {
                name,
                short_oid,
                upstream,
                dry_run,
                ..
            } => {
                if *dry_run {
                    outln!(w, "Would create branch {name} at {short_oid}");
                } else {
                    outln!(w, "Created branch {name} at {short_oid}");
                }
                if let Some(up) = upstream {
                    outln!(w, "  tracking {up}");
                }
            }
            BranchOutcome::Delete {
                name,
                short_oid,
                dry_run,
                ..
            } => {
                if *dry_run {
                    outln!(w, "Would delete branch {name} (was {short_oid}).");
                } else {
                    outln!(w, "Deleted branch {name} (was {short_oid}).");
                }
            }
            BranchOutcome::Rename {
                old, new, dry_run, ..
            } => {
                if *dry_run {
                    outln!(w, "Would rename branch {old} to {new}");
                } else {
                    outln!(w, "Renamed branch {old} to {new}");
                }
            }
            BranchOutcome::SetUpstream {
                branch,
                upstream,
                dry_run,
            } => {
                if *dry_run {
                    outln!(w, "Would set upstream of {branch} to {upstream}");
                } else {
                    outln!(w, "Set upstream of {branch} to {upstream}");
                }
            }
            BranchOutcome::UnsetUpstream { branch, dry_run } => {
                if *dry_run {
                    outln!(w, "Would unset upstream of {branch}");
                } else {
                    outln!(w, "Unset upstream of {branch}");
                }
            }
        }
        Ok(())
    }
}

impl MarkdownRender for BranchOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self {
            BranchOutcome::List {
                branches, scope, ..
            } => markdown_branch_list(w, branches, *scope)?,
            BranchOutcome::Create {
                name,
                short_oid,
                upstream,
                dry_run,
                ..
            } => {
                let verb = if *dry_run { "Would create" } else { "Created" };
                if let Some(up) = upstream {
                    outln!(
                        w,
                        "{verb} branch `{name}` at `{short_oid}` (tracking `{up}`)."
                    );
                } else {
                    outln!(w, "{verb} branch `{name}` at `{short_oid}`.");
                }
            }
            BranchOutcome::Delete {
                name,
                short_oid,
                dry_run,
                ..
            } => {
                let verb = if *dry_run { "Would delete" } else { "Deleted" };
                outln!(w, "{verb} branch `{name}` (was `{short_oid}`).");
            }
            BranchOutcome::Rename {
                old, new, dry_run, ..
            } => {
                let verb = if *dry_run { "Would rename" } else { "Renamed" };
                outln!(w, "{verb} branch `{old}` to `{new}`.");
            }
            BranchOutcome::SetUpstream {
                branch,
                upstream,
                dry_run,
            } => {
                let verb = if *dry_run { "Would set" } else { "Set" };
                outln!(w, "{verb} upstream of `{branch}` to `{upstream}`.");
            }
            BranchOutcome::UnsetUpstream { branch, dry_run } => {
                let verb = if *dry_run { "Would unset" } else { "Unset" };
                outln!(w, "{verb} upstream of `{branch}`.");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_list_table_escapes_pipe_in_subject() {
        let branches = vec![BranchEntry {
            name: "main".to_owned(),
            current: true,
            is_remote: false,
            full_ref: None,
            remote: None,
            oid: Some("a".repeat(40)),
            short_oid: Some("aaaaaaa".to_owned()),
            subject: Some("fix | widget".to_owned()),
            date: None,
            upstream: None,
            worktree: None,
        }];
        let mut buf = Vec::new();
        markdown_branch_list(&mut buf, &branches, BranchListScope::Local).expect("markdown");
        let text = String::from_utf8(buf).expect("utf8");
        assert!(text.contains("fix \\| widget"), "{text}");
    }
}
