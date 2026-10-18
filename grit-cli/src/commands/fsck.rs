//! `grit fsck` — check repository integrity.

use std::collections::BTreeMap;
use std::io::Write;

use anyhow::Result;
use grit_lib::config::ConfigSet;
use grit_lib::maintenance::repository_fsck::{
    issue_msg_id, issue_severity, FsckIssue, FsckOptions, FsckReport, FsckSeverity,
};
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::progress::{NeverCancel, ProgressSink};
use schemars::JsonSchema;
use serde::Serialize;

use crate::context;
use crate::output::{outln, HumanRender, MarkdownRender, OutputMode, RenderContext};
use crate::terminal_progress::{stderr_progress_enabled, TerminalProgress};

/// CLI flags for `grit fsck`.
#[derive(Debug, Clone, Copy)]
pub struct FsckRunOptions {
    /// Skip full object-store validation; check connectivity from ref roots only.
    pub connectivity_only: bool,
    /// List objects not reachable from ref roots (always on for a full fsck).
    pub dangling: bool,
    /// Skip dangling-object enumeration (matches `git fsck --no-dangling`).
    pub no_dangling: bool,
    /// Treat ref-fsck warnings as errors.
    pub strict: bool,
    /// Include info-level object diagnostics in human output (matches Git `--verbose`).
    pub verbose: bool,
    /// Output mode (controls stderr progress).
    pub mode: OutputMode,
}

/// Result of `grit fsck` / `grit verify`.
#[derive(Serialize, JsonSchema)]
pub struct FsckOutcome {
    /// True when no error-severity issues were found.
    pub ok: bool,
    pub objects_checked: usize,
    pub issues: Vec<FsckIssueJson>,
    pub dangling: Vec<DanglingJson>,
    /// True when at least one issue has error severity (not serialized).
    #[serde(skip)]
    pub has_errors: bool,
    /// Include info-level issues in human listing (not serialized).
    #[serde(skip)]
    pub verbose: bool,
}

#[derive(Serialize, JsonSchema)]
pub struct FsckIssueJson {
    pub kind: String,
    pub severity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub detail: String,
}

#[derive(Serialize, JsonSchema)]
pub struct DanglingJson {
    pub kind: String,
    pub oid: String,
}

impl FsckOutcome {
    fn from_report(
        report: &FsckReport,
        config: &ConfigSet,
        options: &FsckOptions,
        verbose: bool,
    ) -> Self {
        let has_errors = report
            .issues
            .iter()
            .any(|issue| issue_severity(issue, config, options.strict) == FsckSeverity::Error);
        Self {
            ok: !has_errors,
            objects_checked: report.objects_checked,
            issues: report
                .issues
                .iter()
                .map(|issue| issue_to_json(issue, config, options.strict))
                .collect(),
            dangling: report
                .dangling
                .iter()
                .map(|(kind, oid)| DanglingJson {
                    kind: object_kind_label(*kind).to_owned(),
                    oid: oid.to_hex(),
                })
                .collect(),
            has_errors,
            verbose,
        }
    }

    fn issues_for_human(&self) -> Vec<&FsckIssueJson> {
        if self.verbose {
            return self.issues.iter().collect();
        }
        self.issues
            .iter()
            .filter(|issue| issue.severity != "info")
            .collect()
    }
}

fn object_kind_label(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Commit => "commit",
        ObjectKind::Tree => "tree",
        ObjectKind::Blob => "blob",
        ObjectKind::Tag => "tag",
    }
}

fn issue_oid(issue: &FsckIssue) -> Option<ObjectId> {
    match issue {
        FsckIssue::CorruptLoose { oid, .. }
        | FsckIssue::HashMismatch { oid, .. }
        | FsckIssue::BadObject { oid, .. }
        | FsckIssue::MissingObject { oid, .. } => Some(*oid),
        FsckIssue::BrokenLink { tree, .. } => Some(*tree),
        FsckIssue::BadRef(_) | FsckIssue::PackChecksum { .. } | FsckIssue::IdxMismatch { .. } => {
            None
        }
        FsckIssue::MidxInvalid { .. } | FsckIssue::CommitGraphInvalid { .. } => None,
        _ => None,
    }
}

