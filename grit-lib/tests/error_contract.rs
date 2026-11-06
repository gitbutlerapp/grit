//! Pinning tests for the error contract (task 7099 step 1).

#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::assertions_on_constants
)]
use grit_lib::error::Error;

/// Slugs from [`Error::kind_name`](grit_lib::error::Error::kind_name) in `grit-lib/src/error.rs`
/// (match arm order).
const UMBRELLA_KIND_NAMES: &[&str] = &[
    "not_a_repository",
    "forbidden_bare_repository",
    "dubious_ownership",
    "unsupported_repository_format_version",
    "invalid_ref_storage_format",
    "init_object_format_change",
    "init_bare_with_separate_git_dir",
    "revision_not_found",
    "io",
    "lock_held",
    "worktree_path_io",
    "git_dir_path_io",
    "worktree_file_changed",
    "index_error",
    "index_unmerged",
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
    "rev_parse",
    "rev_list",
    "hook",
    "history",
    "blame_ignore_rev_unknown",
    "blame_ignore_revs_file_open",
    "blame_ignore_revs_file_invalid_rev",
];

/// Slugs from [`ObjectError::kind_name`](grit_lib::objects::ObjectError::kind_name) in
/// `grit-lib/src/objects/error.rs` (literal `=> "slug"` arms only; `Nested` delegates).
const OBJECT_ERROR_KIND_NAMES: &[&str] = &[
    "invalid_object_id",
    "hash_collision",
    "object_not_found",
    "promised_object_missing",
    "promisor_fetch_failed",
    "promisor_lazy_fetch_nested_too_deep",
    "promisor_lazy_fetch_incomplete",
    "promisor_lazy_fetch_no_git_dir",
    "promisor_fetch_still_missing",
    "operation_cancelled",
    "promisor_remote_failure",
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
    "zlib",
    "corrupt_loose_object",
    "corrupt_packed_object",
    "loose_hash_mismatch",
    "loose_object_write",
    "object_stream_truncated",
    "maintenance_running",
    "midx",
    "offload",
    "no_promisor_remote",
    "unsupported_object_store",
    "precious_objects_repository",
    "object_format_mismatch",
    "odb_objects_dir_mismatch",
    "tree_sha256_invalid_hex",
    "tree_sha256_invalid_revision",
    "tree_sha256_missing_submodules",
    "tree_sha256_requires_signing",
    "tag_verify_not_tag",
    "tag_verify_missing_object_header",
    "tag_verify_duplicate_tree_sha256_header",
    "tree_sha256_claim",
    "git_dir_path_io",
    "lock_held",
    "object_backend",
];

/// Size of [`Error`] when the error-contract step landed (area enums must stay ≤ this until step 12).
const ERROR_SIZE_CEILING: usize = 112;

fn kind_name_slugs_from_source(src: &str, impl_anchor: &str, fn_anchor: &str) -> Vec<String> {
    let impl_start = src
        .find(impl_anchor)
        .unwrap_or_else(|| panic!("{impl_anchor} not found"));
    let rest = &src[impl_start..];
    let fn_rel = rest
        .find(fn_anchor)
        .unwrap_or_else(|| panic!("{fn_anchor} not found after {impl_anchor}"));
    let start = impl_start + fn_rel;
    let match_start = src[start..]
        .find("match self {")
        .unwrap_or_else(|| panic!("kind_name match block not found"));
    let block = &src[start + match_start..];
    let mut slugs = Vec::new();
    for line in block.lines().skip(1) {
        let trimmed = line.trim();
        if trimmed == "}" {
            break;
        }
        if let Some(pos) = line.find("=> \"") {
            let after = &line[pos + 4..];
            if let Some(end) = after.find('"') {
                slugs.push(after[..end].to_owned());
            }
        }
    }
    slugs
}

fn flatten_object_delegation(umbrella: &[String], object_slugs: &[String]) -> Vec<String> {
    // `Error::Object` delegates with `e.kind_name()` (no `=> "slug"` arm), so the parsed
    // umbrella list is only the literal arms. Insert object-area slugs after
    // `init_bare_with_separate_git_dir`, matching the enum order in `error.rs`.
    let pos = umbrella
        .iter()
        .position(|s| s == "init_bare_with_separate_git_dir")
        .expect("init_bare_with_separate_git_dir arm");
    let mut out = Vec::with_capacity(umbrella.len() + object_slugs.len());
    out.extend_from_slice(&umbrella[..pos + 1]);
    out.extend_from_slice(object_slugs);
    out.extend_from_slice(&umbrella[pos + 1..]);
    out
}

fn expected_effective_kind_names_flat() -> Vec<&'static str> {
    let pos = UMBRELLA_KIND_NAMES
        .iter()
        .position(|s| *s == "init_bare_with_separate_git_dir")
        .expect("UMBRELLA_KIND_NAMES must contain init_bare_with_separate_git_dir");
    let mut out = Vec::with_capacity(UMBRELLA_KIND_NAMES.len() + OBJECT_ERROR_KIND_NAMES.len());
    out.extend_from_slice(&UMBRELLA_KIND_NAMES[..pos + 1]);
    out.extend_from_slice(OBJECT_ERROR_KIND_NAMES);
    out.extend_from_slice(&UMBRELLA_KIND_NAMES[pos + 1..]);
    out
}

#[test]
fn error_kind_names_are_stable() {
    let error_rs = include_str!("../src/error.rs");
    let object_error_rs = include_str!("../src/objects/error.rs");

    let umbrella = kind_name_slugs_from_source(error_rs, "impl Error {", "pub fn kind_name");
    assert_eq!(
        umbrella.len(),
        UMBRELLA_KIND_NAMES.len(),
        "update UMBRELLA_KIND_NAMES when Error::kind_name arms change"
    );
    for (idx, (live, pinned)) in umbrella.iter().zip(UMBRELLA_KIND_NAMES.iter()).enumerate() {
        assert_eq!(live, pinned, "umbrella kind slug mismatch at index {idx}");
    }

    let object_area =
        kind_name_slugs_from_source(object_error_rs, "impl ObjectError {", "pub fn kind_name");
    assert_eq!(
        object_area.len(),
        OBJECT_ERROR_KIND_NAMES.len(),
        "update OBJECT_ERROR_KIND_NAMES when ObjectError::kind_name arms change"
    );
    for (idx, (live, pinned)) in object_area
        .iter()
        .zip(OBJECT_ERROR_KIND_NAMES.iter())
        .enumerate()
    {
        assert_eq!(
            live, pinned,
            "object-area kind slug mismatch at index {idx}"
        );
    }

    let effective = flatten_object_delegation(&umbrella, &object_area);
    let expected = expected_effective_kind_names_flat();
    assert_eq!(
        effective.len(),
        expected.len(),
        "update UMBRELLA_KIND_NAMES / OBJECT_ERROR_KIND_NAMES when delegation shape changes"
    );
    for (idx, (live, pinned)) in effective.iter().zip(expected.iter()).enumerate() {
        assert_eq!(live, pinned, "effective kind slug mismatch at index {idx}");
    }
}

#[test]
fn path_io_preserves_raw_os_error_identity() {
    use grit_lib::error::{Error, GitDirPathOperation};

    let source = std::io::Error::from_raw_os_error(libc::EBUSY as i32);
    let err = Error::GitDirPathIo {
        path: ".git/index".into(),
        operation: GitDirPathOperation::WriteIndex,
        source,
    };
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
