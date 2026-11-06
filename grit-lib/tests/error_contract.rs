//! Pinning tests for the error contract (task 7099 step 1).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::assertions_on_constants
)]

use grit_lib::error::Error;

const EXPECTED_KIND_NAMES: &[&str] = &[
    "not_a_repository",
    "forbidden_bare_repository",
    "dubious_ownership",
    "unsupported_repository_format_version",
    "invalid_ref_storage_format",
    "init_object_format_change",
    "init_bare_with_separate_git_dir",
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
    "worktree_path_io",
    "git_dir_path_io",
    "object_stream_truncated",
    "worktree_file_changed",
    "index_error",
    "index_unmerged",
    "maintenance_running",
    "unmerged_conflict_markers",
    "index_path_prefix_conflict",
    "index_fsmonitor_extension_truncated",
    "index_fsmonitor_extension_bad_version",
    "index_fsmonitor_extension_ewah_invalid",
    "cache_tree_corrupt",
    "invalid_ref",
    "packed_refs_unexpected_line",
    "path_error",
    "pathspec_from_file",
    "invalid_path",
    "invalid_path_component",
    "config",
    "submodule",
    "branch",
    "ref_lock",
    "ref_store",
    "reftable",
    "filter",
    "apply",
    "path_outside_repo",
    "signing",
    "auth",
    "credentials_unavailable",
    "push_atomic_unsupported",
    "push_options_unsupported",
    "fetch_unshallow_not_shallow",
    "fetch_shallow_unsupported",
    "fetch_deepen_relative_unsupported",
    "bundle_missing_prerequisites",
    "received_pack_temp_missing",
    "invalid_attr_pathspec",
    "pathspec_no_match",
    "restore_conflict_with_staged",
    "restore_conflict_with_source",
    "restore_conflict_missing_theirs",
    "restore_conflict_missing_ours",
    "restore_conflict_not_unmerged",
    "restore_path_unmerged",
    "pathspec_beyond_symlink",
    "partial_commit_during_merge",
    "partial_commit_during_replay",
    "replay_continue_dirty_index",
    "replay_skip_already_committed",
    "embedded_repo_without_commit",
    "pathspec_ignored",
    "remove_blocked",
    "not_removing_recursively",
    "requires_work_tree",
    "empty_commit_message",
    "remove_paths_required",
    "commit_paths_required",
    "move_source_not_tracked",
    "move_destination_exists",
    "move_destination_not_directory",
    "move_into_subdirectory",
    "move_into_self",
    "move_nothing_to_move",
    "move_duplicate_destination",
    "move_bad_source",
    "move_missing_index_entry",
    "remote_unreadable",
    "remote_fatal",
    "local_upload_pack",
    "local_upload_pack_panicked",
    "resumable",
    "fetch_refspec",
    "fetch_into_checked_out_branch",
    "would_overwrite",
    "dirty_worktree",
    "invalid_identity",
    "cancelled",
    "identity",
    "message",
    "not_fast_forward",
    "nothing_to_commit",
    "unrelated_histories",
    "object_format_mismatch",
    "checkout_rollback_incomplete",
    "nothing_staged_to_commit",
    "detached_head",
    "unborn_head",
    "invalid_head",
    "merge_commit",
    "replay_source_at_head",
    "sequencer_unknown_command",
    "replay_continue_unmerged",
    "no_replay_in_progress",
    "no_merge_in_progress",
    "operation_in_progress",
    "branch_checked_out_in_worktree",
    "branch_not_found",
    "branch_list_names_date_sort",
    "tag",
    "worktree",
    "corrupt_stash",
    "stash_not_found",
    "stash_commit_not_on_stack",
    "reflog_unknown_ref",
    "reflog_invalid_selector",
    "reflog_invalid_index",
    "reflog_index_out_of_range",
    "stash_no_initial_commit",
    "stash_bare_repository",
    "blame_worktree_bare_repository",
    "stash_index_conflict",
    "stash_option_conflict",
    "hunk_selection",
    "stash_missing_head",
    "stash_would_overwrite_local_changes",
    "stash_index_write",
    "stash_symlink_not_utf8",
    "stash_could_not_restore_untracked",
    "amend_unborn",
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
    "unsupported_object_store",
    "no_promisor_remote",
    "precious_objects_repository",
    "odb_objects_dir_mismatch",
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

/// Slugs from `Error::kind_name` match arms in `grit-lib/src/error.rs` (must stay in sync with the table).
fn kind_name_slugs_from_source() -> Vec<String> {
    let src = include_str!("../src/error.rs");
    let anchor = "pub fn kind_name";
    let start = src
        .find(anchor)
        .unwrap_or_else(|| panic!("{anchor} not found in error.rs"));
    let match_start = src[start..]
        .find("match self {")
        .unwrap_or_else(|| panic!("kind_name match block not found"));
    let block = &src[start + match_start..];
    let mut slugs = Vec::new();
    let mut depth = 0usize;
    for line in block.lines() {
        if line.contains("match self {") {
            depth = 1;
            continue;
        }
        if depth == 0 {
            continue;
        }
        depth += line.matches('{').count();
        depth = depth.saturating_sub(line.matches('}').count());
        if depth == 0 {
            break;
        }
        if let Some(pos) = line.find("=> \"") {
            let after = &line[pos + 4..];
            if let Some(end) = after.find('"') {
                slugs.push(after[..end].to_owned());
            }
            continue;
        }
        let trimmed = line.trim();
        if trimmed.starts_with('"') {
            let inner = trimmed.trim_start_matches('"');
            if let Some(end) = inner.find('"') {
                slugs.push(inner[..end].to_owned());
            }
        }
    }
    slugs
}

#[test]
fn error_kind_names_are_stable() {
    let parsed = kind_name_slugs_from_source();
    assert_eq!(
        parsed.len(),
        EXPECTED_KIND_NAMES.len(),
        "update EXPECTED_KIND_NAMES when kind_name arms change"
    );
    for (idx, (live, pinned)) in parsed.iter().zip(EXPECTED_KIND_NAMES.iter()).enumerate() {
        assert_eq!(live, pinned, "kind slug mismatch at index {idx}");
    }
}

#[test]
fn path_io_preserves_raw_os_error_identity() {
    use grit_lib::error::{Error, GitDirPathOperation};
    use grit_lib::repo::RepositoryError;

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
