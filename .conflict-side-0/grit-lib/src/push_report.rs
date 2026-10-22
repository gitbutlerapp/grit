//! Push status reporting that matches Git's output.
//!
//! After a push, Git prints one line per reference describing how the update
//! resolved (`[up to date]`, `[new branch]`, `[deleted]`, `[rejected]`, …).
//! There are two output styles: a human-readable form on stderr and a
//! machine-readable `--porcelain` form on stdout. This module reproduces both
//! exactly, including the ordering and the fixed-width summary column.
//!
//! The canonical C implementation lives in `transport.c`
//! (`print_ref_status`, `print_ok_ref_status`, `print_one_push_report`,
//! `transport_print_push_status`).

use crate::objects::ObjectId;
use std::fmt::Write as _;

/// The resolved outcome of a single reference update during a push.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PushRefStatus {
    /// The reference was already at the requested value (`=`, `[up to date]`).
    UpToDate,
    /// A successful update (`*` new, `-` delete, ` ` fast-forward, `+` forced).
    Ok,
    /// Client-side rejection: the update is not a fast-forward and `--force`
    /// was not given (`!`, `[rejected] (non-fast-forward)`).
    RejectNonFastForward,
    /// Rejected because the new ref already exists (`!`, `[rejected] (already exists)`).
    RejectAlreadyExists,
    /// Rejected because the remote has the ref but we need to fetch first.
    RejectFetchFirst,
    /// Rejected because a forced update is required.
    RejectNeedsForce,
    /// Rejected because force-with-lease found stale info.
    RejectStale,
    /// The remote `receive-pack` declined the update (`!`, `[remote rejected] (<reason>)`).
    RemoteRejected,
    /// Part of an atomic push that failed because another ref was rejected
    /// (`!`, `[rejected] (atomic push failed)`).
    AtomicPushFailed,
}

impl PushRefStatus {
    /// Whether this status represents a hard failure (causes a non-zero exit).
    #[must_use]
    pub fn is_error(&self) -> bool {
        !matches!(self, PushRefStatus::UpToDate | PushRefStatus::Ok)
    }
}

/// One reference's resolved push result, ready for display.
#[derive(Clone, Debug)]
pub struct PushRefResult {
    /// The local ref name (source side), e.g. `refs/heads/next`. `None` for deletions.
    pub local_ref: Option<String>,
    /// The remote ref name (destination side), e.g. `refs/heads/next`.
    pub remote_ref: String,
    /// Old value of the remote ref (`None`/zero for a new ref).
    pub old_oid: Option<ObjectId>,
    /// New value of the remote ref (`None`/zero for a deletion).
    pub new_oid: Option<ObjectId>,
    /// Whether this update was a forced (non-fast-forward but `--force`d) update.
    pub forced: bool,
    /// Whether this update deletes the remote ref.
    pub deletion: bool,
    /// The resolved status.
    pub status: PushRefStatus,
    /// Extra reason text (used for `[remote rejected]`).
    pub message: Option<String>,
}

impl PushRefResult {
    /// Abbreviated 7-char hex of an OID, or seven zeros for `None`.
    fn short(oid: Option<ObjectId>) -> String {
        match oid {
            Some(o) => o.to_hex()[..7].to_owned(),
            None => "0000000".to_owned(),
        }
    }
}

/// Strip the common `refs/heads/`, `refs/tags/`, `refs/remotes/` prefix the way
/// Git's `prettify_refname` does for human-readable output.
fn prettify_refname(name: &str) -> &str {
    name.strip_prefix("refs/heads/")
        .or_else(|| name.strip_prefix("refs/tags/"))
        .or_else(|| name.strip_prefix("refs/remotes/"))
        .unwrap_or(name)
}

