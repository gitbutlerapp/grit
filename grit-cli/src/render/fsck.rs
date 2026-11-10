//! Render `grit fsck` / `grit verify` outcomes.

use std::collections::BTreeMap;
use std::io::Write;

use anyhow::Result;
use grit_lib::config::ConfigSet;
use grit_lib::maintenance::repository_fsck::{
    issue_msg_id, FsckDiagnostic, FsckOptions, FsckReport, FsckSeverity,
};
use schemars::JsonSchema;
use serde::{Serialize, Serializer};

use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

/// Result of `grit fsck` / `grit verify`.
#[derive(Serialize, JsonSchema)]
pub struct FsckOutcome {
    /// True when no error-severity issues were found.
    pub ok: bool,
    pub objects_checked: usize,
    pub issues: Vec<FsckIssueJson>,
    pub dangling: Vec<DanglingJson>,
    /// Whether the library classified any findings as warnings (not serialized).
    #[serde(skip)]
    pub has_warnings: bool,
    /// Visible issue indices grouped by message id (not serialized).
    #[serde(skip)]
    pub visible_issue_groups: BTreeMap<String, Vec<usize>>,
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
    /// Whether the library includes this issue in human listings (not serialized).
    #[serde(skip)]
    pub visible: bool,
}

#[derive(Serialize, JsonSchema)]
pub struct DanglingJson {
    pub kind: String,
    pub oid: String,
}

impl FsckOutcome {
    /// Build the CLI outcome from a library fsck report and summary options.
    pub fn from_report(
        report: &FsckReport,
        config: &ConfigSet,
        options: &FsckOptions,
        verbose: bool,
    ) -> Self {
        let summary = report.summary(config, options, verbose);
        let visible_issue_groups = summary.visible_issue_groups();
        Self {
            ok: summary.ok,
            objects_checked: report.objects_checked,
            issues: summary.diagnostics.into_iter().map(issue_to_json).collect(),
            dangling: report
                .dangling
                .iter()
                .map(|(kind, oid)| DanglingJson {
                    kind: kind.as_str().to_owned(),
                    oid: oid.to_hex(),
                })
                .collect(),
            has_warnings: summary.has_warnings,
            visible_issue_groups,
        }
    }
}

fn severity_label(severity: FsckSeverity) -> &'static str {
    match severity {
        FsckSeverity::Error => "error",
        FsckSeverity::Warning => "warning",
        FsckSeverity::Info => "info",
        _ => "unknown",
    }
}

fn serialize_severity<S: Serializer>(severity: &FsckSeverity, ser: S) -> Result<S::Ok, S::Error> {
    ser.serialize_str(severity_label(*severity))
}

pub(crate) fn issue_to_json(diagnostic: FsckDiagnostic<'_>) -> FsckIssueJson {
    let issue = diagnostic.issue;
    FsckIssueJson {
        kind: issue_msg_id(issue).to_owned(),
        severity: diagnostic.severity,
        oid: issue.oid().map(|o| o.to_hex()),
        path: issue.location().map(|p| p.into_owned()),
        detail: issue.detail(),
        visible: diagnostic.visible,
    }
}

impl HumanRender for FsckOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let visible: Vec<_> = self.issues.iter().filter(|issue| issue.visible).collect();

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

        if !self.visible_issue_groups.is_empty() {
            outln!(w, "\nIssues:");
            for (kind, indices) in &self.visible_issue_groups {
                outln!(w, "  {kind} ({}):", indices.len());
                for &idx in indices {
                    let entry = &self.issues[idx];
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
            outln!(w, "\nDangling objects ({}):", self.dangling.len());
            for d in &self.dangling {
                outln!(w, "  {} {}", d.kind, d.oid);
            }
        }

        if !self.ok {
            outln!(w, "\nIntegrity check failed.");
        } else if self.has_warnings {
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
        } else if self.has_warnings {
            outln!(w, "\n**Completed with warnings only.**");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grit_lib::maintenance::repository_fsck::{
        issue_severity, issue_visible_by_default, FsckIssue, MissingObjectReferrer,
    };
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
        let json = issue_to_json(FsckDiagnostic {
            issue: &issue,
            severity: issue_severity(&issue, &config, false),
            visible: issue_visible_by_default(&issue, &config, false),
        });
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
