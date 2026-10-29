//! Next steps for the failures people hit first: no remote, missing credentials, no
//! upstream, unresolved conflicts, a dirty worktree and an unknown branch.
//!
//! The library reports these as typed errors. This module only chooses the JSON `kind`,
//! the human message and a `hint` naming the command to run next.

use std::error::Error as StdError;

use anyhow::Error;
use grit_lib::error::{AuthFailure, Error as LibError, OverwriteReason, RemoteUnreadableReason};
use grit_lib::porcelain::branch::BranchError;
use grit_lib::porcelain::merge::ContinueMergeError;
use grit_lib::porcelain::pull::PullError;
use grit_lib::remote::clone::CloneError;
use grit_lib::remote::github_auth::is_github_https_url;
use grit_lib::remote::push_plan::PushPlanError;
use grit_lib::remote::{RemoteError, DEFAULT_REMOTE};

use crate::error_kind::ErrorKind;
use crate::json_error::{StructuredCliError, StructuredJsonError};
use crate::usage_hints;

/// Paths shown in human messages and hints before the rest are summarized as a count.
const MAX_LISTED_PATHS: usize = 10;

/// A structured error with a hint for the first typed failure in `err`'s chain that has a
/// known next step, or `None` when there is nothing to add.
pub fn guided_error(err: &Error) -> Option<Error> {
    err.chain().find_map(guide_cause)
}

/// [`guided_error`] for a [`RemoteError`] that has not been wrapped in `anyhow` yet.
pub fn guide_remote(err: &RemoteError) -> Option<Error> {
    match err {
        RemoteError::UnknownRemote(name) => Some(remote_not_found(name)),
        RemoteError::NoUrl { remote: Some(name) } => Some(guided(
            ErrorKind::RemoteNoUrl,
            err.to_string(),
            Some(format!("set it with `grit remote set-url {name} <url>`")),
            None,
        )),
        RemoteError::Library(lib) => guide_lib(lib),
        _ => None,
    }
}

/// The hint for an [`LibError::WouldOverwrite`] refusal.
pub fn would_overwrite_hint(reason: OverwriteReason) -> &'static str {
    match reason {
        OverwriteReason::LocalChanges => {
            "commit them with `grit commit`, or set them aside with `grit stash`, then try again"
        }
        OverwriteReason::Untracked => "move or remove those files, then try again",
    }
}

/// `paths` as indented lines (`\n  a\n  b`), capped at [`MAX_LISTED_PATHS`].
pub fn indented_paths(paths: &[String]) -> String {
    let mut out: String = paths
        .iter()
        .take(MAX_LISTED_PATHS)
        .map(|path| format!("\n  {path}"))
        .collect();
    if let Some(rest) = paths.len().checked_sub(MAX_LISTED_PATHS).filter(|n| *n > 0) {
        out.push_str(&format!("\n  … and {rest} more"));
    }
    out
}

