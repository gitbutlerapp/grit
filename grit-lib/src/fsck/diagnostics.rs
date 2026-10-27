//! Classification and visibility policy for repository integrity findings.

use super::{
    issue_severity, issue_visible_at_severity, FsckIssue, FsckOptions, FsckReport, FsckSeverity,
};
use crate::config::ConfigSet;

/// A repository finding with its severity and listing visibility resolved.
#[derive(Debug, Clone, Copy)]
pub struct FsckDiagnostic<'a> {
    /// Original finding, borrowed from the report.
    pub issue: &'a FsckIssue,
    /// Severity after applying configuration and strictness.
    pub severity: FsckSeverity,
    /// Whether to include this finding in the requested listing.
    ///
    /// All warnings and errors are visible. Most informational findings are
    /// hidden unless the summary was requested with `include_info` enabled;
    /// `missingTaggerEntry` remains visible even without that option.
    pub visible: bool,
}

/// Classified findings and aggregate integrity status.
///
/// Every finding is retained in discovery order, including hidden informational
/// findings. The summary borrows the report and does not clone finding details.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct FsckSummary<'a> {
    /// All findings with severity and visibility resolved once.
    pub diagnostics: Vec<FsckDiagnostic<'a>>,
    /// True when no finding has error severity, independently of visibility.
    pub ok: bool,
    /// True when at least one finding has warning severity.
    ///
    /// This may be true even when `ok` is false. Informational findings,
    /// including visible ones, do not count as warnings.
    pub has_warnings: bool,
}

impl FsckReport {
    /// Resolve diagnostic policy and aggregate status for this report.
    ///
    /// `config` supplies `fsck.<msg-id>` overrides and `options.strict` controls
    /// warning promotion. `include_info` includes all informational findings in
    /// the listing, without changing their severity or the integrity status.
    /// Findings already suppressed during fsck are not recovered here.
    ///
    /// Returns all findings with resolved visibility, whether the check passed,
    /// and whether warnings occurred. This only classifies existing findings;
    /// it performs no repository I/O and cannot fail.
    #[must_use]
    pub fn summary(
        &self,
        config: &ConfigSet,
        options: &FsckOptions,
        include_info: bool,
    ) -> FsckSummary<'_> {
        let mut ok = true;
        let mut has_warnings = false;
        let diagnostics = self
            .issues
            .iter()
            .map(|issue| {
                let severity = issue_severity(issue, config, options.strict);
                ok &= severity != FsckSeverity::Error;
                has_warnings |= severity == FsckSeverity::Warning;
                FsckDiagnostic {
                    issue,
                    severity,
                    visible: include_info || issue_visible_at_severity(issue, severity),
                }
            })
            .collect();
        FsckSummary {
            diagnostics,
            ok,
            has_warnings,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::fsck_standalone::FsckError;
    use crate::objects::ObjectId;
    use crate::refs_fsck::{RefsFsckIssue, RefsFsckSeverity};

    fn report(ids: &[&'static str]) -> FsckReport {
        FsckReport {
            objects_checked: 7,
            issues: ids
                .iter()
                .map(|id| FsckIssue::BadObject {
                    oid: ObjectId::zero(),
                    error: FsckError::new(id, "test finding"),
                })
                .collect(),
            dangling: Vec::new(),
        }
    }

    #[test]
    fn empty_summary_is_clean() {
        let report = report(&[]);
        let summary = report.summary(&ConfigSet::new(), &FsckOptions::default(), false);
        assert!(summary.ok);
        assert!(!summary.has_warnings);
        assert!(summary.diagnostics.is_empty());
    }

    #[test]
    fn summary_retains_order_and_borrows_findings() {
        let report = report(&["badFilemode", "missingTaggerEntry", "hasDot", "badTree"]);
        let config = ConfigSet::new();
        let options = FsckOptions::default();
        let summary = report.summary(&config, &options, false);
        assert!(!summary.ok);
        assert!(summary.has_warnings);
        assert_eq!(summary.diagnostics.len(), report.issues.len());
        let classifications: Vec<_> = summary
            .diagnostics
            .iter()
            .zip(&report.issues)
            .map(|(diagnostic, issue)| {
                assert!(std::ptr::eq(diagnostic.issue, issue));
                (diagnostic.severity, diagnostic.visible)
            })
            .collect();
        assert_eq!(
            classifications,
            vec![
                (FsckSeverity::Info, false),
                (FsckSeverity::Info, true),
                (FsckSeverity::Warning, true),
                (FsckSeverity::Error, true),
            ]
        );
        assert_eq!(summary.ok, report.ok(&config, &options));
    }

    #[test]
    fn including_info_changes_only_visibility() {
        let report = report(&["badFilemode", "missingTaggerEntry"]);
        let config = ConfigSet::new();
        let options = FsckOptions::full_repository(true);
        for include_info in [false, true] {
            let summary = report.summary(&config, &options, include_info);
            assert!(summary.ok);
            assert!(!summary.has_warnings);
            assert_eq!(summary.diagnostics[0].visible, include_info);
            assert!(summary.diagnostics[1].visible);
            assert!(summary
                .diagnostics
                .iter()
                .all(|d| d.severity == FsckSeverity::Info));
        }
    }

    #[test]
    fn summary_resolves_strictness_and_configuration() {
        let mut report = report(&["hasDot", "badFilemode"]);
        report.issues.push(FsckIssue::BadRef(RefsFsckIssue {
            severity: RefsFsckSeverity::Warning,
            path: "refs/heads/demo".into(),
            msg_id: "badRefName",
            detail: "test ref finding".into(),
        }));
        let mut config = ConfigSet::new();
        let ordinary = FsckOptions::full_repository(false);
        let strict = FsckOptions::full_repository(true);
        let summary = report.summary(&config, &ordinary, false);
        assert!(summary.ok);
        assert!(summary.has_warnings);
        let summary = report.summary(&config, &strict, false);
        assert!(!summary.ok);
        assert!(!summary.has_warnings);
        assert_eq!(summary.diagnostics[0].severity, FsckSeverity::Error);
        assert_eq!(summary.diagnostics[2].severity, FsckSeverity::Error);

        config.add_command_override("fsck.hasDot", "warn").unwrap();
        config
            .add_command_override("fsck.badFilemode", "error")
            .unwrap();
        let summary = report.summary(&config, &strict, false);
        assert!(!summary.ok);
        assert!(summary.has_warnings);
        assert_eq!(summary.diagnostics[0].severity, FsckSeverity::Warning);
        assert_eq!(summary.diagnostics[1].severity, FsckSeverity::Error);
        assert!(summary.diagnostics.iter().all(|d| d.visible));
        assert_eq!(summary.ok, report.ok(&config, &strict));
    }
}
