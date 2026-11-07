//! Pinning tests for the error contract (task 7099 step 1; step 7110 repository area).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::assertions_on_constants
)]

use std::collections::HashSet;

use std::path::PathBuf;

use grit_lib::error::{Error, LockHeldError, NotARepositoryReason, RepositoryError};
use grit_lib::index::IndexError;
use grit_lib::porcelain::CommitError;
use grit_lib::remote::RemoteError;

/// Slugs emitted by [`Error::kind_name`], in source order, with [`Index`], [`RepositoryError`],
/// and [`RemoteError`] delegated kinds expanded (duplicates from delegation appear once at their
/// first occurrence).
///
/// [`Index`]: grit_lib::index::IndexError
/// [`RepositoryError`]: grit_lib::error::RepositoryError
/// [`RemoteError`]: grit_lib::remote::RemoteError
const EXPECTED_KIND_NAMES: &[&str] = &[
    "not_a_repository",
    "forbidden_bare_repository",
    "dubious_ownership",
    "unsupported_repository_format_version",
    "invalid_ref_storage_format",
    "init_object_format_change",
    "init_bare_with_separate_git_dir",
    "requires_work_tree",
    "message",
    "git_dir_path_io",
    "worktree_path_io",
    "unsupported_object_store",
    "precious_objects_repository",
    "odb_objects_dir_mismatch",
    "object_format_mismatch",
    "invalid_object_id",
    "hash_collision",
    "object_not_found",
    "revision_not_found",
    "promised_object_missing",
    "promisor_fetch_failed",
    "corrupt_object",
    "pack_index_checksum_mismatch",
    "unresolved_deltas",
    "pack_delta_base_not_written",
    "invalid_object",
    "invalid_tree_entry",
    "invalid_signature",
    "unexpected_object_kind",
    "commit_graph_parent_missing",
    "delta_chain_too_deep",
    "unknown_object_type",
    "object_header_too_long",
    "io",
    "lock_held",
    "zlib",
    "corrupt_loose_object",
    "corrupt_packed_object",
    "loose_hash_mismatch",
    "loose_object_write",
    "object_stream_truncated",
    "worktree_file_changed",
    "index_error",
    "index_unmerged",
    "index_path_prefix_conflict",
    "index_fsmonitor_extension_truncated",
    "index_fsmonitor_extension_bad_version",
    "index_fsmonitor_extension_ewah_invalid",
    "cache_tree_corrupt",
    "maintenance_running",
    "unmerged_conflict_markers",
    "invalid_ref",
    "packed_refs_unexpected_line",
    "path_error",
    "pathspec_from_file",
    "invalid_path",
    "invalid_path_component",
    "config",
    "submodule",
    "branch",
    "empty_commit_message",
    "commit_paths_required",
    "partial_commit_during_merge",
    "partial_commit_during_replay",
    "nothing_to_commit",
    "nothing_staged_to_commit",
    "amend_unborn",
    "invalid_identity",
    "identity",
    "signing",
    "hunk_selection",
    "apply",
    "not_fast_forward",
    "unrelated_histories",
    "merge_commit",
    "replay_source_at_head",
    "sequencer_unknown_command",
    "replay_continue_unmerged",
    "replay_continue_dirty_index",
    "replay_skip_already_committed",
    "no_replay_in_progress",
    "no_merge_in_progress",
    "operation_in_progress",
    "ref_lock",
    "ref_store",
    "reftable",
    "filter",
    "path_outside_repo",
    "remote_no_url",
    "unsupported_scheme",
    "invalid_url",
    "http_unavailable",
    "unknown_remote",
    "remote_exists",
    "invalid_remote_name",
    "auth",
    "credentials_unavailable",
    "push_atomic_unsupported",
    "push_options_unsupported",
    "fetch_unshallow_not_shallow",
    "fetch_shallow_unsupported",
    "fetch_deepen_relative_unsupported",
    "received_pack_temp_missing",
    "remote_unreadable",
    "remote_fatal",
    "local_upload_pack",
    "local_upload_pack_panicked",
    "resumable",
    "fetch_refspec",
    "fetch_into_checked_out_branch",
    "cancelled",
    "protocol",
    "push_plan",
    "partial_clone",
    "malformed_transport_url",
    "transport",
    "http_status",
    "ssh",
    "http",
    "http_worker_panicked",
    "http_low_speed_stall",
    "credential",
    "invalid_attr_pathspec",
    "pathspec_no_match",
    "restore_conflict_with_staged",
    "restore_conflict_with_source",
    "restore_conflict_missing_theirs",
    "restore_conflict_missing_ours",
    "restore_conflict_not_unmerged",
    "restore_path_unmerged",
    "pathspec_beyond_symlink",
    "embedded_repo_without_commit",
    "pathspec_ignored",
    "remove_blocked",
    "not_removing_recursively",
    "remove_paths_required",
    "move_source_not_tracked",
    "move_destination_exists",
    "move_destination_not_directory",
    "move_into_subdirectory",
    "move_into_self",
    "move_nothing_to_move",
    "move_duplicate_destination",
    "move_bad_source",
    "move_missing_index_entry",
    "would_overwrite",
    "dirty_worktree",
    "checkout_rollback_incomplete",
    "detached_head",
    "unborn_head",
    "invalid_head",
    "branch_checked_out_in_worktree",
    "branch_not_found",
    "branch_list_names_date_sort",
    "tag",
    "worktree",
    "reflog_unknown_ref",
    "reflog_invalid_selector",
    "reflog_invalid_index",
    "reflog_index_out_of_range",
    "blame_worktree_bare_repository",
    "note_already_exists",
    "invalid_notes_ref",
    "notes_rewrite_mode",
    "notes_ref_not_commit",
    "notes_object_kind",
    "notes_merge_both_empty",
    "notes_merge_in_progress",
    "midx",
    "offload",
    "rev_parse",
    "rev_list",
    "hook",
    "history",
    "no_promisor_remote",
    "tree_sha256_invalid_hex",
    "tree_sha256_invalid_revision",
    "tree_sha256_missing_submodules",
    "tree_sha256_requires_signing",
    "tag_verify_not_tag",
    "tag_verify_missing_object_header",
    "tag_verify_duplicate_tree_sha256_header",
    "tree_sha256_claim",
    "blame_ignore_rev_unknown",
    "blame_ignore_revs_file_open",
    "blame_ignore_revs_file_invalid_rev",
];

