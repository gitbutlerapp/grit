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

/// Slugs [`Error::kind_name`] emitted before the umbrella collapsed to one variant per area.
/// Each must still be emitted by some area's `kind_name` table.
const EXPECTED_KIND_NAMES: &[&str] = &[
    "not_a_repository",
    "forbidden_bare_repository",
    "dubious_ownership",
    "unsupported_repository_format_version",
    "invalid_ref_storage_format",
    "init_object_format_change",
    "init_bare_with_separate_git_dir",
    "init_ref_storage_with_injected_ref_store",
    "requires_work_tree",
    "message",
    "git_dir_path_io",
    "worktree_path_io",
    "unsupported_object_store",
    "precious_objects_repository",
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
    "worktree_case_collision",
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

/// Slugs emitted by the area enums' `kind_name` tables beyond [`EXPECTED_KIND_NAMES`], sorted.
///
/// Every umbrella variant delegates [`Error::kind_name`] to its area enum, so these tables are
/// part of the stable contract too. Add new slugs here; never rename or remove one.
const AREA_KIND_NAMES: &[&str] = &[
    "bitmap_walk",
    "blame_copy_level_too_high",
    "bundle",
    "clone",
    "corrupt_stash",
    "diff_arguments",
    "funcname_pattern",
    "head_not_detached",
    "head_not_on_branch",
    "index_fsmonitor_hook_empty_token",
    "index_fsmonitor_hook_failed",
    "invalid_reflog_expiry",
    "move_rollback_incomplete",
    "multiple_merge_bases",
    "no_index",
    "no_merge_base",
    "non_utf8_path",
    "pickaxe",
    "promisor_fetch_too_deep",
    "pull",
    "ref_lock_released",
    "ref_query_needs_object_database",
    "ref_query_target_not_commit",
    "remove_contains_current_directory",
    "resolve",
    "restore_paths_required",
    "restore_stage_with_remerge",
    "serve",
    "snapshot",
    "stash_bare_repository",
    "stash_commit_not_on_stack",
    "stash_could_not_restore_untracked",
    "stash_index_conflict",
    "stash_index_write",
    "stash_missing_head",
    "stash_no_initial_commit",
    "stash_not_found",
    "stash_option_conflict",
    "stash_symlink_not_utf8",
    "stash_worktree_update_failed",
    "stash_would_overwrite_local_changes",
    "textconv_failed",
    "textconv_spawn",
];

/// Size of [`Error`] when the error-contract step landed; area enums must keep it at or below.
const ERROR_SIZE_CEILING: usize = 112;

/// Sources whose `kind_name` tables together produce every [`Error::kind_name`] slug.
const KIND_NAME_SOURCES: &[(&str, &str)] = &[
    ("error/mod.rs", include_str!("../src/error/mod.rs")),
    ("error/object.rs", include_str!("../src/error/object.rs")),
    ("error/refs.rs", include_str!("../src/error/refs.rs")),
    (
        "error/revision.rs",
        include_str!("../src/error/revision.rs"),
    ),
    ("error/diff.rs", include_str!("../src/error/diff.rs")),
    ("error/notes.rs", include_str!("../src/error/notes.rs")),
    (
        "repo/repository_error.rs",
        include_str!("../src/repo/repository_error.rs"),
    ),
    ("index/error.rs", include_str!("../src/index/error.rs")),
    (
        "worktree/error.rs",
        include_str!("../src/worktree/error.rs"),
    ),
    (
        "porcelain/commit_error.rs",
        include_str!("../src/porcelain/commit_error.rs"),
    ),
    (
        "porcelain/merge_error.rs",
        include_str!("../src/porcelain/merge_error.rs"),
    ),
    (
        "porcelain/stash_error.rs",
        include_str!("../src/porcelain/stash_error.rs"),
    ),
    (
        "porcelain/branch_error.rs",
        include_str!("../src/porcelain/branch_error.rs"),
    ),
    ("porcelain/tag.rs", include_str!("../src/porcelain/tag.rs")),
    ("remote/error.rs", include_str!("../src/remote/error.rs")),
];

/// Bodies of every `pub fn kind_name(&self) -> &'static str` in `src`.
fn kind_name_bodies(src: &str) -> Vec<&str> {
    let signature = "pub fn kind_name(&self) -> &'static str {";
    let mut bodies = Vec::new();
    let mut rest = src;
    while let Some(start) = rest.find(signature) {
        let open = start + signature.len();
        let mut depth = 1usize;
        let mut end = open;
        for (offset, byte) in rest.as_bytes()[open..].iter().enumerate() {
            match byte {
                b'{' => depth += 1,
                b'}' => depth -= 1,
                _ => {}
            }
            if depth == 0 {
                end = open + offset;
                break;
            }
        }
        assert_eq!(depth, 0, "unbalanced braces in kind_name");
        bodies.push(&rest[open..end]);
        rest = &rest[end..];
    }
    bodies
}

/// String literals a `kind_name` body returns (`=> "slug"` or `=> { "slug" }`).
fn literal_slugs(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(arrow) = rest.find("=>") {
        rest = &rest[arrow + 2..];
        let value = rest.trim_start().trim_start_matches('{').trim_start();
        let Some(value) = value.strip_prefix('"') else {
            continue;
        };
        let Some(end) = value.find('"') else {
            continue;
        };
        out.push(value[..end].to_owned());
    }
    out
}

fn live_kind_name_slugs() -> HashSet<String> {
    let mut live = HashSet::new();
    for (name, src) in KIND_NAME_SOURCES {
        let bodies = kind_name_bodies(src);
        assert!(!bodies.is_empty(), "{name} has no kind_name table");
        for body in bodies {
            live.extend(literal_slugs(body));
        }
    }
    live
}

#[test]
fn error_kind_names_are_stable() {
    let live = live_kind_name_slugs();
    for slug in EXPECTED_KIND_NAMES {
        assert!(
            live.contains(*slug),
            "kind slug {slug} is no longer emitted; kind names are a stable contract"
        );
    }
    let pinned: HashSet<&str> = EXPECTED_KIND_NAMES
        .iter()
        .chain(AREA_KIND_NAMES)
        .copied()
        .collect();
    assert_eq!(
        pinned.len(),
        EXPECTED_KIND_NAMES.len() + AREA_KIND_NAMES.len(),
        "a slug is pinned twice"
    );
    let mut unpinned: Vec<&String> = live
        .iter()
        .filter(|s| !pinned.contains(s.as_str()))
        .collect();
    unpinned.sort();
    assert!(
        unpinned.is_empty(),
        "add new kind slugs to AREA_KIND_NAMES: {unpinned:?}"
    );
    let mut sorted = AREA_KIND_NAMES.to_vec();
    sorted.sort_unstable();
    assert_eq!(sorted, AREA_KIND_NAMES, "keep AREA_KIND_NAMES sorted");
}

#[test]
fn legacy_kind_names_survive_the_area_collapse() {
    use grit_lib::error::{ObjectError, RefError, RevisionError};
    use grit_lib::repo::RepositoryError;

    let cases: Vec<(Error, &str)> = vec![
        (
            Error::Repository(RepositoryError::InitObjectFormatChange),
            "init_object_format_change",
        ),
        (
            Error::Object(ObjectError::InvalidId { input: "x".into() }),
            "invalid_object_id",
        ),
        (
            Error::Object(ObjectError::NotFound { name: "x".into() }),
            "object_not_found",
        ),
        (
            Error::Revision(RevisionError::NotFound { spec: "x".into() }),
            "revision_not_found",
        ),
        (Error::Ref(RefError::UnbornHead), "unborn_head"),
        (Error::Cancelled, "cancelled"),
        (Error::Io(std::io::Error::other("x")), "io"),
    ];
    for (err, kind) in cases {
        assert_eq!(err.kind_name(), kind, "{err:?}");
        assert!(EXPECTED_KIND_NAMES.contains(&kind));
    }
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
    assert_eq!(Error::Commit(CommitError::NothingToCommit).area(), "commit");
    assert_eq!(Error::Remote(RemoteError::HttpUnavailable).area(), "remote");
}

#[test]
fn area_enum_area_agrees_with_the_umbrella() {
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
    assert_eq!(RemoteError::HttpUnavailable.area(), "remote");

    let lock: Error = IndexError::LockHeld(LockHeldError {
        path: PathBuf::from("index.lock"),
        holder: None,
    })
    .into();
    assert!(matches!(lock, Error::LockHeld(_)), "{lock:?}");
    assert_eq!(lock.area(), "lock_held");
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