fn guide_cause(cause: &(dyn StdError + 'static)) -> Option<Error> {
    if let Some(err) = cause.downcast_ref::<PushPlanError>() {
        return guide_push_plan(err);
    }
    if let Some(err) = cause.downcast_ref::<RemoteError>() {
        return guide_remote(err);
    }
    if let Some(err) = cause.downcast_ref::<PullError>() {
        return guide_pull(err);
    }
    if let Some(err) = cause.downcast_ref::<ContinueMergeError>() {
        return guide_continue_merge(err);
    }
    if let Some(err) = cause.downcast_ref::<CloneError>() {
        return match err {
            CloneError::Library(lib) => guide_lib(lib),
            CloneError::Remote(remote) => guide_remote(remote),
            CloneError::CheckoutFailed { .. } => Some(clone_checkout_failed(err)),
            _ => None,
        };
    }
    cause.downcast_ref::<LibError>().and_then(guide_lib)
}

fn guide_push_plan(err: &PushPlanError) -> Option<Error> {
    match err {
        PushPlanError::NoRemote => Some(guided(
            ErrorKind::NoRemote,
            err.to_string(),
            Some(format!(
                "add one with `grit remote add {DEFAULT_REMOTE} <url>`, or push to a URL with `grit push <url> <branch>`"
            )),
            None,
        )),
        PushPlanError::UnknownRemote(name) => Some(remote_not_found(name)),
        _ => None,
    }
}

fn guide_pull(err: &PullError) -> Option<Error> {
    match err {
        PullError::NoUpstream { branch } => Some(guided(
            ErrorKind::BranchNoUpstream,
            err.to_string(),
            Some(format!(
                "pull a branch by name with `grit pull {DEFAULT_REMOTE} {branch}`, or publish this one and track it with `grit push -u {DEFAULT_REMOTE} {branch}`"
            )),
            None,
        )),
        PullError::Remote(remote) => guide_remote(remote),
        PullError::Repository(lib) => guide_lib(lib),
        _ => None,
    }
}

fn guide_continue_merge(err: &ContinueMergeError) -> Option<Error> {
    match err {
        ContinueMergeError::UnmergedPaths { paths } => Some(guided(
            ErrorKind::UnmergedPaths,
            err.to_string(),
            Some(format!(
                "resolve the conflict markers in {}, then run `grit merge --continue` (or `grit merge --abort` to start over)",
                listed_paths(paths)
            )),
            Some(paths.clone()),
        )),
        ContinueMergeError::ResolvedNotStaged { paths } => Some(guided(
            ErrorKind::UnmergedPaths,
            err.to_string(),
            Some(format!(
                "stage them with `grit add {}`, then run `grit merge --continue`",
                shell_words(paths)
            )),
            Some(paths.clone()),
        )),
        ContinueMergeError::Repository(lib) => guide_lib(lib),
        _ => None,
    }
}

fn guide_lib(err: &LibError) -> Option<Error> {
    match err {
        LibError::Auth { url, reason } => Some(auth_error(err, url, reason)),
        LibError::RemoteUnreadable {
            reason:
                RemoteUnreadableReason::RepositoryNotFound
                | RemoteUnreadableReason::HttpStatus { status: 404 }
                | RemoteUnreadableReason::NotGitServer { .. },
            ..
        } => Some(guided(
            ErrorKind::RemoteUnreadable,
            err.to_string(),
            Some("check the URL, and that you have access to the repository".to_owned()),
            None,
        )),
        LibError::DirtyWorktree { paths, .. } => Some(Error::new(StructuredCliError {
            human: format!("{err}{}", indented_paths(paths)),
            payload: StructuredJsonError {
                hint: Some(would_overwrite_hint(OverwriteReason::LocalChanges).to_owned()),
                paths: Some(paths.clone()),
                ..StructuredJsonError::new(err.to_string(), ErrorKind::DirtyWorktree)
            },
        })),
        LibError::RestorePathUnmerged { paths } => {
            let words = shell_words(paths);
            Some(guided(
                ErrorKind::UnmergedPaths,
                err.to_string(),
                Some(format!(
                    "take one side with `grit restore --ours {words}` or `grit restore --theirs {words}`, bring the conflict markers back with `grit restore --merge {words}`, or edit out the markers and run `grit add {words}`"
                )),
                Some(paths.clone()),
            ))
        }
        LibError::UnmergedConflictMarkers { paths } => Some(Error::new(StructuredCliError {
            human: format!("{err}{}", indented_paths(paths)),
            payload: StructuredJsonError {
                hint: Some(format!(
                    "edit out the conflict markers in {}, then try again",
                    listed_paths(paths)
                )),
                paths: Some(paths.clone()),
                ..StructuredJsonError::new(err.to_string(), ErrorKind::UnmergedPaths)
            },
        })),
        LibError::WorktreePathIo { path, .. } => Some(guided(
            ErrorKind::WorktreePathIo,
            err.to_string(),
            err.is_worktree_file_in_use()
                .then(|| "close programs that may have the file open, then try again".to_owned()),
            Some(vec![path.clone()]),
        )),
        LibError::CheckoutRollbackIncomplete { unrestored, .. } => Some(Error::new(
            StructuredCliError {
                human: format!("{err}{}", indented_paths(unrestored)),
                payload: StructuredJsonError {
                    hint: Some(
                        "HEAD and the index did not move; `grit status` shows the listed paths, and `grit reset --hard` plus `grit clean -f` put them back".to_owned(),
                    ),
                    paths: Some(unrestored.clone()),
                    ..StructuredJsonError::new(err.to_string(), ErrorKind::CheckoutIncomplete)
                },
            },
        )),
        LibError::GitDirPathIo { path, .. } => Some(guided(
            ErrorKind::GitDirPathIo,
            err.to_string(),
            err.is_git_dir_file_in_use()
                .then(|| {
                    "another thread or process is updating the same git metadata; retry shortly"
                        .to_owned()
                })
                .or_else(|| Some(git_dir_path_io_hint(path))),
            Some(vec![path.clone()]),
        )),
        LibError::UnbornHead => Some(
            crate::unborn_errors::unborn_head_from_cwd()
                .unwrap_or_else(|| crate::unborn_errors::unborn_head_error(None)),
        ),
        LibError::Branch(branch_err) => guide_branch(branch_err),
        _ => None,
    }
}

/// A clone kept after its checkout failed: say so, and how to finish the checkout.
fn clone_checkout_failed(err: &CloneError) -> Error {
    let CloneError::CheckoutFailed {
        path,
        commit,
        source,
    } = err
    else {
        return guided(ErrorKind::CloneCheckoutFailed, err.to_string(), None, None);
    };
    let dir = path.display();
    let human = format!("the clone into '{dir}' succeeded, but its checkout failed: {source}");
    let hint = format!(
        "the repository was kept; run `grit reset --hard` in '{dir}' to finish the checkout{}",
        if err.missing_promised_object().is_some() {
            " once its promisor remotes can supply the missing objects"
        } else {
            ""
        }
    );
    Error::new(StructuredCliError {
        payload: StructuredJsonError {
            hint: Some(hint),
            clone_checkout: Some(crate::json_error::CloneCheckoutJson {
                path: path.display().to_string(),
                commit: commit.to_hex(),
                missing_object: err.missing_promised_object().map(|oid| oid.to_hex()),
                failed_remotes: err
                    .failed_promisor_remotes()
                    .iter()
                    .map(|(remote, e)| crate::json_error::PromisorRemoteFailureJson {
                        remote: remote.clone(),
                        error: e.to_string(),
                    })
                    .collect(),
            }),
            ..StructuredJsonError::new(human.clone(), ErrorKind::CloneCheckoutFailed)
        },
        human,
    })
}

fn guide_branch(err: &BranchError) -> Option<Error> {
    match err {
        BranchError::NoPreviousBranch => Some(guided(
            ErrorKind::NoPreviousBranch,
            err.to_string(),
            None,
            None,
        )),
        BranchError::NotFound { name } => {
            if let Some(hint) = usage_hints::checkout_restore_hint(name) {
                Some(guided(
                    ErrorKind::BranchNotFound,
                    format!("no branch named '{name}'"),
                    Some(hint),
                    None,
                ))
            } else {
                let hint = (!looks_like_rev_syntax(name)).then(|| {
                    format!(
                        "`grit branch` lists your branches; create this one with `grit switch -c {name}`"
                    )
                });
                Some(guided(
                    ErrorKind::BranchNotFound,
                    format!("no branch named '{name}'"),
                    hint,
                    None,
                ))
            }
        }
        _ => None,
    }
}

/// True when `name` is revision syntax, not a literal branch name (do not suggest `switch -c`).
fn looks_like_rev_syntax(name: &str) -> bool {
    name.starts_with("@{")
        || name.contains('^')
        || name.contains('~')
        || name.contains(':')
        || name.contains('*')
        || name.contains('?')
        || (name.starts_with('-') && name != "-")
}

fn auth_error(err: &LibError, url: &str, reason: &AuthFailure) -> Error {
    let github = is_github_https_url(url);
    let store = format!("`grit auth {url} --username <user> --token <token>`");
    let (kind, hint) = match reason {
        AuthFailure::Rejected if github => (
            ErrorKind::AuthRejected,
            Some("your saved GitHub token may have expired; sign in again with `grit auth`".to_owned()),
        ),
        AuthFailure::Rejected => (
            ErrorKind::AuthRejected,
            Some(format!(
                "update the credentials your credential helper stores for this host (for example with {store}), then try again"
            )),
        ),
        AuthFailure::UnsupportedScheme { .. } | AuthFailure::UrlNotDescribable => {
            (ErrorKind::AuthRequired, None)
        }
        _ if github => (
            ErrorKind::AuthRequired,
            Some("sign in to GitHub with `grit auth` and try again; if it still fails, check the URL".to_owned()),
        ),
        // Helper entries are commands that can embed secrets, so they are counted, never shown.
        AuthFailure::CredentialsUnavailable { helpers, .. } if !helpers.is_empty() => (
            ErrorKind::AuthRequired,
            Some(format!(
                "{} has no credentials for this host; save them with {store}, or point GIT_ASKPASS at a program that prints them, then try again",
                if helpers.len() == 1 {
                    "your credential helper".to_owned()
                } else {
                    format!("none of your {} credential helpers", helpers.len())
                }
            )),
        ),
        _ => (
            ErrorKind::AuthRequired,
            Some(format!(
                "save a user name and token for this host with {store} (it sets up a credential helper), or point GIT_ASKPASS at a program that prints them, then try again"
            )),
        ),
    };
    guided(kind, err.to_string(), hint, None)
}

fn remote_not_found(name: &str) -> Error {
    guided(
        ErrorKind::RemoteNotFound,
        format!("unknown remote '{name}'"),
        Some(format!(
            "add it with `grit remote add {name} <url>`, or pass a URL or path instead; `grit remote` lists the configured remotes"
        )),
        None,
    )
}

fn git_dir_path_io_hint(path: &str) -> String {
    if path.starts_with(".git/") || path == ".git" {
        "check permissions and ownership under .git (for example `ls -l .git`); \
         after running as root, fix ownership with `chown -R` for your user"
            .to_owned()
    } else {
        "check permissions and ownership in the repository directory (for example `ls -l` in the repo root); \
         after running as root, fix ownership with `chown -R` for your user"
            .to_owned()
    }
}

fn guided(
    kind: ErrorKind,
    human: String,
    hint: Option<String>,
    paths: Option<Vec<String>>,
) -> Error {
    Error::new(StructuredCliError {
        payload: StructuredJsonError {
            hint,
            paths,
            ..StructuredJsonError::new(human.clone(), kind)
        },
        human,
    })
}

/// `a, b and c`, capped at [`MAX_LISTED_PATHS`].
fn listed_paths(paths: &[String]) -> String {
    let shown: Vec<&str> = paths
        .iter()
        .take(MAX_LISTED_PATHS)
        .map(String::as_str)
        .collect();
    let rest = paths.len() - shown.len();
    match (shown.split_last(), rest) {
        (None, _) => String::new(),
        (Some((last, [])), 0) => (*last).to_owned(),
        (Some((last, init)), 0) => format!("{} and {last}", init.join(", ")),
        (Some(_), rest) => format!("{} and {rest} more", shown.join(", ")),
    }
}

/// `paths` as shell words, single-quoting any that need it.
fn shell_words(paths: &[String]) -> String {
    paths
        .iter()
        .map(|path| {
            let plain = !path.is_empty()
                && path
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._-/+@%,=:".contains(c));
            if plain {
                path.clone()
            } else {
                format!("'{}'", path.replace('\'', r"'\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json_error::structured_json_payload;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    fn payload(err: Error) -> StructuredJsonError {
        let guided = guided_error(&err).expect("guided");
        structured_json_payload(&guided).expect("structured")
    }

    #[test]
    fn listed_paths_reads_as_prose() {
        assert_eq!(listed_paths(&strings(&["a"])), "a");
        assert_eq!(listed_paths(&strings(&["a", "b", "c"])), "a, b and c");
        let many: Vec<String> = (0..12).map(|i| format!("f{i}")).collect();
        assert!(listed_paths(&many).ends_with("f9 and 2 more"));
    }

    #[test]
    fn shell_words_quote_only_when_needed() {
        assert_eq!(
            shell_words(&strings(&["src/a.rs", "with space", "it's"])),
            r"src/a.rs 'with space' 'it'\''s'"
        );
    }

    #[test]
    fn indented_paths_caps_the_list() {
        assert_eq!(indented_paths(&strings(&["a", "b"])), "\n  a\n  b");
        let many: Vec<String> = (0..11).map(|i| format!("f{i}")).collect();
        assert!(indented_paths(&many).ends_with("\n  f9\n  … and 1 more"));
    }

    #[test]
    fn push_without_remote_suggests_remote_add() {
        let p = payload(Error::new(PushPlanError::NoRemote));
        assert_eq!(p.kind, ErrorKind::NoRemote);
        assert!(p
            .hint
            .expect("hint")
            .contains("grit remote add origin <url>"));
    }

    #[test]
    fn unknown_and_url_less_remotes_name_the_remote() {
        let p = payload(Error::new(RemoteError::UnknownRemote("up".into())));
        assert_eq!(p.kind, ErrorKind::RemoteNotFound);
        assert!(p.hint.expect("hint").contains("grit remote add up <url>"));
        let p = payload(Error::new(RemoteError::NoUrl {
            remote: Some("half".into()),
        }));
        assert_eq!(p.kind, ErrorKind::RemoteNoUrl);
        assert!(p
            .hint
            .expect("hint")
            .contains("grit remote set-url half <url>"));
        assert!(guided_error(&Error::new(RemoteError::NoUrl { remote: None })).is_none());
    }

    #[test]
    fn worktree_path_io_suggests_closing_programs_only_when_in_use() {
        let in_use = payload(Error::new(LibError::WorktreePathIo {
            path: "a.txt".into(),
            operation: grit_lib::error::WorktreePathOperation::Replace,
            source: std::io::Error::from_raw_os_error(libc::EBUSY),
        }));
        assert_eq!(in_use.kind, ErrorKind::WorktreePathIo);
        assert!(in_use.hint.expect("hint").contains("close programs"));
        let denied = payload(Error::new(LibError::WorktreePathIo {
            path: "a.txt".into(),
            operation: grit_lib::error::WorktreePathOperation::Metadata,
            source: std::io::Error::from_raw_os_error(libc::EACCES),
        }));
        assert_eq!(denied.paths, Some(vec!["a.txt".to_owned()]));
        assert!(denied.hint.is_none());
    }

    #[test]
    fn git_dir_path_io_suggests_retry_when_in_use() {
        let in_use = payload(Error::new(LibError::GitDirPathIo {
            path: ".git/index".into(),
            operation: grit_lib::error::GitDirPathOperation::WriteIndex,
            source: std::io::Error::from_raw_os_error(libc::EBUSY as i32),
        }));
        assert!(in_use.hint.expect("hint").contains("retry shortly"));
    }

    #[test]
    fn git_dir_path_io_hint_matches_layout() {
        let worktree = payload(Error::new(LibError::GitDirPathIo {
            path: ".git/objects/pack/tmp".into(),
            operation: grit_lib::error::GitDirPathOperation::GcPack,
            source: std::io::Error::from_raw_os_error(libc::EACCES as i32),
        }));
        assert!(worktree.hint.expect("hint").contains("ls -l .git"));
        let bare = payload(Error::new(LibError::GitDirPathIo {
            path: "objects/pack/tmp_repack_1".into(),
            operation: grit_lib::error::GitDirPathOperation::GcPack,
            source: std::io::Error::from_raw_os_error(libc::EACCES as i32),
        }));
        let bare_hint = bare.hint.expect("hint");
        assert!(
            !bare_hint.contains("ls -l .git"),
            "bare hint should not reference .git"
        );
        assert!(bare_hint.contains("repo root"));
    }

    #[test]
    fn auth_hints_depend_on_host_and_failure() {
        let auth = |url: &str, reason| {
            payload(Error::new(LibError::Auth {
                url: url.to_owned(),
                reason,
            }))
        };
        let unavailable = || AuthFailure::CredentialsUnavailable {
            detail: "none".into(),
            helpers: Vec::new(),
        };
        let p = auth("https://github.com/o/r.git", unavailable());
        assert_eq!(p.kind, ErrorKind::AuthRequired);
        assert!(p.hint.expect("hint").contains("`grit auth`"));
        assert!(!p.error.contains("info/refs"));
        let p = auth("https://git.example.com/o/r.git", unavailable());
        let hint = p.hint.expect("hint");
        assert!(
            hint.contains(
                "`grit auth https://git.example.com/o/r.git --username <user> --token <token>`"
            ),
            "{hint}"
        );
        assert!(hint.contains("GIT_ASKPASS"), "{hint}");
        let configured = |helpers: &[&str]| {
            auth(
                "https://git.example.com/o/r.git",
                AuthFailure::CredentialsUnavailable {
                    detail: "none".into(),
                    helpers: helpers.iter().map(|h| (*h).to_owned()).collect(),
                },
            )
        };
        let secret = "!echo password=literal-secret";
        let two = configured(&["store", secret]).hint.expect("hint");
        assert!(two.contains("none of your 2 credential helpers"), "{two}");
        assert!(
            !two.contains("literal-secret") && !two.contains("store"),
            "{two}"
        );
        let p = configured(&[secret]);
        assert!(!p.error.contains("literal-secret"), "{}", p.error);
        let hint = p.hint.expect("hint");
        assert!(
            hint.contains("your credential helper has no credentials"),
            "{hint}"
        );
        assert!(!hint.contains("literal-secret"), "{hint}");
        assert!(!hint.contains("configure a credential helper"), "{hint}");
        assert!(hint.contains("--username <user> --token <token>"), "{hint}");
        let p = auth("https://git.example.com/o/r.git", AuthFailure::Rejected);
        assert_eq!(p.kind, ErrorKind::AuthRejected);
        let hint = p.hint.expect("hint");
        assert!(
            hint.contains("update the credentials your credential helper stores"),
            "{hint}"
        );
    }

    #[test]
    fn pull_without_upstream_names_the_branch() {
        let p = payload(Error::new(PullError::NoUpstream {
            branch: "main".into(),
        }));
        assert_eq!(p.kind, ErrorKind::BranchNoUpstream);
        let hint = p.hint.expect("hint");
        assert!(hint.contains("grit pull origin main"));
        assert!(hint.contains("grit push -u origin main"));
    }

    #[test]
    fn unmerged_paths_carry_paths_and_next_commands() {
        let p = payload(Error::new(ContinueMergeError::UnmergedPaths {
            paths: strings(&["f.txt"]),
        }));
        assert_eq!(p.kind, ErrorKind::UnmergedPaths);
        assert_eq!(p.paths, Some(strings(&["f.txt"])));
        assert_eq!(p.error, "you still have unmerged paths: f.txt");
        let hint = p.hint.expect("hint");
        assert!(hint.contains("f.txt") && hint.contains("grit merge --continue"));
        let p = payload(Error::new(ContinueMergeError::ResolvedNotStaged {
            paths: strings(&["a b"]),
        }));
        assert!(p.hint.expect("hint").contains("grit add 'a b'"));
    }

    #[test]
    fn restore_of_unmerged_paths_names_each_side() {
        let p = payload(Error::new(LibError::RestorePathUnmerged {
            paths: strings(&["a b"]),
        }));
        assert_eq!(p.kind, ErrorKind::UnmergedPaths);
        assert_eq!(p.paths, Some(strings(&["a b"])));
        assert_eq!(p.error, "path 'a b' is unmerged");
        let hint = p.hint.expect("hint");
        for side in ["--ours", "--theirs", "--merge"] {
            assert!(
                hint.contains(&format!("grit restore {side} 'a b'")),
                "{hint}"
            );
        }
        assert!(hint.contains("grit add 'a b'"), "{hint}");
    }

    #[test]
    fn conflict_markers_refusal_names_paths() {
        let p = payload(Error::new(LibError::UnmergedConflictMarkers {
            paths: strings(&["f"]),
        }));
        assert_eq!(p.kind, ErrorKind::UnmergedPaths);
        assert_eq!(p.paths, Some(strings(&["f"])));
        assert_eq!(p.error, "unmerged paths still have conflict markers");
        assert!(p.hint.expect("hint").contains("conflict markers in f"));
    }

    #[test]
    fn dirty_worktree_lists_paths_and_suggests_stash() {
        let err = Error::new(LibError::dirty_worktree(
            grit_lib::error::WorktreeOperation::Switch,
            strings(&["b", "a"]),
        ));
        let guided = guided_error(&err).expect("guided");
        assert!(guided.to_string().ends_with("\n  a\n  b"));
        let p = structured_json_payload(&guided).expect("structured");
        assert_eq!(p.kind, ErrorKind::DirtyWorktree);
        assert_eq!(p.paths, Some(strings(&["a", "b"])));
        assert!(p.hint.expect("hint").contains("grit stash"));
    }

    #[test]
    fn errors_inside_wrappers_are_found() {
        let wrapped = Error::new(CloneError::Remote(RemoteError::Library(
            LibError::remote_unreadable(
                "https://example.com/x.git",
                RemoteUnreadableReason::HttpStatus { status: 404 },
            ),
        )))
        .context("clone failed");
        let p = payload(wrapped);
        assert_eq!(p.kind, ErrorKind::RemoteUnreadable);
        assert!(p.hint.expect("hint").contains("check the URL"));
        let unreachable = Error::new(LibError::remote_unreadable(
            "https://example.com/x.git",
            RemoteUnreadableReason::Transport {
                detail: "refused".into(),
            },
        ));
        assert!(guided_error(&unreachable).is_none());
    }
}