/// The flag character, fixed summary text, and optional parenthetical reason for
/// a result, matching `print_one_push_report` / `print_ok_ref_status`.
///
/// Returns `(flag, summary, message)` where `summary` is the bracketed status or
/// the `old..new` quickref, and `message` is the trailing `(reason)` if any.
fn describe(result: &PushRefResult) -> (char, String, Option<String>) {
    match result.status {
        PushRefStatus::UpToDate => ('=', "[up to date]".to_owned(), None),
        PushRefStatus::Ok => {
            if result.deletion {
                ('-', "[deleted]".to_owned(), None)
            } else if result.old_oid.is_none() {
                let summary = if result.remote_ref.starts_with("refs/tags/") {
                    "[new tag]"
                } else if result.remote_ref.starts_with("refs/heads/") {
                    "[new branch]"
                } else {
                    "[new reference]"
                };
                ('*', summary.to_owned(), None)
            } else {
                let old = PushRefResult::short(result.old_oid);
                let new = PushRefResult::short(result.new_oid);
                if result.forced {
                    (
                        '+',
                        format!("{old}...{new}"),
                        Some("forced update".to_owned()),
                    )
                } else {
                    (' ', format!("{old}..{new}"), None)
                }
            }
        }
        PushRefStatus::RejectNonFastForward => (
            '!',
            "[rejected]".to_owned(),
            Some("non-fast-forward".to_owned()),
        ),
        PushRefStatus::RejectAlreadyExists => (
            '!',
            "[rejected]".to_owned(),
            Some("already exists".to_owned()),
        ),
        PushRefStatus::RejectFetchFirst => {
            ('!', "[rejected]".to_owned(), Some("fetch first".to_owned()))
        }
        PushRefStatus::RejectNeedsForce => {
            ('!', "[rejected]".to_owned(), Some("needs force".to_owned()))
        }
        PushRefStatus::RejectStale => ('!', "[rejected]".to_owned(), Some("stale info".to_owned())),
        PushRefStatus::RemoteRejected => (
            '!',
            "[remote rejected]".to_owned(),
            result
                .message
                .clone()
                .or_else(|| Some("remote rejected".to_owned())),
        ),
        PushRefStatus::AtomicPushFailed => (
            '!',
            "[rejected]".to_owned(),
            Some("atomic push failed".to_owned()),
        ),
    }
}

/// Output produced by [`format_push_status`].
#[derive(Default, Debug)]
pub struct PushStatusOutput {
    /// Lines for stdout (porcelain mode writes everything here).
    pub stdout: String,
    /// Lines for stderr (human-readable mode writes everything here).
    pub stderr: String,
    /// Whether any reference failed (the push command should exit non-zero).
    pub had_errors: bool,
}

/// Sort key matching Git's push-status output order: up-to-date refs first,
/// then successful updates, then everything else (errors), preserving the
/// original order within each bucket.
fn status_bucket(status: &PushRefStatus) -> u8 {
    match status {
        PushRefStatus::UpToDate => 0,
        PushRefStatus::Ok => 1,
        _ => 2,
    }
}