/// Size of [`Error`] when the error-contract step landed (area enums must stay ≤ this until step 12).
const ERROR_SIZE_CEILING: usize = 112;

fn extract_kind_name_match_body<'a>(src: &'a str, impl_anchor: &str) -> &'a str {
    let idx = src
        .find(impl_anchor)
        .unwrap_or_else(|| panic!("{impl_anchor} not found"));
    let sub = &src[idx..];
    let fn_start = sub
        .find("pub fn kind_name")
        .unwrap_or_else(|| panic!("pub fn kind_name not found after {impl_anchor}"));
    let sub = &sub[fn_start..];
    let match_kw = "match self {";
    let m_start = sub
        .find(match_kw)
        .unwrap_or_else(|| panic!("{match_kw} not found in kind_name"));
    let after_open = m_start + match_kw.len();
    let mut brace = 1usize;
    let mut i = after_open;
    while i < sub.len() && brace > 0 {
        match sub.as_bytes()[i] {
            b'{' => brace += 1,
            b'}' => brace -= 1,
            _ => {}
        }
        i += 1;
    }
    assert_eq!(brace, 0, "unbalanced braces in kind_name match");
    &sub[after_open..i - 1]
}

fn literal_slugs_from_kind_match(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.contains("=>") && trimmed.contains("kind_name()") {
            continue;
        }
        let value = if let Some(pos) = line.find("=> \"") {
            &line[pos + 4..]
        } else if trimmed.starts_with('"') {
            trimmed
        } else {
            continue;
        };
        let value = value.trim_start_matches('"');
        let Some(end) = value.find('"') else {
            continue;
        };
        out.push(value[..end].to_owned());
    }
    out
}

