//! Outcome types and renderers for `grit fetch`.

use std::io::Write;

use anyhow::Result;
use grit_lib::objects::short_hex;
use schemars::JsonSchema;
use serde::Serialize;

use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use crate::progress::summary::TransferSummary;
use crate::render::promisor::{
    human_lines, markdown_line, markdown_warning_lines, PromisorRemoteJson, PromisorWarningJson,
    StoredFieldJson,
};

/// Result of `grit fetch`: the refs that changed.
#[derive(Serialize, JsonSchema)]
pub struct FetchOutcome {
    /// Remote name, or the URL fetched from with any credentials removed.
    pub remote: String,
    pub updates: Vec<FetchUpdate>,
    pub updated: usize,
    /// True when the fetch was a dry run (no refs or objects written).
    pub dry_run: bool,
    /// True when any ref update was rejected (exit 1 when set).
    pub rejected: bool,
    /// Shallow boundary changes when a depth/deepen/unshallow fetch ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub shallow: Option<FetchShallowDelta>,
    /// Promisor remotes the server advertised, and whether this fetch accepted each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub promisor_remotes: Vec<PromisorRemoteJson>,
    /// Remote fields stored from the advertisement (`promisor.storeFields`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub stored_fields: Vec<StoredFieldJson>,
    /// Promisor config or advertised values that were ignored or could not be used.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub promisor_warnings: Vec<PromisorWarningJson>,
    /// What was received and where the time went, for the human summary.
    #[serde(skip)]
    #[schemars(skip)]
    pub transfer: TransferSummary,
    /// Refs this fetch changed, or on a dry run would change.
    #[serde(skip)]
    #[schemars(skip)]
    pub changed: usize,
}

/// Shallow boundaries added or removed by this fetch (`--json` only).
#[derive(Serialize, JsonSchema)]
pub struct FetchShallowDelta {
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

/// Result of `grit fetch --all`.
#[derive(Serialize, JsonSchema)]
pub struct FetchAllOutcome {
    pub remotes: Vec<FetchAllRemoteOutcome>,
    pub updated: usize,
    pub dry_run: bool,
    /// Totals across every remote, for the human summary.
    #[serde(skip)]
    #[schemars(skip)]
    pub transfer: TransferSummary,
}

/// One remote's result inside [`FetchAllOutcome`].
#[derive(Serialize, JsonSchema)]
pub struct FetchAllRemoteOutcome {
    /// Remote name.
    pub remote: String,
    pub updates: Vec<FetchUpdate>,
    pub updated: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Refs this remote's fetch changed, or on a dry run would change.
    #[serde(skip)]
    #[schemars(skip)]
    pub changed: usize,
    /// True when any of this remote's ref updates was rejected.
    #[serde(skip)]
    #[schemars(skip)]
    pub rejected: bool,
}

impl FetchAllRemoteOutcome {
    /// This remote's result in the single-remote shape, for rendering.
    pub(crate) fn as_single(&self, dry_run: bool) -> FetchOutcome {
        FetchOutcome {
            remote: self.remote.clone(),
            updates: self.updates.clone(),
            updated: self.updated,
            dry_run,
            rejected: self.rejected,
            shallow: None,
            promisor_remotes: Vec::new(),
            stored_fields: Vec::new(),
            promisor_warnings: Vec::new(),
            transfer: TransferSummary::default(),
            changed: self.changed,
        }
    }
}

impl HumanRender for FetchAllOutcome {
    fn render_human(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        for remote in &self.remotes {
            if let Some(err) = &remote.error {
                outln!(w, "From {}:", remote.remote);
                outln!(w, "  error: {err}");
                continue;
            }
            remote.as_single(self.dry_run).render_human(w, ctx)?;
        }
        render_transfer(&self.transfer, w, ctx.color)
    }
}

impl MarkdownRender for FetchAllOutcome {
    fn render_markdown(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        for remote in &self.remotes {
            if let Some(err) = &remote.error {
                outln!(w, "## {}", remote.remote);
                outln!(w, "- error: {err}");
                continue;
            }
            remote.as_single(self.dry_run).render_markdown(w, ctx)?;
        }
        Ok(())
    }
}

/// Per-ref fetch status in JSON.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FetchUpdateStatus {
    New,
    FastForward,
    Forced,
    Pruned,
    Rejected,
    UpToDate,
}