fn issue_path(issue: &FsckIssue) -> Option<String> {
    match issue {
        FsckIssue::BadRef(r) => Some(r.path.clone()),
        FsckIssue::PackChecksum { pack } | FsckIssue::IdxMismatch { pack } => {
            Some(pack.display().to_string())
        }
        FsckIssue::CorruptLoose { path, .. } | FsckIssue::HashMismatch { path, .. } => {
            Some(path.display().to_string())
        }
        FsckIssue::MidxInvalid { .. } => Some("multi-pack-index".to_owned()),
        FsckIssue::CommitGraphInvalid { .. } => Some("commit-graph".to_owned()),
        _ => None,
    }
}

fn issue_detail(issue: &FsckIssue) -> String {
    match issue {
        FsckIssue::CorruptLoose { path, .. } => format!("could not read {}", path.display()),
        FsckIssue::HashMismatch { path, .. } => {
            format!("loose hash mismatch at {}", path.display())
        }
        FsckIssue::BadObject { error, .. } => error.detail.clone(),
        FsckIssue::PackChecksum { pack } => format!("pack checksum invalid for {}", pack.display()),
        FsckIssue::IdxMismatch { pack } => {
            format!("index checksum mismatch for {}", pack.display())
        }
        FsckIssue::MissingObject { oid, referenced_by } => {
            if referenced_by.is_zero() {
                format!("object {} missing (staged in index)", oid.to_hex())
            } else {
                format!(
                    "object {} referenced by {}",
                    oid.to_hex(),
                    referenced_by.to_hex()
                )
            }
        }
        FsckIssue::BrokenLink {
            tree,
            name,
            mode,
            target,
        } => format!(
            "tree {} entry {name} mode {mode:o} -> {}",
            tree.to_hex(),
            target.to_hex()
        ),
        FsckIssue::BadRef(r) => r.detail.clone(),
        FsckIssue::MidxInvalid { details } => details.join("; "),
        FsckIssue::CommitGraphInvalid { detail } => detail.clone(),
        _ => "unknown fsck issue".to_owned(),
    }
}

fn severity_label(severity: FsckSeverity) -> &'static str {
    match severity {
        FsckSeverity::Error => "error",
        FsckSeverity::Warning => "warning",
        FsckSeverity::Info => "info",
    }
}

fn issue_to_json(issue: &FsckIssue, config: &ConfigSet, strict: bool) -> FsckIssueJson {
    let severity = issue_severity(issue, config, strict);
    FsckIssueJson {
        kind: issue_msg_id(issue).to_owned(),
        severity: severity_label(severity).to_owned(),
        oid: issue_oid(issue).map(|o| o.to_hex()),
        path: issue_path(issue),
        detail: issue_detail(issue),
    }
}

impl HumanRender for FsckOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let visible = self.issues_for_human();

        outln!(
            w,
            "Checked {} object{}.",
            self.objects_checked,
            if self.objects_checked == 1 { "" } else { "s" }
        );

        if visible.is_empty() && self.dangling.is_empty() {
            outln!(w, "No integrity problems found.");
            return Ok(());
        }

        if !visible.is_empty() {
            let mut by_kind: BTreeMap<&str, Vec<&&FsckIssueJson>> = BTreeMap::new();
            for issue in &visible {
                by_kind.entry(&issue.kind).or_default().push(issue);
            }
            outln!(w, "\nIssues:");
            for (kind, entries) in by_kind {
                outln!(w, "  {kind} ({}):", entries.len());
                for entry in entries {
                    let target = entry
                        .oid
                        .as_deref()
                        .or(entry.path.as_deref())
                        .unwrap_or("?");
                    outln!(w, "    {target}: {}", entry.detail);
                }
            }
        }

        if !self.dangling.is_empty() {
            outln!(w, "\nUnreachable objects ({}):", self.dangling.len());
            for d in &self.dangling {
                outln!(w, "  {} {}", d.kind, d.oid);
            }
        }

        if self.has_errors {
            outln!(w, "\nIntegrity check failed.");
        } else if !visible.is_empty() {
            outln!(w, "\nCompleted with warnings only.");
        }

        Ok(())
    }
}

