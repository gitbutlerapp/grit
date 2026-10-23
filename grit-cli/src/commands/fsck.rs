//! `grit fsck` — check repository integrity.

use std::collections::BTreeMap;
use std::io::Write;

use anyhow::Result;
use grit_lib::config::ConfigSet;
use grit_lib::maintenance::repository_fsck::{
    issue_msg_id, issue_severity, issue_visible_by_default, FsckIssue, FsckOptions, FsckReport,
    FsckSeverity,
};
use grit_lib::progress::{NeverCancel, ProgressSink};
use schemars::JsonSchema;
use serde::{Serialize, Serializer};

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
    /// Include info-level issues in human listing (not serialized).
    #[serde(skip)]
    pub verbose: bool,
}

#[derive(Serialize, JsonSchema)]
pub struct FsckIssueJson {
    pub kind: String,
    #[serde(serialize_with = "serialize_severity")]
    #[schemars(with = "String")]
    pub severity: FsckSeverity,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub detail: String,
    /// Whether default human output lists this issue (not serialized).
    #[serde(skip)]
    pub show_in_human_default: bool,
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
        Self {
            ok: report.ok(config, options),
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
                    kind: kind.as_str().to_owned(),
                    oid: oid.to_hex(),
                })
                .collect(),
            verbose,
        }
    }

    fn issues_for_human(&self) -> Vec<&FsckIssueJson> {
        self.issues
            .iter()
            .filter(|issue| self.verbose || issue.show_in_human_default)
            .collect()
    }
}

fn visible_has_warning_or_error(issues: &[&FsckIssueJson]) -> bool {
    issues
        .iter()
        .any(|issue| issue.severity != FsckSeverity::Info)
}

fn severity_label(severity: FsckSeverity) -> &'static str {
    match severity {
        FsckSeverity::Error => "error",
        FsckSeverity::Warning => "warning",
        FsckSeverity::Info => "info",
    }
}

fn serialize_severity<S: Serializer>(severity: &FsckSeverity, ser: S) -> Result<S::Ok, S::Error> {
    ser.serialize_str(severity_label(*severity))
}

fn issue_to_json(issue: &FsckIssue, config: &ConfigSet, strict: bool) -> FsckIssueJson {
    FsckIssueJson {
        kind: issue_msg_id(issue).to_owned(),
        severity: issue_severity(issue, config, strict),
        oid: issue.oid().map(|o| o.to_hex()),
        path: issue.location().map(|p| p.into_owned()),
        detail: issue.detail(),
        show_in_human_default: issue_visible_by_default(issue, config, strict),
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

        if !self.ok {
            outln!(w, "\nIntegrity check failed.");
        } else if visible_has_warning_or_error(&visible) {
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
                    severity_label(issue.severity),
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
        if !self.ok {
            outln!(w, "\n**Integrity check failed.**");
        } else if visible_has_warning_or_error(&self.issues_for_human()) {
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
    if outcome.ok {
        0
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grit_lib::maintenance::repository_fsck::MissingObjectReferrer;
    use grit_lib::objects::ObjectId;

    #[test]
    fn json_issue_maps_missing_object() {
        let issue = FsckIssue::MissingObject {
            oid: ObjectId::from_hex("abcd1234abcd1234abcd1234abcd1234abcd1234").expect("oid"),
            referrer: MissingObjectReferrer::Object(
                ObjectId::from_hex("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef").expect("oid"),
            ),
        };
        let config = ConfigSet::default();
        let json = issue_to_json(&issue, &config, false);
        assert_eq!(json.kind, "missingObject");
        assert_eq!(json.severity, FsckSeverity::Error);
        assert!(json.oid.is_some());
        let value = serde_json::to_value(&json).expect("serialize issue");
        assert_eq!(value["severity"], "error");
    }

    #[test]
    fn missing_object_kind_is_stable() {
        let issue = FsckIssue::MissingObject {
            oid: ObjectId::from_hex("abcd1234abcd1234abcd1234abcd1234abcd1234").expect("oid"),
            referrer: MissingObjectReferrer::Object(
                ObjectId::from_hex("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef").expect("oid"),
            ),
        };
        assert_eq!(issue_msg_id(&issue), "missingObject");
    }
}