/// One updated tracking ref. `old_oid`/`new_oid` are full hex, or `null` for a
/// newly-created (`old_oid`) or deleted (`new_oid`) ref.
#[derive(Clone, Serialize, JsonSchema)]
pub struct FetchUpdate {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub remote_ref: String,
    pub status: FetchUpdateStatus,
    pub old_oid: Option<String>,
    pub new_oid: Option<String>,
    /// Rejection detail, present only when `status == "rejected"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl HumanRender for FetchOutcome {
    fn render_human(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        for update in &self.updates {
            if update.status == FetchUpdateStatus::Rejected {
                let reason = update.reason.as_deref().unwrap_or("ref update rejected");
                outln!(w, "  ! rejected {}  {}", update.ref_name, reason);
                continue;
            }
            let from = update.old_oid.as_deref().map_or("new", short_hex);
            let to = update.new_oid.as_deref().map_or("deleted", short_hex);
            if update.remote_ref.is_empty() || update.remote_ref == update.ref_name {
                outln!(w, "  {}  {from} → {to}", update.ref_name);
            } else {
                outln!(
                    w,
                    "  {}  ← {}  {from} → {to}",
                    update.ref_name,
                    update.remote_ref
                );
            }
        }
        if self.rejected {
            outln!(w, "Some ref updates from {} were rejected.", self.remote);
        } else if self.changed == 0 {
            outln!(w, "Already up to date with {}.", self.remote);
        } else {
            outln!(
                w,
                "{} {} update{} from {}.",
                if self.dry_run {
                    "Would fetch"
                } else {
                    "Fetched"
                },
                self.changed,
                plural(self.changed),
                self.remote
            );
        }
        for line in human_lines(
            &self.promisor_remotes,
            &self.stored_fields,
            &self.promisor_warnings,
        ) {
            outln!(w, "  promisor  {line}");
        }
        render_transfer(&self.transfer, w, ctx.color)
    }
}

/// The time bar and data/objects rows under a fetch's ref list, when the
/// fetch received anything.
fn render_transfer(transfer: &TransferSummary, w: &mut dyn Write, color: bool) -> Result<()> {
    if !transfer.received_anything() {
        return Ok(());
    }
    outln!(w);
    transfer.render_rows(w, color)
}

impl MarkdownRender for FetchOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        print_fetch_updates(w, &self.remote, &self.updates, self.changed, self.dry_run)?;
        if let Some(line) = markdown_line(&self.promisor_remotes) {
            outln!(w, "{line}");
        }
        for s in &self.stored_fields {
            outln!(w, "- **stored**: `{}` for `{}`", s.field, s.remote);
        }
        for line in markdown_warning_lines(&self.promisor_warnings) {
            outln!(w, "{line}");
        }
        Ok(())
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Fetch ref updates as bullets (`ref` · old → new). `changed` counts the refs
/// the fetch changed, or on a dry run would change.
fn print_fetch_updates(
    w: &mut dyn Write,
    remote: &str,
    updates: &[FetchUpdate],
    changed: usize,
    dry_run: bool,
) -> anyhow::Result<()> {
    if updates.is_empty() && changed == 0 {
        outln!(w, "Already up to date with `{remote}`.");
        return Ok(());
    }
    outln!(w, "## Updates from `{remote}`");
    let mut any_rejected = false;
    for update in updates {
        if update.status == FetchUpdateStatus::Rejected {
            any_rejected = true;
            let reason = update.reason.as_deref().unwrap_or("ref update rejected");
            outln!(w, "- **`{}`** · rejected: {reason}", update.ref_name);
            continue;
        }
        let from = update.old_oid.as_deref().map(short_hex).unwrap_or("new");
        let to = update
            .new_oid
            .as_deref()
            .map(short_hex)
            .unwrap_or("deleted");
        if update.remote_ref.is_empty() || update.remote_ref == update.ref_name {
            outln!(w, "- `{}` · `{from}` → `{to}`", update.ref_name);
        } else {
            outln!(
                w,
                "- `{}` ← `{}` · `{from}` → `{to}`",
                update.ref_name,
                update.remote_ref
            );
        }
    }
    if any_rejected {
        outln!(w);
        outln!(w, "Some ref updates from `{remote}` were **rejected**.");
    } else if dry_run && changed > 0 {
        outln!(w);
        outln!(
            w,
            "Would fetch **{changed}** update{} from `{remote}`.",
            if changed == 1 { "" } else { "s" }
        );
    } else if changed > 0 {
        outln!(w);
        outln!(
            w,
            "Fetched **{changed}** update{}.",
            if changed == 1 { "" } else { "s" }
        );
    }
    Ok(())
}
