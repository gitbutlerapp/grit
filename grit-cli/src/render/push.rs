//! Outcome types and renderers for `grit push`.

use std::io::Write;

use anyhow::Result;
use grit_lib::remote::push_report::PushRefStatus;
use schemars::JsonSchema;
use serde::Serialize;

use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use crate::progress::summary::TransferSummary;
use crate::render::list::format_inline_code;
use crate::render::shared_branch::{IntegratedJson, SharedHead};

/// Result of `grit push`: the per-ref outcomes for the remote.
#[derive(Serialize, JsonSchema)]
pub struct PushOutcome {
    /// Remote name, or the URL pushed to with any credentials removed.
    pub remote: String,
    /// High-level push kind (`branch`, `tags`, `delete`, `refspecs`).
    pub kind: PushKind,
    /// Local branch short name when [`Self::kind`] is `branch` or `delete` (legacy `branch` field).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub results: Vec<PushRefResult>,
    pub rejected: bool,
    /// True when `--dry-run` was used (no refs or objects written).
    pub dry_run: bool,
    /// Branch short names whose upstream was configured (`-u` / `--set-upstream`).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub upstream_set: Option<Vec<String>>,
    /// URL saved as `origin`: a `<owner>/<project>` push from a repository with no remotes.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub origin_added: Option<String>,
    /// Hook and diagnostic lines from the remote (`remote: …` in human output).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(skip_serializing_if = "Vec::is_empty")]
    pub remote_messages: Vec<String>,
    /// For each shared branch pushed (`grit share`): the other members'
    /// heads, fetched after the push and compared with yours.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub shared: Vec<PushSharedBranch>,
    /// Branches this push made again after a share of them was integrated
    /// (scenario `pushed_to_integrated`): nobody shares them now. Each has
    /// the options `grit share resolve` takes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub pushed_to_integrated: Vec<PushedToIntegrated>,
    /// Pack build and send timing, for the human summary after ref results.
    #[serde(skip)]
    #[schemars(skip)]
    pub transfer: TransferSummary,
}

/// A shared branch a push replaced your head of.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PushSharedBranch {
    /// The branch.
    pub branch: String,
    /// The other members' heads, compared with yours.
    pub heads: Vec<SharedHead>,
    /// Members whose diverged heads went together with yours without losing
    /// anything: combined into your branch here, and pushed again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub combined: Vec<String>,
    /// Your head afterwards, when combining moved it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(default)]
    pub head: Option<String>,
}

/// A branch pushed again after a share of it was integrated.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PushedToIntegrated {
    /// The branch.
    pub branch: String,
    /// What happened, and the options.
    #[serde(flatten)]
    pub report: IntegratedJson,
}

impl PushOutcome {
    /// Whether a shared branch pushed needs someone to decide what to do.
    pub fn needs_decision(&self) -> bool {
        !self.pushed_to_integrated.is_empty()
            || self
                .shared
                .iter()
                .any(|s| s.heads.iter().any(SharedHead::needs_decision))
    }
}

/// JSON classification for [`PushOutcome`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PushKind {
    Branch,
    Tags,
    Delete,
    Refspecs,
}

/// Per-ref push status in JSON (`ok`, `up_to_date`, or `rejected`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PushRefOutcomeStatus {
    Ok,
    UpToDate,
    Rejected,
}

/// One ref's push result.
#[derive(Serialize, JsonSchema)]
pub struct PushRefResult {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub status: PushRefOutcomeStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub old_oid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub new_oid: Option<String>,
    pub forced: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub src: Option<String>,
    /// True when this ref was deleted on the remote (omit when false).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub deleted: Option<bool>,
    /// The library's status, which [`Self::status`] and [`Self::reason`] summarize.
    #[serde(skip)]
    #[schemars(skip)]
    pub detail: PushRefStatus,
}

/// Short display name for a pushed remote ref (`refs/heads/foo` → `foo`).
pub fn ref_short_display(full_ref: &str) -> &str {
    for prefix in ["refs/heads/", "refs/tags/", "refs/remotes/"] {
        if let Some(rest) = full_ref.strip_prefix(prefix) {
            return rest;
        }
    }
    full_ref
}

pub fn push_result_source_label(result: &PushRefResult, kind: PushKind) -> &str {
    if let Some(src) = result.src.as_deref() {
        return ref_short_display(src);
    }
    if kind == PushKind::Tags {
        return "--tags";
    }
    ref_short_display(&result.ref_name)
}