impl MarkdownRender for FsckOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "## Fsck\n");
        outln!(w, "- **ok:** {}", self.ok);
        outln!(w, "- **objects_checked:** {}", self.objects_checked);
        if self.issues.is_empty() && self.dangling.is_empty() {
            outln!(w, "\nNo issues reported.");
            return Ok(());
        }
        if !self.issues.is_empty() {
            outln!(w, "\n### Issues\n");
            outln!(w, "| Kind | Severity | OID | Path | Detail |");
            outln!(w, "| --- | --- | --- | --- | --- |");
            for issue in &self.issues {
                outln!(
                    w,
                    "| `{}` | {} | {} | {} | {} |",
                    issue.kind,
                    issue.severity,
                    issue.oid.as_deref().unwrap_or("—"),
                    issue
                        .path
                        .as_deref()
                        .map(|p| format!("`{p}`"))
                        .unwrap_or_else(|| "—".to_owned()),
                    issue.detail
                );
            }
        }
        if !self.dangling.is_empty() {
            outln!(w, "\n### Dangling\n");
            for d in &self.dangling {
                outln!(w, "- `{}` `{}`", d.kind, d.oid);
            }
        }
        let has_visible_warnings = self
            .issues
            .iter()
            .any(|i| i.severity == "warning" || i.severity == "error");
        if self.has_errors {
            outln!(w, "\n**Integrity check failed.**");
        } else if has_visible_warnings {
            outln!(w, "\n**Completed with warnings only.**");
        }
        Ok(())
    }
}

pub fn run(options: FsckRunOptions) -> Result<FsckOutcome> {
    let repo = context::discover()?;
    let config = repo.config()?;
    let mut fsck_opts = if options.connectivity_only {
        FsckOptions::connectivity_and_refs(options.strict)
    } else {
        FsckOptions::full_repository(options.strict)
    };
    if options.no_dangling {
        fsck_opts = fsck_opts.with_check_dangling(false);
    } else if options.connectivity_only {
        fsck_opts = fsck_opts.with_check_dangling(options.dangling);
    }

    let progress_enabled = options.mode == OutputMode::Human && stderr_progress_enabled();
    let mut progress = TerminalProgress::new(progress_enabled);
    let report = repo.fsck(&fsck_opts, &mut progress, &NeverCancel)?;
    progress.finish();

    Ok(FsckOutcome::from_report(
        &report,
        &config,
        &fsck_opts,
        options.verbose,
    ))
}

/// Exit code helper: non-zero when error-severity issues were found.
pub fn exit_code(outcome: &FsckOutcome) -> i32 {
    if outcome.has_errors {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_issue_maps_missing_object() {
        let issue = FsckIssue::MissingObject {
            oid: ObjectId::from_hex("abcd1234abcd1234abcd1234abcd1234abcd1234").expect("oid"),
            referenced_by: ObjectId::from_hex("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef")
                .expect("oid"),
        };
        let config = ConfigSet::default();
        let json = issue_to_json(&issue, &config, false);
        assert_eq!(json.kind, "missingObject");
        assert_eq!(json.severity, "error");
        assert!(json.oid.is_some());
    }

    #[test]
    fn missing_object_kind_is_stable() {
        let issue = FsckIssue::MissingObject {
            oid: ObjectId::from_hex("abcd1234abcd1234abcd1234abcd1234abcd1234").expect("oid"),
            referenced_by: ObjectId::from_hex("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef")
                .expect("oid"),
        };
        assert_eq!(issue_msg_id(&issue), "missingObject");
    }

    #[test]
    fn index_missing_object_detail_omits_zero_referrer() {
        let issue = FsckIssue::MissingObject {
            oid: ObjectId::from_hex("abcd1234abcd1234abcd1234abcd1234abcd1234").expect("oid"),
            referenced_by: ObjectId::zero(),
        };
        let detail = issue_detail(&issue);
        assert!(detail.contains("staged in index"));
        assert!(!detail.contains("0000000"));
    }
}
