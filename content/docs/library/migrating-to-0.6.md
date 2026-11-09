---
title: Migrating from 0.5.x to 0.6
summary: Theme-by-theme API updates for embedders moving to the stable 0.6 library surface, plus a generated path map from v0.5.31.
---

Grit-lib **0.6** keeps on-disk and wire compatibility with Git, but the **public Rust API** is intentionally smaller: one [`Repository`](rustdoc:grit_lib::repo::Repository) entry point, typed handles, options structs, and per-area errors. This page is for code written against **0.5.x** (snapshot baseline **v0.5.31**).

For the stability rules and handle map, see [`grit-lib/API.md`](https://github.com/gitbutlerapp/grit/blob/main/grit-lib/API.md) and the [API layout](api-layout.md). For an overview of the 0.6 surface, see [API layout](api-layout.md) and [Repository](repository.md).

## 1. Theme-by-theme changes

### Repository handles instead of free functions

Operations that used to take `&Repository`, `repo.git_dir()`, or loose module paths are methods on the handle that owns the area. Open the repo once, then call the accessor:

```rust
// 0.5.x
use grit_lib::{config, worktree, command_runner::hooks};
hooks::run_hook(&repo, "pre-commit", &[], None)?;
worktree::list_worktrees(&repo)?;
let _ = config::set_config_value(
    grit_lib::config::ConfigLocation::Local,
    repo.environment(),
    Some(repo.git_dir()),
    "user.name",
    "Ada",
    false,
)?;

// 0.6
repo.hooks().run("pre-commit", &[], None)?;
repo.worktrees().list()?;
let editor = grit_lib::config::ConfigEditor::open(
    repo.environment().clone(),
    repo.git_dir().to_path_buf(),
    grit_lib::config::ConfigScope::Local,
);
editor.set("user.name", "Ada", &Default::default())?;
```

The same pattern applies across the tree: [`repo.diff()`](rustdoc:grit_lib::repo::Repository::diff) for blame and diffs, [`repo.notes()`](rustdoc:grit_lib::repo::Repository::notes) for notes, [`repo.branches()`](rustdoc:grit_lib::repo::Repository::branches) / [`repo.checkout()`](rustdoc:grit_lib::repo::Repository::checkout) for branch and working-tree porcelain, [`repo.commits()`](rustdoc:grit_lib::repo::Repository::commits) for commit creation, and [`repo.index()`](rustdoc:grit_lib::repo::Repository::index) for staging.

Config **writes** no longer use top-level `config::set_config_value` helpers. Use [`ConfigEditor`](rustdoc:grit_lib::config::ConfigEditor) on a file or scope, or [`Config::set`](rustdoc:grit_lib::config::Config::set) on an open repository when editing local/worktree config in place.

### Options structs instead of `_with_x` families

Optional behaviour is a single `#[non_exhaustive]` options struct with `Default` and `with_*` setters — not parallel function names.

```rust
// 0.5.x
checkout::checkout_between_trees_with_progress(
    &repo,
    old_tree,
    new_tree,
    &options,
    &mut progress,
)?;

// 0.6
repo.checkout().checkout(
    Some(&old_tree),
    new_tree,
    &mut progress,
    &grit_lib::progress::CancelToken::never(),
)?;
```

Pass [`SwitchOptions`](rustdoc:grit_lib::porcelain::switch::SwitchOptions) (and related checkout option structs) through [`repo.checkout()`](rustdoc:grit_lib::repo::Repository::checkout) methods; long-running paths take progress and cancel as **trailing** parameters (see below).

### One `CancelToken` and one `ProgressSink`

Cancellation and progress are always the last parameters, in that order: `progress`, then `cancel`. Use [`NullProgress`](rustdoc:grit_lib::progress::NullProgress) and [`CancelToken::never()`](rustdoc:grit_lib::progress::CancelToken::never) when you do not need them.

```rust
// 0.5.x
worktree::add_linked_worktree_with_cancel(&repo, &opts, &cancel)?;

// 0.6 — linked worktrees still expose `add` and `add_with_cancel`; progress/cancel are trailing args
repo.worktrees().add_with_cancel(&opts, &cancel)?;
```

Many porcelain calls dropped separate `_with_progress` / `_with_cancel` free functions in favour of one method with trailing `progress` and `cancel`. Some handles still keep a dedicated `_with_cancel` when the default entry point does not take cancellation (for example [`RepositoryWorktrees::add`](rustdoc:grit_lib::repo_handles::RepositoryWorktrees::add) vs [`add_with_cancel`](rustdoc:grit_lib::repo_handles::RepositoryWorktrees::add_with_cancel)). Prefer the plain method when it already accepts [`CancelToken`](rustdoc:grit_lib::progress::CancelToken).

### `RefName` and `Pathspec` instead of bare strings

Ref and path selection use newtypes and [`Pathspec`](rustdoc:grit_lib::worktree::pathspec::Pathspec) rather than `&str` / `&[String]` deep in library calls. Parse at your CLI or UI boundary; inside the library, use [`References::resolve`](rustdoc:grit_lib::refs::References::resolve) and [`Diff::resolve_pathspec`](rustdoc:grit_lib::diff::Diff::resolve_pathspec).

### Typed errors per area

Stop matching `Error::Message` or `Result<_, String>`. Each area returns its own `#[non_exhaustive]` enum that converts into [`Error`](rustdoc:grit_lib::error::Error). Match on variants (see [Errors](errors.md)):

```rust
// 0.5.x — do not use string matching on Error::Message or Result<_, String>

// 0.6
use grit_lib::error::{Error, RevisionError};
match err {
    Error::Revision(RevisionError::NotFound { .. }) => { /* … */ }
    other => return Err(other.into()),
}
```

### Custom object store via `RepositoryOptions`

Injection moved into [`RepositoryOptions`](rustdoc:grit_lib::config::environment::RepositoryOptions): use [`with_object_store`](rustdoc:grit_lib::config::environment::RepositoryOptions::with_object_store) and [`with_ref_store`](rustdoc:grit_lib::config::environment::RepositoryOptions::with_ref_store) on open/discover/init, not ad hoc parameters on individual calls.

```rust
let opts = RepositoryOptions::empty()
    .with_object_store(Arc::new(my_store));
let repo = Repository::open(git_dir, opts)?;
```

### `test-support` feature for helpers

Benchmark fixtures, probe hooks, and test-only dump helpers are behind the **`test-support`** cargo feature and are **not** part of the semver public API. Depend on `grit-lib` with `features = ["test-support"]` only in tests or internal tools.

### One public path per item

Flat root re-exports and compatibility type aliases (`CreateBranchOptions`, `TagInfo`, `DeleteMode`, …) are removed. Import the canonical path (for example [`BranchCreateOptions`](rustdoc:grit_lib::porcelain::branch::BranchCreateOptions), [`TagSummary`](rustdoc:grit_lib::porcelain::tag::TagSummary), [`BranchDeleteMode`](rustdoc:grit_lib::porcelain::branch::BranchDeleteMode)). The [canonical module map](api-layout.md) lists flat → domain paths.

## 2. Removed public paths (v0.5.31 → 0.6)

The table below is generated from `grit-lib/public-api.txt` at **v0.5.31** vs the current tree. Regenerate with `python3 scripts/api_migration.py` (CI runs `python3 scripts/api_migration.py --check` in the public-api job).

<!-- api-migration:begin -->

| Removed path (v0.5.31) | Replacement (0.6) | Notes |
| --- | --- | --- |
| `grit_lib::blame::blame_file` | `grit_lib::diff::Diff::blame` |  |
| `grit_lib::blame::resolve_blame_line_range` | `grit_lib::diff::Diff::resolve_blame_line_range` |  |
| `grit_lib::blame::resolve_blame_line_range_with_options` | `grit_lib::diff::Diff::resolve_blame_line_range_with_options` |  |
| `grit_lib::command_runner::hooks::resolve_hooks_dir` | `grit_lib::repo_handles::RepositoryHooks::hooks_dir` |  |
| `grit_lib::command_runner::hooks::run_commit_hook` | `grit_lib::repo_handles::RepositoryHooks::run_commit` |  |
| `grit_lib::command_runner::hooks::run_commit_hook_checked` | `grit_lib::repo_handles::RepositoryHooks::run_commit_checked` |  |
| `grit_lib::command_runner::hooks::run_hook` | `grit_lib::repo_handles::RepositoryHooks::run` |  |
| `grit_lib::command_runner::hooks::run_hook_in_git_dir` | `grit_lib::repo_handles::RepositoryHooks::run_in_git_dir` |  |
| `grit_lib::command_runner::hooks::run_hook_opts` | `grit_lib::repo_handles::RepositoryHooks::run` |  |
| `grit_lib::command_runner::hooks::run_post_checkout` | `grit_lib::repo_handles::RepositoryHooks::run_post_checkout` |  |
| `grit_lib::command_runner::hooks::run_post_merge` | `grit_lib::repo_handles::RepositoryHooks::run_post_merge` |  |
| `grit_lib::command_runner::hooks::run_post_rewrite` | `grit_lib::repo_handles::RepositoryHooks::run_post_rewrite` |  |
| `grit_lib::command_runner::hooks::run_pre_push_hook` | `grit_lib::repo_handles::RepositoryHooks::run_pre_push` |  |
| `grit_lib::command_runner::hooks::run_pre_rebase` | `grit_lib::repo_handles::RepositoryHooks::run_pre_rebase` |  |
| `grit_lib::config::add_config_value` | `grit_lib::config::ConfigEditor::add_value` | use ConfigEditor on a repository config snapshot |
| `grit_lib::config::remove_config_section` | `grit_lib::config::ConfigEditor::remove_section` | use ConfigEditor on a repository config snapshot |
| `grit_lib::config::rename_config_section` | `grit_lib::config::ConfigEditor::rename_section` | use ConfigEditor on a repository config snapshot |
| `grit_lib::config::set_config_value` | `grit_lib::config::ConfigEditor::set` | use ConfigEditor on a repository config snapshot |
| `grit_lib::config::unset_all_config_values` | `grit_lib::config::ConfigEditor::unset_all` | use ConfigEditor on a repository config snapshot |
| `grit_lib::config::unset_config_value` | `grit_lib::config::ConfigEditor::unset` | use ConfigEditor on a repository config snapshot |
| `grit_lib::diff::DiffAlgorithm::similar_algorithm` | `core::str::FromStr::from_str (DiffAlgorithm)` |  |
| `grit_lib::diff::anchored_unified_diff` | `grit_lib::diff::unified_diff` |  |
| `grit_lib::diff::annotate_unified_diff_no_newline_at_eof` | `grit_lib::diff::structured (hunk metadata)` |  |
| `grit_lib::diff::count_changes_with_algorithm` | `grit_lib::diff::Diff::files` |  |
| `grit_lib::diff::line_diff_unified_hunk_body` | `grit_lib::diff::unified_diff` |  |
| `grit_lib::diff::rerere::rerere_clear` | `grit_lib::merge::Merge::abort (rerere state)` |  |
| `grit_lib::diff::resolve_indent_heuristic` | `grit_lib::diff::DiffOptions::resolve_indent_heuristic` |  |
| `grit_lib::diff::unified_diff_histogram_hunks_only` | `grit_lib::diff::unified_diff` |  |
| `grit_lib::diff::unified_diff_with_prefix_and_funcname` | `grit_lib::diff::unified_diff` |  |
| `grit_lib::diff::unified_diff_with_prefix_and_funcname_and_algorithm` | `grit_lib::diff::unified_diff` |  |
| `grit_lib::notes::apply_prepared_notes_rewrite` | `grit_lib::repo_handles::RepositoryNotes::apply_rewrite_plan` |  |
| `grit_lib::notes::copy_notes_for_rewrite` | `grit_lib::repo_handles::RepositoryNotes::copy_for_rewrite` |  |
| `grit_lib::notes::notes_merge_inner` | `grit_lib::repo_handles::RepositoryNotes::merge_refs` |  |
| `grit_lib::notes::prepare_notes_for_rewrite` | `grit_lib::repo_handles::RepositoryNotes::prepare_rewrite` |  |
| `grit_lib::notes::read_notes_tree` | `grit_lib::repo_handles::RepositoryNotes::read_tree` |  |
| `grit_lib::notes::read_notes_tree_limited` | `grit_lib::repo_handles::RepositoryNotes::read_tree_limited` |  |
| `grit_lib::notes::write_notes_commit` | `grit_lib::repo_handles::RepositoryNotes::write_tree_commit` |  |
| `grit_lib::porcelain::add::stage` | `grit_lib::index::IndexFile::stage` |  |
| `grit_lib::porcelain::add::stage_for_commit` | `grit_lib::index::IndexFile::stage_for_commit` |  |
| `grit_lib::porcelain::add::stage_on_index` | `grit_lib::index::IndexFile::stage` |  |
| `grit_lib::porcelain::branch::CreateBranchOptions` | `grit_lib::porcelain::branch::BranchCreateOptions` |  |
| `grit_lib::porcelain::branch::CreateBranchOptions::at_head` | `grit_lib::porcelain::branch::BranchCreateOptions::at_head` |  |
| `grit_lib::porcelain::branch::CreateBranchOptions::clone` | `grit_lib::porcelain::branch::BranchCreateOptions::clone` |  |
| `grit_lib::porcelain::branch::CreateBranchOptions::dry_run` | `grit_lib::porcelain::branch::BranchCreateOptions::dry_run` |  |
| `grit_lib::porcelain::branch::CreateBranchOptions::eq` | `grit_lib::porcelain::branch::BranchCreateOptions::eq` |  |
| `grit_lib::porcelain::branch::CreateBranchOptions::fmt` | `grit_lib::porcelain::branch::BranchCreateOptions::fmt` |  |
| `grit_lib::porcelain::branch::CreateBranchOptions::force` | `grit_lib::porcelain::branch::BranchCreateOptions::force` |  |
| `grit_lib::porcelain::branch::CreateBranchOptions::name` | `grit_lib::porcelain::branch::BranchCreateOptions::name` |  |
| `grit_lib::porcelain::branch::CreateBranchOptions::start` | `grit_lib::porcelain::branch::BranchCreateOptions::start` |  |
| `grit_lib::porcelain::branch::CreateBranchOptions::track` | `grit_lib::porcelain::branch::BranchCreateOptions::track` |  |
| `grit_lib::porcelain::branch::DeleteMode` | `grit_lib::porcelain::branch::BranchDeleteMode` |  |
| `grit_lib::porcelain::branch::DeleteMode::Force` | `grit_lib::porcelain::branch::BranchDeleteMode::Force` |  |
| `grit_lib::porcelain::branch::DeleteMode::Safe` | `grit_lib::porcelain::branch::BranchDeleteMode::Safe` |  |
| `grit_lib::porcelain::branch::DeleteMode::clone` | `grit_lib::porcelain::branch::BranchDeleteMode::clone` |  |
| `grit_lib::porcelain::branch::DeleteMode::eq` | `grit_lib::porcelain::branch::BranchDeleteMode::eq` |  |
| `grit_lib::porcelain::branch::DeleteMode::fmt` | `grit_lib::porcelain::branch::BranchDeleteMode::fmt` |  |
| `grit_lib::porcelain::branch::RenameBranchOptions` | `grit_lib::porcelain::branch::BranchRenameOptions` |  |
| `grit_lib::porcelain::branch::RenameBranchOptions::clone` | `grit_lib::porcelain::branch::BranchRenameOptions::clone` |  |
| `grit_lib::porcelain::branch::RenameBranchOptions::dry_run` | `grit_lib::porcelain::branch::BranchRenameOptions::dry_run` |  |
| `grit_lib::porcelain::branch::RenameBranchOptions::eq` | `grit_lib::porcelain::branch::BranchRenameOptions::eq` |  |
| `grit_lib::porcelain::branch::RenameBranchOptions::fmt` | `grit_lib::porcelain::branch::BranchRenameOptions::fmt` |  |
| `grit_lib::porcelain::branch::RenameBranchOptions::force` | `grit_lib::porcelain::branch::BranchRenameOptions::force` |  |
| `grit_lib::porcelain::branch::RenameBranchOptions::new` | `grit_lib::porcelain::branch::BranchRenameOptions::new` |  |
| `grit_lib::porcelain::branch::RenameBranchOptions::old` | `grit_lib::porcelain::branch::BranchRenameOptions::old` |  |
| `grit_lib::porcelain::branch::create_branch` | `grit_lib::repo_handles::Branches::create` |  |
| `grit_lib::porcelain::branch::delete_branch` | `grit_lib::repo_handles::Branches::delete` |  |
| `grit_lib::porcelain::branch::list_branches` | `grit_lib::repo_handles::Branches::list_simple` |  |
| `grit_lib::porcelain::branch::preview_unset_upstream` | `grit_lib::repo_handles::Branches::preview_unset_upstream` |  |
| `grit_lib::porcelain::branch::rename_branch` | `grit_lib::repo_handles::Branches::rename` |  |
| `grit_lib::porcelain::branch::resolve_create_tracking` | `grit_lib::repo_handles::Branches::resolve_create_tracking` |  |
| `grit_lib::porcelain::branch::set_upstream` | `grit_lib::repo_handles::Branches::set_upstream` |  |
| `grit_lib::porcelain::branch::unset_upstream` | `grit_lib::repo_handles::Branches::unset_upstream` |  |
| `grit_lib::porcelain::branch::validate_set_upstream` | `grit_lib::repo_handles::Branches::validate_set_upstream` |  |
| `grit_lib::porcelain::checkout::checkout_between_trees` | `grit_lib::repo_handles::Checkout::checkout` |  |
| `grit_lib::porcelain::checkout::checkout_between_trees_with_progress` | `grit_lib::repo_handles::Checkout::checkout` |  |
| `grit_lib::porcelain::checkout::checkout_hard_to_tree_from_head_tree` | `grit_lib::repo_handles::Checkout::checkout_hard_to_tree` |  |
| `grit_lib::porcelain::checkout::checkout_tree_changes` | `grit_lib::repo_handles::Checkout::checkout_tree_changes` |  |
| `grit_lib::porcelain::checkout_reflog::append_head_checkout_reflog` | `removed (used by Checkout::switch)` |  |
| `grit_lib::porcelain::checkout_reflog::checkout_reflog_identity` | `removed (used by Checkout::switch)` |  |
| `grit_lib::porcelain::checkout_reflog::create_branch_ref_with_reflog` | `removed (used by Checkout::switch)` |  |
| `grit_lib::porcelain::checkout_reflog::finish_switch_ref_updates` | `removed (used by Checkout::switch)` |  |
| `grit_lib::porcelain::checkout_reflog::is_already_detached_at` | `removed (used by Checkout::switch)` |  |
| `grit_lib::porcelain::checkout_reflog::is_already_on_branch` | `removed (used by Checkout::switch)` |  |
| `grit_lib::porcelain::clean::clean_untracked` | `grit_lib::repo_handles::Checkout::clean` |  |
| `grit_lib::porcelain::clean::clean_untracked_with_cancel` | `grit_lib::repo_handles::Checkout::clean` |  |
| `grit_lib::porcelain::commit::commit_with_patch` | `grit_lib::commits::Commits::create` |  |
| `grit_lib::porcelain::commit::commit_with_scope` | `grit_lib::commits::Commits::create` |  |
| `grit_lib::porcelain::commit::commit_worktree` | `grit_lib::commits::Commits::create` |  |
| `grit_lib::porcelain::commit::create_commit` | `grit_lib::commits::Commits::create` |  |
| `grit_lib::porcelain::commit::default_commit_scope` | `grit_lib::commits::Commits::default_scope` |  |
| `grit_lib::porcelain::commit::pending_commit_message` | `grit_lib::commits::Commits::pending_message` |  |
| `grit_lib::porcelain::commit::plan_commit` | `grit_lib::commits::Commits::plan` |  |
| `grit_lib::porcelain::commit::prepare_commit_bytes` | `grit_lib::commits::Commits::prepare_bytes` |  |
| `grit_lib::porcelain::commit::write_commit_object` | `grit_lib::commits::Commits::write_object` |  |
| `grit_lib::porcelain::hunk_select::diff_options_for_hunk_select` | `removed (use DiffOptions)` |  |
| `grit_lib::porcelain::hunk_select::list_unstaged_hunk_ids` | `grit_lib::index::IndexFile::list_unstaged_hunk_ids` |  |
| `grit_lib::porcelain::hunk_select::resolve_selection` | `grit_lib::index::IndexFile::resolve_hunk_selection` |  |
| `grit_lib::porcelain::hunk_select::restore_hunks` | `grit_lib::index::IndexFile::restore_hunks` |  |
| `grit_lib::porcelain::hunk_select::reverse_apply_patches_to_worktree` | `removed (use IndexFile::restore_hunks)` |  |
| `grit_lib::porcelain::hunk_select::stage_hunks` | `grit_lib::index::IndexFile::stage_hunks` |  |
| `grit_lib::porcelain::hunk_select::unstaged_file_diffs` | `grit_lib::index::IndexFile::unstaged_file_diffs` |  |
| `grit_lib::porcelain::paths::move_path` | `grit_lib::repo_handles::Checkout::move_path` |  |
| `grit_lib::porcelain::paths::move_paths` | `grit_lib::repo_handles::Checkout::move_paths` |  |
| `grit_lib::porcelain::paths::remove_paths` | `grit_lib::repo_handles::Checkout::remove_paths` |  |
| `grit_lib::porcelain::pathspec_match::ensure_pathspec_matches_known_in_repository` | `grit_lib::index::IndexFile::ensure_pathspec_matches_known` |  |
| `grit_lib::porcelain::pathspec_match::pathspec_matches_known_in_repository` | `grit_lib::index::IndexFile::pathspec_matches_known` |  |
| `grit_lib::porcelain::reset::preview_reset` | `grit_lib::repo_handles::Checkout::preview_reset` |  |
| `grit_lib::porcelain::reset::reset_to_commit` | `grit_lib::repo_handles::Checkout::reset` |  |
| `grit_lib::porcelain::restore::conflict_style_from_config` | `grit_lib::repo_handles::Checkout::conflict_style` |  |
| `grit_lib::porcelain::restore::restore_paths` | `grit_lib::repo_handles::Checkout::restore` |  |
| `grit_lib::porcelain::stage_tracked::stage_tracked_modifications` | `grit_lib::index::IndexFile::stage_tracked_modifications` |  |
| `grit_lib::porcelain::stage_tracked::stage_tracked_modifications_in_index` | `grit_lib::index::IndexFile::stage_tracked_modifications_on` |  |
| `grit_lib::porcelain::stage_tracked::stage_tracked_modifications_in_index_with_thresholds` | `grit_lib::index::IndexFile::stage_tracked_modifications_on` |  |
| `grit_lib::porcelain::stage_tracked::stage_tracked_modifications_with_thresholds` | `grit_lib::index::IndexFile::stage_tracked_modifications` |  |
| `grit_lib::porcelain::staging::stage_worktree_changes` | `grit_lib::index::IndexFile::stage_worktree_paths` |  |
| `grit_lib::porcelain::status::collect_untracked_and_ignored` | `grit_lib::status::Status::untracked` |  |
| `grit_lib::porcelain::status::collect_untracked_and_ignored_with_cache` | `grit_lib::status::Status::untracked` |  |
| `grit_lib::porcelain::status::collect_untracked_and_ignored_with_rules_and_cancel` | `grit_lib::status::Status::untracked_with_meta` |  |
| `grit_lib::porcelain::status::dir_is_nested_submodule_worktree` | `removed (status internals)` |  |
| `grit_lib::porcelain::status::expand_untracked_for_staging` | `grit_lib::status::Status::expand_untracked_for_staging` |  |
| `grit_lib::porcelain::status::expand_untracked_for_staging_with_rules` | `grit_lib::status::Status::expand_untracked_for_staging` |  |
| `grit_lib::porcelain::status::status` | `grit_lib::status::Status::run` |  |
| `grit_lib::porcelain::status::status_diff_entry_matches_pathspec_typed` | `grit_lib::status::Status::diff_entry_matches_pathspec` |  |
| `grit_lib::porcelain::switch::resolve_create_branch` | `grit_lib::repo_handles::Checkout::resolve_create_branch` |  |
| `grit_lib::porcelain::switch::resolve_revision_spec` | `grit_lib::repo_handles::Checkout::resolve_revision_spec` |  |
| `grit_lib::porcelain::switch::resolve_switch_target` | `grit_lib::repo_handles::Checkout::resolve_switch_target` |  |
| `grit_lib::porcelain::switch::switch_repository` | `grit_lib::repo_handles::Checkout::switch` |  |
| `grit_lib::porcelain::tag::TagInfo` | `grit_lib::porcelain::tag::TagSummary` | use TagSummary instead |
| `grit_lib::porcelain::tag::create_tag` | `grit_lib::repo_handles::Tags::create` |  |
| `grit_lib::porcelain::tag::creator_date` | `grit_lib::repo_handles::Tags::creator_date` |  |
| `grit_lib::porcelain::tag::delete_tag` | `grit_lib::repo_handles::Tags::delete` |  |
| `grit_lib::porcelain::tag::list_tags` | `grit_lib::repo_handles::Tags::list` |  |
| `grit_lib::porcelain::tag::list_tags_with_detail` | `grit_lib::repo_handles::Tags::list` |  |
| `grit_lib::porcelain::tag::tag_ref_metadata` | `grit_lib::repo_handles::Tags::ref_metadata` |  |
| `grit_lib::porcelain::tag::verify_tag_by_name` | `grit_lib::repo_handles::Tags::verify` |  |
| `grit_lib::porcelain::workspace_snapshot::capture_workspace_snapshot` | `grit_lib::repo_handles::RepositoryWorkspaceSnapshot::capture` |  |
| `grit_lib::porcelain::workspace_snapshot::pin_workspace_snapshot` | `grit_lib::repo_handles::RepositoryWorkspaceSnapshot::pin` |  |
| `grit_lib::porcelain::workspace_snapshot::restore_workspace_snapshot` | `grit_lib::repo_handles::RepositoryWorkspaceSnapshot::restore` |  |
| `grit_lib::porcelain::workspace_snapshot::unpin_workspace_snapshot` | `grit_lib::repo_handles::RepositoryWorkspaceSnapshot::unpin` |  |
| `grit_lib::refs::branch_tracking::apply_push_remote_tracking_updates` | `grit_lib::repo_handles::Branches::apply_push_remote_tracking_updates` |  |
| `grit_lib::refs::branch_tracking::compare_with_target_optional` | `grit_lib::repo_handles::Branches::compare_with_target` |  |
| `grit_lib::refs::branch_tracking::list_branches` | `grit_lib::repo_handles::Branches::list` |  |
| `grit_lib::refs::branch_tracking::upstream_status` | `grit_lib::repo_handles::Branches::upstream_status` |  |
| `grit_lib::refs::branch_tracking::upstream_tracking_full_ref` | `grit_lib::repo_handles::Branches::upstream_tracking_full_ref` |  |
| `grit_lib::repo::InitOptions::environment` | `grit_lib::config::environment::RepositoryOptions::environment` |  |
| `grit_lib::repo::InitOptions::with_environment` | `grit_lib::repo::InitOptions::with_repository_options` | use `InitOptions::with_repository_options(RepositoryOptions::with_environment(env))` |
| `grit_lib::revwalk::rev_list::shallow_boundary_oids` | `grit_lib::revwalk::handle::RevWalk::shallow_boundary_oids` |  |
| `grit_lib::revwalk::rev_parse::load_graft_parents` | `removed (crate-internal graft helpers)` |  |
| `grit_lib::revwalk::rev_parse::superproject_work_tree_from_nested_git_modules` | `removed (crate-internal submodule helper)` |  |
| `grit_lib::worktree::add_linked_worktree` | `grit_lib::repo_handles::RepositoryWorktrees::add` |  |
| `grit_lib::worktree::add_linked_worktree_with_cancel` | `grit_lib::repo_handles::RepositoryWorktrees::add_with_cancel` |  |
| `grit_lib::worktree::common_git_dir` | `grit_lib::repo::Repository::common_git_dir` |  |
| `grit_lib::worktree::copy_filtered_worktree_config` | `removed (crate-internal worktree setup)` |  |
| `grit_lib::worktree::ensure_branch_not_checked_out_elsewhere` | `grit_lib::repo_handles::RepositoryWorktrees::ensure_branch_not_checked_out_elsewhere` |  |
| `grit_lib::worktree::ensure_branch_not_checked_out_in_any_worktree` | `grit_lib::repo_handles::RepositoryWorktrees::ensure_branch_not_checked_out_in_any_worktree` |  |
| `grit_lib::worktree::find_worktree_with_branch_checked_out` | `grit_lib::repo_handles::RepositoryWorktrees::find_with_branch_checked_out` |  |
| `grit_lib::worktree::list_worktrees` | `grit_lib::repo_handles::RepositoryWorktrees::list` |  |
| `grit_lib::worktree::move_linked_worktree` | `grit_lib::repo_handles::RepositoryWorktrees::move_linked` |  |
| `grit_lib::worktree::prune_linked_worktrees` | `grit_lib::repo_handles::RepositoryWorktrees::prune` |  |
| `grit_lib::worktree::remove_linked_worktree` | `grit_lib::repo_handles::RepositoryWorktrees::remove` |  |
| `grit_lib::worktree::repair_worktrees` | `grit_lib::repo_handles::RepositoryWorktrees::repair` |  |
| `grit_lib::worktree::worktree_admin_dir` | `grit_lib::repo_handles::RepositoryWorktrees::admin_dir` |  |

<!-- api-migration:end -->