impl HumanRender for PushOutcome {
    fn render_human(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        for result in &self.results {
            let target = format!("{} {}", self.remote, result.ref_name);
            match result.status {
                PushRefOutcomeStatus::Ok => {
                    if result.deleted == Some(true) {
                        let short = ref_short_display(&result.ref_name);
                        if self.dry_run {
                            outln!(w, "  would delete {short} from {}", self.remote);
                        } else {
                            outln!(w, "  deleted {short} from {}", self.remote);
                        }
                    } else {
                        let label = push_result_source_label(result, self.kind);
                        if self.dry_run {
                            outln!(w, "  would push {label} → {target}");
                        } else {
                            outln!(w, "  pushed {label} → {target}");
                        }
                    }
                }
                PushRefOutcomeStatus::UpToDate => outln!(w, "  {target} already up to date"),
                PushRefOutcomeStatus::Rejected => match result.detail {
                    PushRefStatus::RejectStale => outln!(
                        w,
                        "  rejected {target}: stale remote ref — run `grit fetch --prune` and try again"
                    ),
                    PushRefStatus::RejectRemoteRefMissing => {
                        outln!(w, "  rejected {target}: remote ref does not exist")
                    }
                    PushRefStatus::RejectAlreadyExists => {
                        if result.reason.as_deref() == Some("tag_exists") {
                            outln!(
                                w,
                                "  rejected {target}: tag already exists (use `--force` to replace)"
                            );
                        } else {
                            outln!(
                                w,
                                "  rejected {target}: created on the remote meanwhile — run `grit fetch` and try again"
                            );
                        }
                    }
                    _ => {
                        let detail = result.reason.as_deref().unwrap_or("rejected");
                        outln!(w, "  rejected {target}: {detail}");
                    }
                },
            }
        }
        if let Some(url) = &self.origin_added {
            outln!(w, "  added remote origin → {url}");
        }
        for s in &self.shared {
            if !s.combined.is_empty() {
                outln!(
                    w,
                    "✓ combined {} with {}'s head (nothing lost) and pushed it{}",
                    s.branch,
                    s.combined.join("'s and "),
                    s.head
                        .as_deref()
                        .map(|h| format!(": {}", grit_lib::objects::short_hex(h)))
                        .unwrap_or_default()
                );
            }
            for h in &s.heads {
                if s.combined.contains(&h.user) {
                    // Nothing to decide: their next sync takes the result.
                    outln!(
                        w,
                        "  {}: {}'s head {} is in yours now; their next sync takes it",
                        s.branch,
                        h.user,
                        grit_lib::objects::short_hex(&h.tip),
                    );
                    continue;
                }
                outln!(
                    w,
                    "  {}: {}'s head {} {}",
                    s.branch,
                    h.user,
                    grit_lib::objects::short_hex(&h.tip),
                    h.relation.words()
                );
                if let Some(d) = &h.divergence {
                    d.render_human(w, "    ")?;
                }
            }
        }
        for p in &self.pushed_to_integrated {
            p.report.render_human(w, &p.branch)?;
        }
        if self.transfer.received_anything() {
            outln!(w);
            self.transfer.render_rows(w, ctx.color)?;
        }
        Ok(())
    }
}

impl MarkdownRender for PushOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        print_remote_push_messages(w, &self.remote_messages)?;
        print_push_results(w, &self.remote, self.kind, self.dry_run, &self.results)?;
        for s in &self.shared {
            writeln!(w, "\n## Shared branch `{}`\n", s.branch)?;
            if !s.combined.is_empty() {
                writeln!(
                    w,
                    "Combined with {}'s head (nothing lost) and pushed the result{}.\n",
                    s.combined.join("'s and "),
                    s.head
                        .as_deref()
                        .map(|h| format!(", `{}`", grit_lib::objects::short_hex(h)))
                        .unwrap_or_default()
                )?;
            }
            if s.heads.is_empty() {
                writeln!(w, "Nobody else has pushed a head yet.")?;
            }
            for h in &s.heads {
                if s.combined.contains(&h.user) {
                    writeln!(
                        w,
                        "- `{}`'s head `{}`: in yours now; their next sync takes it",
                        h.user,
                        grit_lib::objects::short_hex(&h.tip),
                    )?;
                    continue;
                }
                writeln!(
                    w,
                    "- `{}`'s head `{}`: {}",
                    h.user,
                    grit_lib::objects::short_hex(&h.tip),
                    h.relation.words()
                )?;
                if let Some(d) = &h.divergence {
                    d.render_markdown(w, "  ")?;
                }
            }
            if s.heads.iter().any(SharedHead::needs_decision) {
                writeln!(w, "\nExit status 3: pushed, decision needed.")?;
            }
        }
        for p in &self.pushed_to_integrated {
            writeln!(w, "\n## Pushed to an integrated branch: `{}`\n", p.branch)?;
            p.report.render_markdown(w, &p.branch)?;
            writeln!(w, "\nExit status 3: pushed, decision needed.")?;
        }
        if let Some(url) = &self.origin_added {
            writeln!(w, "\nAdded remote `origin` → `{url}`.")?;
        }
        Ok(())
    }
}