fn expanded_kind_name_slugs() -> Vec<String> {
    let umbrella =
        extract_kind_name_match_body(include_str!("../src/error/mod.rs"), "impl Error {");
    let index_slugs = literal_slugs_from_kind_match(extract_kind_name_match_body(
        include_str!("../src/index/error.rs"),
        "impl IndexError {",
    ));
    let commit_slugs = literal_slugs_from_kind_match(extract_kind_name_match_body(
        include_str!("../src/porcelain/commit_error.rs"),
        "impl CommitError {",
    ));
    let merge_slugs = literal_slugs_from_kind_match(extract_kind_name_match_body(
        include_str!("../src/porcelain/merge_error.rs"),
        "impl MergeError {",
    ));
    let repository_slugs = literal_slugs_from_kind_match(extract_kind_name_match_body(
        include_str!("../src/repo/repository_error.rs"),
        "impl RepositoryError {",
    ));
    let remote_slugs = literal_slugs_from_kind_match(extract_kind_name_match_body(
        include_str!("../src/remote/error.rs"),
        "impl RemoteError {",
    ));

    let mut expanded: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for line in umbrella.lines() {
        let trimmed = line.trim();
        if !trimmed.contains("=>") {
            continue;
        }
        if trimmed.contains("Self::Repository(") && trimmed.contains("kind_name()") {
            for s in &repository_slugs {
                if seen.insert(s.clone()) {
                    expanded.push(s.clone());
                }
            }
            continue;
        }
        if trimmed.contains("Self::Remote(") && trimmed.contains("kind_name()") {
            for s in &remote_slugs {
                if seen.insert(s.clone()) {
                    expanded.push(s.clone());
                }
            }
            continue;
        }
        if trimmed.contains("Self::Index(") && trimmed.contains("kind_name()") {
            for s in &index_slugs {
                if seen.insert(s.clone()) {
                    expanded.push(s.clone());
                }
            }
            continue;
        }
        if trimmed.contains("Self::Commit(") && trimmed.contains("kind_name()") {
            for s in &commit_slugs {
                if seen.insert(s.clone()) {
                    expanded.push(s.clone());
                }
            }
            continue;
        }
        if trimmed.contains("Self::Merge(") && trimmed.contains("kind_name()") {
            for s in &merge_slugs {
                if seen.insert(s.clone()) {
                    expanded.push(s.clone());
                }
            }
            continue;
        }
        let value = if let Some(pos) = line.find("=> \"") {
            &line[pos + 4..]
        } else if trimmed.starts_with('"') {
            trimmed
        } else {
            continue;
        };
        let value = value.trim_start_matches('"');
        let Some(end) = value.find('"') else {
            continue;
        };
        let slug = value[..end].to_owned();
        if seen.insert(slug.clone()) {
            expanded.push(slug);
        }
    }
    expanded
}

#[test]
fn error_kind_names_are_stable() {
    let parsed = expanded_kind_name_slugs();
    assert_eq!(
        parsed.len(),
        EXPECTED_KIND_NAMES.len(),
        "update EXPECTED_KIND_NAMES when kind_name arms or delegated kind_name tables change"
    );
    for (idx, (live, pinned)) in parsed.iter().zip(EXPECTED_KIND_NAMES.iter()).enumerate() {
        assert_eq!(live, pinned, "kind slug mismatch at index {idx}");
    }
}

#[test]
fn verbatim_kind_slug_is_legacy_message() {
    assert_eq!(
        Error::Verbatim("x".into()).kind_name(),
        "message",
        "serialized kind slug must stay `message` for this variant"
    );
}

#[test]
fn error_area_matches_umbrella_branch() {
    assert_eq!(
        Error::Repository(RepositoryError::NotARepository(
            NotARepositoryReason::NoRepositoryAt {
                path: "/tmp/nope".into(),
            },
        ))
        .area(),
        "repository"
    );
    assert_eq!(Error::Verbatim("e".into()).area(), "verbatim");
    assert_eq!(Error::Commit(CommitError::NothingToCommit).area(), "merge");
    assert_eq!(Error::Remote(RemoteError::Cancelled).area(), "remote");
}

#[test]
fn umbrella_area_delegates_from_wrapped_area_enums() {
    let repo_err = RepositoryError::ForbiddenBareRepository {
        path: "/repo".into(),
    };
    assert_eq!(
        Error::Repository(RepositoryError::ForbiddenBareRepository {
            path: "/repo".into(),
        })
        .area(),
        repo_err.area()
    );

    assert_eq!(
        Error::Remote(RemoteError::Nested(Box::new(Error::Verbatim("x".into())))).area(),
        "verbatim",
        "Nested(remote) must use the wrapped Error::area"
    );

    assert_eq!(
        Error::Index(IndexError::LockHeld(LockHeldError {
            path: PathBuf::from("index.lock"),
            holder: None,
        }))
        .area(),
        "lock",
        "IndexError::LockHeld area matches Error::LockHeld"
    );
}

#[test]
fn path_io_preserves_raw_os_error_identity() {
    use grit_lib::error::{Error, GitDirPathOperation};

    let source = std::io::Error::from_raw_os_error(libc::EBUSY as i32);
    let err = Error::Repository(RepositoryError::git_dir_path_io(
        ".git/index",
        GitDirPathOperation::WriteIndex,
        source,
    ));
    assert!(err.is_git_dir_file_in_use());
    let leaf = err.path_io().expect("path_io");
    let roundtrip: Error = leaf.into();
    assert!(
        roundtrip.is_git_dir_file_in_use(),
        "PathIoError round-trip must keep raw OS error for contention checks"
    );
}

#[test]
fn error_type_sizes() {
    let size = std::mem::size_of::<Error>();
    assert_eq!(
        size, ERROR_SIZE_CEILING,
        "update ERROR_SIZE_CEILING in error_contract.rs if the umbrella Error layout changed intentionally"
    );
}