/// Render the full set of per-ref push results the way Git does.
///
/// `dest` is the (already credential-scrubbed) destination URL printed in the
/// `To <url>` header. When `porcelain` is true the machine-readable format is
/// emitted to `stdout` and terminated with a `Done` line; otherwise the
/// human-readable format is emitted to `stderr`. `quiet` suppresses all output
/// unless there were errors (matching `if (!quiet || err)` in `transport_push`).
///
/// Results are reordered into Git's display order but the input slice is left
/// untouched.
#[must_use]
pub fn format_push_status(
    dest: &str,
    results: &[PushRefResult],
    porcelain: bool,
    quiet: bool,
) -> PushStatusOutput {
    let mut out = PushStatusOutput {
        had_errors: results.iter().any(|r| r.status.is_error()),
        ..PushStatusOutput::default()
    };

    if quiet && !out.had_errors {
        return out;
    }

    // Sort into Git's three buckets (up-to-date, ok, errors); within a bucket the
    // remote `refs` list is advertised in sorted order, so order by ref name.
    let mut order: Vec<usize> = (0..results.len()).collect();
    order.sort_by(|&a, &b| {
        status_bucket(&results[a].status)
            .cmp(&status_bucket(&results[b].status))
            .then_with(|| results[a].remote_ref.cmp(&results[b].remote_ref))
    });

    // Compute the fixed summary-column width for human-readable output:
    // 2 * max_abbrev + 3, where abbrev is 7 here (DEFAULT_ABBREV).
    let summary_width = 2 * 7 + 3;

    let buf = if porcelain {
        &mut out.stdout
    } else {
        &mut out.stderr
    };

    let _ = writeln!(buf, "To {dest}");

    for &i in &order {
        let result = &results[i];
        let (flag, summary, message) = describe(result);

        if porcelain {
            let to_name = &result.remote_ref;
            // Git prints the source side from `ref->peer_ref`. A *successful*
            // deletion is reported via `print_ok_ref_status` with `from = NULL`
            // (just `:dst`). Most error paths pass `ref->peer_ref` for deletions
            // (whose name is the literal `(delete)`), except `REF_STATUS_REMOTE_REJECT`,
            // which explicitly passes `NULL` for a deletion (`ref->deletion ? NULL : …`).
            if result.deletion {
                let from_delete = result.status.is_error()
                    && !matches!(result.status, PushRefStatus::RemoteRejected);
                if from_delete {
                    let _ = write!(buf, "{flag}\t(delete):{to_name}\t");
                } else {
                    let _ = write!(buf, "{flag}\t:{to_name}\t");
                }
            } else if let Some(from) = &result.local_ref {
                let _ = write!(buf, "{flag}\t{from}:{to_name}\t");
            } else {
                let _ = write!(buf, "{flag}\t:{to_name}\t");
            }
            match &message {
                Some(msg) => {
                    let _ = writeln!(buf, "{summary} ({msg})");
                }
                None => {
                    let _ = writeln!(buf, "{summary}");
                }
            }
        } else {
            let _ = write!(buf, " {flag} {summary:<summary_width$} ");
            match &result.local_ref {
                Some(from) if !result.deletion => {
                    let _ = write!(
                        buf,
                        "{} -> {}",
                        prettify_refname(from),
                        prettify_refname(&result.remote_ref)
                    );
                }
                _ => {
                    let _ = write!(buf, "{}", prettify_refname(&result.remote_ref));
                }
            }
            if let Some(msg) = &message {
                let _ = write!(buf, " ({msg})");
            }
            let _ = writeln!(buf);
        }
    }

    if porcelain {
        let _ = writeln!(buf, "Done");
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(byte: u8) -> ObjectId {
        ObjectId::from_bytes(&[byte; 20]).expect("valid 20-byte oid")
    }

    fn results() -> Vec<PushRefResult> {
        vec![
            PushRefResult {
                local_ref: Some("refs/heads/main".to_owned()),
                remote_ref: "refs/heads/main".to_owned(),
                old_oid: Some(oid(0xbb)),
                new_oid: Some(oid(0xaa)),
                forced: false,
                deletion: false,
                status: PushRefStatus::RejectNonFastForward,
                message: None,
            },
            PushRefResult {
                local_ref: None,
                remote_ref: "refs/heads/foo".to_owned(),
                old_oid: Some(oid(0xaa)),
                new_oid: None,
                forced: false,
                deletion: true,
                status: PushRefStatus::Ok,
                message: None,
            },
            PushRefResult {
                local_ref: Some("refs/heads/baz".to_owned()),
                remote_ref: "refs/heads/baz".to_owned(),
                old_oid: Some(oid(0xaa)),
                new_oid: Some(oid(0xaa)),
                forced: false,
                deletion: false,
                status: PushRefStatus::UpToDate,
                message: None,
            },
            PushRefResult {
                local_ref: Some("refs/heads/next".to_owned()),
                remote_ref: "refs/heads/next".to_owned(),
                old_oid: None,
                new_oid: Some(oid(0xaa)),
                forced: false,
                deletion: false,
                status: PushRefStatus::Ok,
                message: None,
            },
        ]
    }

    #[test]
    fn porcelain_orders_and_formats() {
        let out = format_push_status("URL", &results(), true, false);
        let expected = "To URL\n\
            =\trefs/heads/baz:refs/heads/baz\t[up to date]\n\
            -\t:refs/heads/foo\t[deleted]\n\
            *\trefs/heads/next:refs/heads/next\t[new branch]\n\
            !\trefs/heads/main:refs/heads/main\t[rejected] (non-fast-forward)\n\
            Done\n";
        assert_eq!(out.stdout, expected);
        assert!(out.had_errors);
        assert!(out.stderr.is_empty());
    }

    #[test]
    fn quiet_suppresses_when_no_errors() {
        let mut rs = results();
        // Make main up-to-date so there are no errors.
        rs[0].status = PushRefStatus::UpToDate;
        let out = format_push_status("URL", &rs, true, true);
        assert!(out.stdout.is_empty());
        assert!(!out.had_errors);
    }

    #[test]
    fn atomic_failure_message() {
        let mut rs = results();
        rs[1].status = PushRefStatus::AtomicPushFailed;
        let out = format_push_status("URL", &rs, true, false);
        // A rejected deletion (not REMOTE_REJECT) prints `(delete)` as its source,
        // matching Git's `print_ref_status(... ref->peer_ref ...)`.
        assert!(out
            .stdout
            .contains("!\t(delete):refs/heads/foo\t[rejected] (atomic push failed)"));
    }

    #[test]
    fn remote_rejected_deletion_omits_delete_source() {
        let mut rs = results();
        rs[1].status = PushRefStatus::RemoteRejected;
        rs[1].message = Some("pre-receive hook declined".to_owned());
        let out = format_push_status("URL", &rs, true, false);
        // REMOTE_REJECT explicitly passes NULL for a deletion: just `:dst`.
        assert!(out
            .stdout
            .contains("!\t:refs/heads/foo\t[remote rejected] (pre-receive hook declined)"));
    }
}