/// Remote hook/diagnostic lines from a push (side-band channel 2).
fn print_remote_push_messages(w: &mut dyn Write, messages: &[String]) -> anyhow::Result<()> {
    if messages.is_empty() {
        return Ok(());
    }
    outln!(w, "### Remote messages");
    for line in messages {
        outln!(w, "- {}", format_inline_code(line));
    }
    Ok(())
}

/// Per-ref push results.
fn print_push_results(
    w: &mut dyn Write,
    remote: &str,
    kind: PushKind,
    dry_run: bool,
    results: &[PushRefResult],
) -> anyhow::Result<()> {
    if results.is_empty() {
        outln!(w, "No refs pushed to `{remote}`.");
        return Ok(());
    }
    outln!(w, "## Push to `{remote}`");
    for result in results {
        let target = format!("{remote} {}", result.ref_name);
        match result.status {
            PushRefOutcomeStatus::Ok => {
                if result.deleted == Some(true) {
                    let short = ref_short_display(&result.ref_name);
                    if dry_run {
                        outln!(
                            w,
                            "- **`{}`** · would delete `{short}` from `{remote}`",
                            result.ref_name
                        );
                    } else {
                        outln!(
                            w,
                            "- **`{}`** · deleted `{short}` from `{remote}`",
                            result.ref_name
                        );
                    }
                } else {
                    let label = push_result_source_label(result, kind);
                    if dry_run {
                        outln!(
                            w,
                            "- **`{}`** · would push `{label}` → `{target}`",
                            result.ref_name
                        );
                    } else {
                        outln!(
                            w,
                            "- **`{}`** · pushed `{label}` → `{target}`",
                            result.ref_name
                        );
                    }
                }
            }
            PushRefOutcomeStatus::UpToDate => outln!(
                w,
                "- **`{}`** · `{target}` already up to date",
                result.ref_name
            ),
            PushRefOutcomeStatus::Rejected => {
                let reason = match result.reason.as_deref() {
                    Some("tag_exists") => {
                        "tag already exists (use `--force` to replace)".to_owned()
                    }
                    other => other.unwrap_or("rejected").to_owned(),
                };
                outln!(
                    w,
                    "- **`{}`** · rejected `{target}`: {reason}",
                    result.ref_name
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn remote_push_messages_use_safe_inline_code() {
        let mut buf = Vec::new();
        print_remote_push_messages(&mut buf, &["use `ticket` in message".to_owned()]).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("``use `ticket` in message``"));
    }

    use super::*;

    #[test]
    fn push_status_serializes_snake_case() {
        for (detail, expected) in [
            (PushRefStatus::Ok, "ok"),
            (PushRefStatus::UpToDate, "up_to_date"),
            (PushRefStatus::RejectFetchFirst, "rejected"),
        ] {
            let result = result_with(detail, None);
            let value = serde_json::to_value(&result).expect("json");
            assert_eq!(value["status"].as_str(), Some(expected));
            assert!(value.get("detail").is_none(), "{value}");
        }
    }

    fn result_with(detail: PushRefStatus, reason: Option<&str>) -> PushRefResult {
        PushRefResult {
            ref_name: "refs/heads/main".to_owned(),
            status: PushRefOutcomeStatus::from(&detail),
            reason: reason.map(str::to_owned),
            old_oid: None,
            new_oid: None,
            forced: false,
            src: None,
            deleted: None,
            detail,
        }
    }

    #[test]
    fn human_rejection_wording_follows_the_library_status() {
        let ctx = RenderContext::human_for_tests();
        let render = |result: PushRefResult| {
            let outcome = PushOutcome {
                remote: "origin".to_owned(),
                kind: PushKind::Branch,
                branch: Some("main".to_owned()),
                results: vec![result],
                rejected: true,
                dry_run: false,
                upstream_set: None,
                origin_added: None,
                remote_messages: Vec::new(),
                shared: Vec::new(),
                pushed_to_integrated: Vec::new(),
                transfer: TransferSummary::default(),
            };
            let mut out = Vec::new();
            outcome.render_human(&mut out, &ctx).expect("render");
            String::from_utf8(out).expect("utf8")
        };

        let stale = render(result_with(PushRefStatus::RejectStale, Some("stale")));
        assert!(
            stale.contains("stale remote ref — run `grit fetch --prune`"),
            "{stale}"
        );
        let missing = render(result_with(
            PushRefStatus::RejectRemoteRefMissing,
            Some("remote_ref_missing"),
        ));
        assert!(missing.contains("remote ref does not exist"), "{missing}");
        let exists = render(result_with(
            PushRefStatus::RejectAlreadyExists,
            Some("already_exists"),
        ));
        assert!(
            exists.contains("created on the remote meanwhile"),
            "{exists}"
        );

        let declined = render(result_with(PushRefStatus::RemoteRejected, Some("stale")));
        assert!(
            declined.contains("rejected origin refs/heads/main: stale"),
            "a remote's own message is shown as given: {declined}"
        );
    }
}
