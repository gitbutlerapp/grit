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
| `grit_lib::apply::BlobApplyOptions` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::apply::BlobApplyOptions::allow_binary_marker_only` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::apply::BlobApplyOptions::clone` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::apply::BlobApplyOptions::default` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::apply::BlobApplyOptions::eq` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::apply::BlobApplyOptions::fmt` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::apply::BlobApplyOptions::reverse` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::apply::apply_patch_to_blob` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::blame::blame_file` | `grit_lib::diff::Diff::blame` |  |
| `grit_lib::blame::resolve_blame_line_range` | `grit_lib::diff::Diff::resolve_blame_line_range` |  |
| `grit_lib::blame::resolve_blame_line_range_with_options` | `grit_lib::diff::Diff::resolve_blame_line_range_with_options` |  |
| `grit_lib::bundle::write_bundle_header` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::change_id::OsChangeIdGenerator` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::change_id::OsChangeIdGenerator::clone` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::change_id::OsChangeIdGenerator::default` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::change_id::OsChangeIdGenerator::fmt` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::change_id::OsChangeIdGenerator::random_bytes` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::command_runner::hooks::RefHookContext` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::command_runner::hooks::RefHookContext::clone` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::command_runner::hooks::post_rewrite_stdin` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
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
| `grit_lib::command_runner::run_sh_dash_c` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::commit::encoding::decode_bytes` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::commit::encoding::encode_header_text` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::commit::encoding::encode_unicode` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::commit::encoding::ensure_body_trailing_newline` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::commit::encoding::find_invalid_utf8` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::commit::encoding::is_strict_utf8` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::commit::signing::GPG_SIG_HEADER_SHA1` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::commit::signing::GPG_SIG_HEADER_SHA256` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::commit::signing::add_header_signature` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::commit::signing::append_tag_signature` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::commit::signing::commit_gpg_sig_header` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::commit::signing::committer_signing_default` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::commit::signing::extra_headers_without_commit_signatures` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::commit::signing::extract_signed_payload` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::commit::signing::parse_signed_buffer` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::commit::signing::should_sign_commit` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::commit::signing::sign_buffer` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::commit::signing::sign_serialized_commit` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::commit::signing::sign_serialized_tag` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::commit::trailers::append_cherry_picked_from_line` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::config::add_config_value` | `grit_lib::config::ConfigEditor::add_value` | use ConfigEditor on a repository config snapshot |
| `grit_lib::config::config_entry_origin_line` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::config::environment::Environment::capture_process` | `grit_lib::config::environment::Environment (unchanged; the snapshot lists this member under grit_lib::prelude::Environment)` |  |
| `grit_lib::config::environment::Environment::clone` | `grit_lib::config::environment::Environment (unchanged; the snapshot lists this member under grit_lib::prelude::Environment)` |  |
| `grit_lib::config::environment::Environment::config_fingerprint` | `grit_lib::config::environment::Environment (unchanged; the snapshot lists this member under grit_lib::prelude::Environment)` |  |
| `grit_lib::config::environment::Environment::default_xdg_config_git_file` | `grit_lib::config::environment::Environment (unchanged; the snapshot lists this member under grit_lib::prelude::Environment)` |  |
| `grit_lib::config::environment::Environment::discovery_cwd` | `grit_lib::config::environment::Environment (unchanged; the snapshot lists this member under grit_lib::prelude::Environment)` |  |
| `grit_lib::config::environment::Environment::empty` | `grit_lib::config::environment::Environment (unchanged; the snapshot lists this member under grit_lib::prelude::Environment)` |  |
| `grit_lib::config::environment::Environment::fmt` | `grit_lib::config::environment::Environment (unchanged; the snapshot lists this member under grit_lib::prelude::Environment)` |  |
| `grit_lib::config::environment::Environment::from_vars` | `grit_lib::config::environment::Environment (unchanged; the snapshot lists this member under grit_lib::prelude::Environment)` |  |
| `grit_lib::config::environment::Environment::git_now_date_override` | `grit_lib::config::environment::Environment (unchanged; the snapshot lists this member under grit_lib::prelude::Environment)` |  |
| `grit_lib::config::environment::Environment::home_dir` | `grit_lib::config::environment::Environment (unchanged; the snapshot lists this member under grit_lib::prelude::Environment)` |  |
| `grit_lib::config::environment::Environment::is_fetch_options_default` | `grit_lib::config::environment::Environment (unchanged; the snapshot lists this member under grit_lib::prelude::Environment)` |  |
| `grit_lib::config::environment::Environment::push_config_override` | `grit_lib::config::environment::Environment (unchanged; the snapshot lists this member under grit_lib::prelude::Environment)` |  |
| `grit_lib::config::environment::Environment::subprocess_environment` | `grit_lib::config::environment::Environment (unchanged; the snapshot lists this member under grit_lib::prelude::Environment)` |  |
| `grit_lib::config::environment::Environment::var` | `grit_lib::config::environment::Environment (unchanged; the snapshot lists this member under grit_lib::prelude::Environment)` |  |
| `grit_lib::config::environment::Environment::var_os` | `grit_lib::config::environment::Environment (unchanged; the snapshot lists this member under grit_lib::prelude::Environment)` |  |
| `grit_lib::config::environment::Environment::with_git_dir` | `grit_lib::config::environment::Environment (unchanged; the snapshot lists this member under grit_lib::prelude::Environment)` |  |
| `grit_lib::config::environment::Environment::with_var` | `grit_lib::config::environment::Environment (unchanged; the snapshot lists this member under grit_lib::prelude::Environment)` |  |
| `grit_lib::config::environment::Environment::with_work_tree` | `grit_lib::config::environment::Environment (unchanged; the snapshot lists this member under grit_lib::prelude::Environment)` |  |
| `grit_lib::config::environment::RepositoryOptions::clone` | `grit_lib::config::environment::RepositoryOptions (unchanged; the snapshot lists this member under grit_lib::prelude::RepositoryOptions)` |  |
| `grit_lib::config::environment::RepositoryOptions::default` | `grit_lib::config::environment::RepositoryOptions (unchanged; the snapshot lists this member under grit_lib::prelude::RepositoryOptions)` |  |
| `grit_lib::config::environment::RepositoryOptions::empty` | `grit_lib::config::environment::RepositoryOptions (unchanged; the snapshot lists this member under grit_lib::prelude::RepositoryOptions)` |  |
| `grit_lib::config::environment::RepositoryOptions::fmt` | `grit_lib::config::environment::RepositoryOptions (unchanged; the snapshot lists this member under grit_lib::prelude::RepositoryOptions)` |  |
| `grit_lib::config::environment::RepositoryOptions::for_serving` | `grit_lib::config::environment::RepositoryOptions (unchanged; the snapshot lists this member under grit_lib::prelude::RepositoryOptions)` |  |
| `grit_lib::config::environment::RepositoryOptions::from_process` | `grit_lib::config::environment::RepositoryOptions (unchanged; the snapshot lists this member under grit_lib::prelude::RepositoryOptions)` |  |
| `grit_lib::config::environment::RepositoryOptions::ownership_check` | `grit_lib::config::environment::RepositoryOptions (unchanged; the snapshot lists this member under grit_lib::prelude::RepositoryOptions)` |  |
| `grit_lib::config::environment::RepositoryOptions::uses_default_ownership_check` | `grit_lib::config::environment::RepositoryOptions (unchanged; the snapshot lists this member under grit_lib::prelude::RepositoryOptions)` |  |
| `grit_lib::config::environment::RepositoryOptions::with_change_id_generator` | `grit_lib::config::environment::RepositoryOptions (unchanged; the snapshot lists this member under grit_lib::prelude::RepositoryOptions)` |  |
| `grit_lib::config::environment::RepositoryOptions::with_command_runner` | `grit_lib::config::environment::RepositoryOptions (unchanged; the snapshot lists this member under grit_lib::prelude::RepositoryOptions)` |  |
| `grit_lib::config::environment::RepositoryOptions::with_diagnostics` | `grit_lib::config::environment::RepositoryOptions (unchanged; the snapshot lists this member under grit_lib::prelude::RepositoryOptions)` |  |
| `grit_lib::config::environment::RepositoryOptions::with_environment` | `grit_lib::config::environment::RepositoryOptions (unchanged; the snapshot lists this member under grit_lib::prelude::RepositoryOptions)` | use `InitOptions::with_repository_options(RepositoryOptions::with_environment(env))` |
| `grit_lib::config::environment::RepositoryOptions::with_hook_stdio` | `grit_lib::config::environment::RepositoryOptions (unchanged; the snapshot lists this member under grit_lib::prelude::RepositoryOptions)` |  |
| `grit_lib::config::environment::RepositoryOptions::with_initial_branch` | `grit_lib::config::environment::RepositoryOptions (unchanged; the snapshot lists this member under grit_lib::prelude::RepositoryOptions)` |  |
| `grit_lib::config::environment::RepositoryOptions::with_network_trace` | `grit_lib::config::environment::RepositoryOptions (unchanged; the snapshot lists this member under grit_lib::prelude::RepositoryOptions)` |  |
| `grit_lib::config::environment::RepositoryOptions::with_object_store` | `grit_lib::config::environment::RepositoryOptions (unchanged; the snapshot lists this member under grit_lib::prelude::RepositoryOptions)` |  |
| `grit_lib::config::environment::RepositoryOptions::with_odb_builder` | `grit_lib::config::environment::RepositoryOptions (unchanged; the snapshot lists this member under grit_lib::prelude::RepositoryOptions)` |  |
| `grit_lib::config::environment::RepositoryOptions::with_ownership_check` | `grit_lib::config::environment::RepositoryOptions (unchanged; the snapshot lists this member under grit_lib::prelude::RepositoryOptions)` |  |
| `grit_lib::config::environment::RepositoryOptions::with_promisor_fetcher` | `grit_lib::config::environment::RepositoryOptions (unchanged; the snapshot lists this member under grit_lib::prelude::RepositoryOptions)` |  |
| `grit_lib::config::environment::RepositoryOptions::with_ref_store` | `grit_lib::config::environment::RepositoryOptions (unchanged; the snapshot lists this member under grit_lib::prelude::RepositoryOptions)` |  |
| `grit_lib::config::environment::RepositoryOptions::with_reftable_write` | `grit_lib::config::environment::RepositoryOptions (unchanged; the snapshot lists this member under grit_lib::prelude::RepositoryOptions)` |  |
| `grit_lib::config::environment::RepositoryOptions::with_work_tree` | `grit_lib::config::environment::RepositoryOptions (unchanged; the snapshot lists this member under grit_lib::prelude::RepositoryOptions)` |  |
| `grit_lib::config::environment::RepositoryOptions::without_format_validation` | `grit_lib::config::environment::RepositoryOptions (unchanged; the snapshot lists this member under grit_lib::prelude::RepositoryOptions)` |  |
| `grit_lib::config::git_date::TzHhmm` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::config::git_date::approx::approxidate_careful` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::config::git_date::local_tzoffset` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::config::git_date::show_date_relative` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::config::gitmodules::ATTR_MAX_FILE_SIZE` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::config::gitmodules::ATTR_MAX_LINE_LENGTH` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::config::gitmodules::gitdir::registered_submodule_paths` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::config::ident_resolve::GitIdentityNameEnv` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::config::ident_resolve::GitIdentityNameEnv::Set` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::config::ident_resolve::GitIdentityNameEnv::Unset` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::config::ident_resolve::GitIdentityNameEnv::clone` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::config::ident_resolve::GitIdentityNameEnv::eq` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::config::ident_resolve::GitIdentityNameEnv::fmt` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::config::parse_git_signed_config` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::config::remove_config_section` | `grit_lib::config::ConfigEditor::remove_section` | use ConfigEditor on a repository config snapshot |
| `grit_lib::config::rename_config_section` | `grit_lib::config::ConfigEditor::rename_section` | use ConfigEditor on a repository config snapshot |
| `grit_lib::config::set_config_value` | `grit_lib::config::ConfigEditor::set` | use ConfigEditor on a repository config snapshot |
| `grit_lib::config::unset_all_config_values` | `grit_lib::config::ConfigEditor::unset_all` | use ConfigEditor on a repository config snapshot |
| `grit_lib::config::unset_config_value` | `grit_lib::config::ConfigEditor::unset` | use ConfigEditor on a repository config snapshot |
| `grit_lib::diagnostics::fatal_line` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::diff::DiffAlgorithm::similar_algorithm` | `core::str::FromStr::from_str (DiffAlgorithm)` |  |
| `grit_lib::diff::anchored_unified_diff` | `grit_lib::diff::unified_diff` |  |
| `grit_lib::diff::annotate_unified_diff_no_newline_at_eof` | `grit_lib::diff::structured (hunk metadata)` |  |
| `grit_lib::diff::bytes_for_diff` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::diff::combined_structured_patch_body` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::diff::combined_tree_diff::combined_diff_paths_trees` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::diff::combined_tree_diff::combined_parent_status_char` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::diff::combined_tree_diff::combined_raw_meta` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::diff::count_changes_with_algorithm` | `grit_lib::diff::Diff::files` |  |
| `grit_lib::diff::line_diff_unified_hunk_body` | `grit_lib::diff::unified_diff` |  |
| `grit_lib::diff::load_odb_blob_side_for_diff` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::diff::normalize_ignore_space_change_line` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::diff::peer_scope_includes_path` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::diff::rerere::RerereAutoupdate::clone` | `grit_lib::diff::rerere::RerereAutoupdate (unchanged; the snapshot lists this member under grit_lib::merge::RerereAutoupdate)` |  |
| `grit_lib::diff::rerere::RerereAutoupdate::default` | `grit_lib::diff::rerere::RerereAutoupdate (unchanged; the snapshot lists this member under grit_lib::merge::RerereAutoupdate)` |  |
| `grit_lib::diff::rerere::RerereAutoupdate::eq` | `grit_lib::diff::rerere::RerereAutoupdate (unchanged; the snapshot lists this member under grit_lib::merge::RerereAutoupdate)` |  |
| `grit_lib::diff::rerere::RerereAutoupdate::fmt` | `grit_lib::diff::rerere::RerereAutoupdate (unchanged; the snapshot lists this member under grit_lib::merge::RerereAutoupdate)` |  |
| `grit_lib::diff::rerere::RerereEvent::clone` | `grit_lib::diff::rerere::RerereEvent (unchanged; the snapshot lists this member under grit_lib::merge::RerereEvent)` |  |
| `grit_lib::diff::rerere::RerereEvent::eq` | `grit_lib::diff::rerere::RerereEvent (unchanged; the snapshot lists this member under grit_lib::merge::RerereEvent)` |  |
| `grit_lib::diff::rerere::RerereEvent::fmt` | `grit_lib::diff::rerere::RerereEvent (unchanged; the snapshot lists this member under grit_lib::merge::RerereEvent)` |  |
| `grit_lib::diff::rerere::RerereEventKind::clone` | `grit_lib::diff::rerere::RerereEventKind (unchanged; the snapshot lists this member under grit_lib::merge::RerereEventKind)` |  |
| `grit_lib::diff::rerere::RerereEventKind::eq` | `grit_lib::diff::rerere::RerereEventKind (unchanged; the snapshot lists this member under grit_lib::merge::RerereEventKind)` |  |
| `grit_lib::diff::rerere::RerereEventKind::fmt` | `grit_lib::diff::rerere::RerereEventKind (unchanged; the snapshot lists this member under grit_lib::merge::RerereEventKind)` |  |
| `grit_lib::diff::rerere::rerere_clear` | `grit_lib::merge::Merge::abort (rerere state)` |  |
| `grit_lib::diff::resolve_indent_heuristic` | `grit_lib::diff::DiffOptions::resolve_indent_heuristic` |  |
| `grit_lib::diff::text_sides_differ` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::diff::tree_diff_structured_entry` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::diff::unified_diff_histogram_hunks_only` | `grit_lib::diff::unified_diff` |  |
| `grit_lib::diff::unified_diff_with_prefix_and_funcname` | `grit_lib::diff::unified_diff` |  |
| `grit_lib::diff::unified_diff_with_prefix_and_funcname_and_algorithm` | `grit_lib::diff::unified_diff` |  |
| `grit_lib::error::SparseCheckoutWarning` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::error::SparseCheckoutWarning::DisablingConePatternMatching` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::error::SparseCheckoutWarning::RepeatedPattern` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::error::SparseCheckoutWarning::RepeatedPattern::pattern` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::error::SparseCheckoutWarning::UnrecognizedNegativePattern` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::error::SparseCheckoutWarning::UnrecognizedNegativePattern::pattern` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::error::SparseCheckoutWarning::UnrecognizedPattern` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::error::SparseCheckoutWarning::UnrecognizedPattern::pattern` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::error::SparseCheckoutWarning::body` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::error::SparseCheckoutWarning::clone` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::error::SparseCheckoutWarning::eq` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::error::SparseCheckoutWarning::fmt` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::error::SparseCheckoutWarning::format_line` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::ident::ParsedSignatureTimes` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::ident::ParsedSignatureTimes::clone` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::ident::ParsedSignatureTimes::eq` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::ident::ParsedSignatureTimes::fmt` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::ident::ParsedSignatureTimes::tz_hhmm_range` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::ident::ParsedSignatureTimes::tz_offset_secs` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::ident::ParsedSignatureTimes::unix_seconds` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::ident::SignatureTail` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::ident::SignatureTail::NonNumeric` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::ident::SignatureTail::Overflow` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::ident::SignatureTail::Valid` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::ident::SignatureTail::clone` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::ident::SignatureTail::eq` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::ident::SignatureTail::fmt` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::ident::author_identity_line` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::ident::committer_timestamp_for_until_filter` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::ident::committer_timestamp_for_until_filter_bytes` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::ident::committer_unix_seconds_for_ordering_bytes` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::ident::parse_signature_tail` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::ident::parse_signature_times` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::ident::prepare_commit_identity_line` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::ident::replace_identity_date_suffix` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::ident::signature_timestamp_for_pretty` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::index::IndexStatFields` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::index::IndexStatFields::clone` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::index::IndexStatFields::fmt` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::index::IndexStatFields::from` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::index::IndexStatFields::from_metadata` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::index::IndexStatFields::index_entry` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::lockfile::SystemLockClock` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::lockfile::SystemLockClock::clone` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::lockfile::SystemLockClock::default` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::lockfile::SystemLockClock::fmt` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::lockfile::SystemLockClock::now` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::lockfile::SystemLockClock::sleep` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::notes::apply_prepared_notes_rewrite` | `grit_lib::repo_handles::RepositoryNotes::apply_rewrite_plan` |  |
| `grit_lib::notes::copy_notes_for_rewrite` | `grit_lib::repo_handles::RepositoryNotes::copy_for_rewrite` |  |
| `grit_lib::notes::notes_merge_inner` | `grit_lib::repo_handles::RepositoryNotes::merge_refs` |  |
| `grit_lib::notes::prepare_notes_for_rewrite` | `grit_lib::repo_handles::RepositoryNotes::prepare_rewrite` |  |
| `grit_lib::notes::read_notes_tree` | `grit_lib::repo_handles::RepositoryNotes::read_tree` |  |
| `grit_lib::notes::read_notes_tree_limited` | `grit_lib::repo_handles::RepositoryNotes::read_tree_limited` |  |
| `grit_lib::notes::write_notes_commit` | `grit_lib::repo_handles::RepositoryNotes::write_tree_commit` |  |
| `grit_lib::objects::bitmap::BitmapIndexCache` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::objects::bitmap::BitmapIndexCache::default` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::objects::bitmap::MidxBitmapWriter` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::objects::bitmap::MidxBitmapWriter::write` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::objects::bitmap::MidxBitmapWriter::write_for_midx_layer` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::objects::commit_subject_pretty` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::objects::empty_object_id_hash_set` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::objects::empty_oid_byte_filter_map` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::objects::empty_oid_set` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::objects::insert_header_before_signature` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::objects::iter_extra_headers` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::objects::next_header_block` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::objects::oid_hash_set_from` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::objects::pack::ReuseWriteStats` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::objects::pack::ReuseWriteStats::clone` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::objects::pack::ReuseWriteStats::default` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::objects::pack::ReuseWriteStats::eq` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::objects::pack::ReuseWriteStats::fmt` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::objects::pack::ReuseWriteStats::pack_ofs_fixup_objects` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::objects::pack::ReuseWriteStats::pack_ref_from_ofs_objects` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::objects::pack::ReuseWriteStats::verbatim_reuse_bytes` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::objects::pack::ReuseWriteStats::verbatim_reuse_objects` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::objects::pack::StreamingReuse` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::objects::promisor_remote::PromisorReplyError` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::objects::promisor_remote::PromisorReplyError::Empty` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::objects::promisor_remote::PromisorReplyError::clone` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::objects::promisor_remote::PromisorReplyError::eq` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::objects::promisor_remote::PromisorReplyError::fmt` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::objects::read_commit_subject_pretty` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::objects::remove_header_lines` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::objects::tag_header_field` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::parallel::parallel_worthwhile` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::path_validate::component_is_dotgit_alias` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
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
| `grit_lib::porcelain::checkout::remove_empty_parent_dirs` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::porcelain::checkout_reflog::append_head_checkout_reflog` | `removed (used by Checkout::switch)` |  |
| `grit_lib::porcelain::checkout_reflog::branch_create_reflog_source_label` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::porcelain::checkout_reflog::checkout_reflog_from_label` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::porcelain::checkout_reflog::checkout_reflog_identity` | `removed (used by Checkout::switch)` |  |
| `grit_lib::porcelain::checkout_reflog::create_branch_ref_with_reflog` | `removed (used by Checkout::switch)` |  |
| `grit_lib::porcelain::checkout_reflog::finish_switch_ref_updates` | `removed (used by Checkout::switch)` |  |
| `grit_lib::porcelain::checkout_reflog::is_already_detached_at` | `removed (used by Checkout::switch)` |  |
| `grit_lib::porcelain::checkout_reflog::is_already_on_branch` | `removed (used by Checkout::switch)` |  |
| `grit_lib::porcelain::checkout_reflog::switch_create_reflog_source_label` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::porcelain::cherry_pick::directory_renames_from_file_renames` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::porcelain::clean::clean_untracked` | `grit_lib::repo_handles::Checkout::clean` |  |
| `grit_lib::porcelain::clean::clean_untracked_with_cancel` | `grit_lib::repo_handles::Checkout::clean` |  |
| `grit_lib::porcelain::commit::commit_with_patch` | `grit_lib::commits::Commits::create` |  |
| `grit_lib::porcelain::commit::commit_with_scope` | `grit_lib::commits::Commits::create` |  |
| `grit_lib::porcelain::commit::commit_worktree` | `grit_lib::commits::Commits::create` |  |
| `grit_lib::porcelain::commit::create_commit` | `grit_lib::commits::Commits::create` |  |
| `grit_lib::porcelain::commit::default_commit_scope` | `grit_lib::commits::Commits::default_scope` |  |
| `grit_lib::porcelain::commit::default_commit_scope_from_config` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::porcelain::commit::pending_commit_message` | `grit_lib::commits::Commits::pending_message` |  |
| `grit_lib::porcelain::commit::plan_commit` | `grit_lib::commits::Commits::plan` |  |
| `grit_lib::porcelain::commit::prepare_commit_bytes` | `grit_lib::commits::Commits::prepare_bytes` |  |
| `grit_lib::porcelain::commit::write_commit_object` | `grit_lib::commits::Commits::write_object` |  |
| `grit_lib::porcelain::hunk_select::WorktreeReverseApplyOptions` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::porcelain::hunk_select::WorktreeReverseApplyOptions::clone` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::porcelain::hunk_select::WorktreeReverseApplyOptions::default` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::porcelain::hunk_select::WorktreeReverseApplyOptions::dry_run` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::porcelain::hunk_select::WorktreeReverseApplyOptions::eq` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::porcelain::hunk_select::WorktreeReverseApplyOptions::fmt` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::porcelain::hunk_select::diff_options_for_hunk_select` | `removed (use DiffOptions)` |  |
| `grit_lib::porcelain::hunk_select::list_unstaged_hunk_ids` | `grit_lib::index::IndexFile::list_unstaged_hunk_ids` |  |
| `grit_lib::porcelain::hunk_select::resolve_selection` | `grit_lib::index::IndexFile::resolve_hunk_selection` |  |
| `grit_lib::porcelain::hunk_select::restore_hunks` | `grit_lib::index::IndexFile::restore_hunks` |  |
| `grit_lib::porcelain::hunk_select::reverse_apply_patches_to_worktree` | `removed (use IndexFile::restore_hunks)` |  |
| `grit_lib::porcelain::hunk_select::stage_hunks` | `grit_lib::index::IndexFile::stage_hunks` |  |
| `grit_lib::porcelain::hunk_select::unstaged_file_diffs` | `grit_lib::index::IndexFile::unstaged_file_diffs` |  |
| `grit_lib::porcelain::index_mode::set_stage0_file_mode` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::porcelain::paths::move_path` | `grit_lib::repo_handles::Checkout::move_path` |  |
| `grit_lib::porcelain::paths::move_paths` | `grit_lib::repo_handles::Checkout::move_paths` |  |
| `grit_lib::porcelain::paths::remove_paths` | `grit_lib::repo_handles::Checkout::remove_paths` |  |
| `grit_lib::porcelain::pathspec_match::ensure_pathspec_matches_known_in_repository` | `grit_lib::index::IndexFile::ensure_pathspec_matches_known` |  |
| `grit_lib::porcelain::pathspec_match::pathspec_matches_known_in_repository` | `grit_lib::index::IndexFile::pathspec_matches_known` |  |
| `grit_lib::porcelain::quote_path::quote_c_style_bytes` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::porcelain::reset::preview_reset` | `grit_lib::repo_handles::Checkout::preview_reset` |  |
| `grit_lib::porcelain::reset::reset_to_commit` | `grit_lib::repo_handles::Checkout::reset` |  |
| `grit_lib::porcelain::restore::conflict_style_from_config` | `grit_lib::repo_handles::Checkout::conflict_style` |  |
| `grit_lib::porcelain::restore::restore_paths` | `grit_lib::repo_handles::Checkout::restore` |  |
| `grit_lib::porcelain::revert::merge_commit_message_for_revert` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::porcelain::revert::revert_mainline_extra_body` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::porcelain::stage_tracked::replay_deferred_untracked_invalidations` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
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
| `grit_lib::porcelain::status::normalize_status_conflicts` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::porcelain::status::status` | `grit_lib::status::Status::run` |  |
| `grit_lib::porcelain::status::status_diff_entry_matches_pathspec` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::porcelain::status::status_diff_entry_matches_pathspec_typed` | `grit_lib::status::Status::diff_entry_matches_pathspec` |  |
| `grit_lib::porcelain::status::status_renames_from_config` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::porcelain::status::untracked_mode_from_config` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
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
| `grit_lib::porcelain::tag::normalize_tag_message` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::porcelain::tag::strip_comments` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::porcelain::tag::tag_ref_metadata` | `grit_lib::repo_handles::Tags::ref_metadata` |  |
| `grit_lib::porcelain::tag::verify_tag_by_name` | `grit_lib::repo_handles::Tags::verify` |  |
| `grit_lib::porcelain::workspace_snapshot::capture_workspace_snapshot` | `grit_lib::repo_handles::RepositoryWorkspaceSnapshot::capture` |  |
| `grit_lib::porcelain::workspace_snapshot::pin_workspace_snapshot` | `grit_lib::repo_handles::RepositoryWorkspaceSnapshot::pin` |  |
| `grit_lib::porcelain::workspace_snapshot::restore_workspace_snapshot` | `grit_lib::repo_handles::RepositoryWorkspaceSnapshot::restore` |  |
| `grit_lib::porcelain::workspace_snapshot::unpin_workspace_snapshot` | `grit_lib::repo_handles::RepositoryWorkspaceSnapshot::unpin` |  |
| `grit_lib::porcelain::workspace_snapshot::workspace_snapshot_pin_ref_name` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::porcelain::worktree_guard::TreeSwitchPlan` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::porcelain::worktree_guard::TreeSwitchPlan::tree_changes` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::porcelain::worktree_guard::TreeSwitchPlan::untracked` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::porcelain::worktree_guard::WorktreeSnapshot` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::porcelain::worktree_guard::WorktreeSnapshot::head_tree` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::porcelain::worktree_guard::WorktreeSnapshot::index` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::prelude::InitOptions::environment` | `grit_lib::config::environment::RepositoryOptions::environment` |  |
| `grit_lib::prelude::MidxError::clone` | `grit_lib::prelude::MidxError (unchanged; the snapshot lists this member under grit_lib::error::MidxError)` |  |
| `grit_lib::prelude::MidxError::eq` | `grit_lib::prelude::MidxError (unchanged; the snapshot lists this member under grit_lib::error::MidxError)` |  |
| `grit_lib::prelude::MidxError::fmt` | `grit_lib::prelude::MidxError (unchanged; the snapshot lists this member under grit_lib::error::MidxError)` |  |
| `grit_lib::refs::branch_tracking::apply_push_remote_tracking_updates` | `grit_lib::repo_handles::Branches::apply_push_remote_tracking_updates` |  |
| `grit_lib::refs::branch_tracking::compare_with_target_optional` | `grit_lib::repo_handles::Branches::compare_with_target` |  |
| `grit_lib::refs::branch_tracking::list_branches` | `grit_lib::repo_handles::Branches::list` |  |
| `grit_lib::refs::branch_tracking::status_integration_hint` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::refs::branch_tracking::upstream_status` | `grit_lib::repo_handles::Branches::upstream_status` |  |
| `grit_lib::refs::branch_tracking::upstream_tracking_full_ref` | `grit_lib::repo_handles::Branches::upstream_tracking_full_ref` |  |
| `grit_lib::refs::check_ref_format::collapse_slashes` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::refs::listing::pattern_matches_ls_remote_pattern` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::refs::listing::patterns_match_ls_remote` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::refs::query_reachability::ref_points_at_any` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::refs::reflog::MessageMatcher::clone` | `grit_lib::refs::reflog::MessageMatcher (unchanged; the snapshot lists this member under grit_lib::repo_handles::MessageMatcher)` |  |
| `grit_lib::refs::reflog::MessageMatcher::eq` | `grit_lib::refs::reflog::MessageMatcher (unchanged; the snapshot lists this member under grit_lib::repo_handles::MessageMatcher)` |  |
| `grit_lib::refs::reflog::MessageMatcher::fmt` | `grit_lib::refs::reflog::MessageMatcher (unchanged; the snapshot lists this member under grit_lib::repo_handles::MessageMatcher)` |  |
| `grit_lib::refs::reflog::MessageMatcher::matches` | `grit_lib::refs::reflog::MessageMatcher (unchanged; the snapshot lists this member under grit_lib::repo_handles::MessageMatcher)` |  |
| `grit_lib::refs::reflog::MessageMatcher::new` | `grit_lib::refs::reflog::MessageMatcher (unchanged; the snapshot lists this member under grit_lib::repo_handles::MessageMatcher)` |  |
| `grit_lib::refs::store::backend::expected_matches` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::refs::store::backend::verify_create_casefold_against_existing` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::refs::store::backend::verify_create_casefold_conflicts` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::refs::store::backend::verify_create_casefold_same_batch` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::refs::store::backend::verify_create_conflicts` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::refs::store::paths::packed_refs_rel` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::remote::transport::http::ureq_client::UreqHttpClient::default` | `grit_lib::remote::transport::http::ureq_client::UreqHttpClient (unchanged; the snapshot lists this member under grit_lib::remote::http_client::UreqHttpClient)` |  |
| `grit_lib::remote::transport::http::ureq_client::UreqHttpClient::from_config` | `grit_lib::remote::transport::http::ureq_client::UreqHttpClient (unchanged; the snapshot lists this member under grit_lib::remote::http_client::UreqHttpClient)` |  |
| `grit_lib::remote::transport::http::ureq_client::UreqHttpClient::get` | `grit_lib::remote::transport::http::ureq_client::UreqHttpClient (unchanged; the snapshot lists this member under grit_lib::remote::http_client::UreqHttpClient)` |  |
| `grit_lib::remote::transport::http::ureq_client::UreqHttpClient::get_following_redirects` | `grit_lib::remote::transport::http::ureq_client::UreqHttpClient (unchanged; the snapshot lists this member under grit_lib::remote::http_client::UreqHttpClient)` |  |
| `grit_lib::remote::transport::http::ureq_client::UreqHttpClient::git_protocol_header` | `grit_lib::remote::transport::http::ureq_client::UreqHttpClient (unchanged; the snapshot lists this member under grit_lib::remote::http_client::UreqHttpClient)` |  |
| `grit_lib::remote::transport::http::ureq_client::UreqHttpClient::low_speed_stall_window` | `grit_lib::remote::transport::http::ureq_client::UreqHttpClient (unchanged; the snapshot lists this member under grit_lib::remote::http_client::UreqHttpClient)` |  |
| `grit_lib::remote::transport::http::ureq_client::UreqHttpClient::new` | `grit_lib::remote::transport::http::ureq_client::UreqHttpClient (unchanged; the snapshot lists this member under grit_lib::remote::http_client::UreqHttpClient)` |  |
| `grit_lib::remote::transport::http::ureq_client::UreqHttpClient::post` | `grit_lib::remote::transport::http::ureq_client::UreqHttpClient (unchanged; the snapshot lists this member under grit_lib::remote::http_client::UreqHttpClient)` |  |
| `grit_lib::remote::transport::http::ureq_client::UreqHttpClient::post_into_reader` | `grit_lib::remote::transport::http::ureq_client::UreqHttpClient (unchanged; the snapshot lists this member under grit_lib::remote::http_client::UreqHttpClient)` |  |
| `grit_lib::remote::transport::http::ureq_client::UreqHttpClient::post_streaming` | `grit_lib::remote::transport::http::ureq_client::UreqHttpClient (unchanged; the snapshot lists this member under grit_lib::remote::http_client::UreqHttpClient)` |  |
| `grit_lib::remote::transport::http::ureq_client::UreqHttpClient::reset_auth_after_redirect_rebase` | `grit_lib::remote::transport::http::ureq_client::UreqHttpClient (unchanged; the snapshot lists this member under grit_lib::remote::http_client::UreqHttpClient)` |  |
| `grit_lib::remote::transport::http::ureq_client::UreqHttpClient::send` | `grit_lib::remote::transport::http::ureq_client::UreqHttpClient (unchanged; the snapshot lists this member under grit_lib::remote::http_client::UreqHttpClient)` |  |
| `grit_lib::remote::transport::http::ureq_client::UreqHttpClient::set_operation_cancel` | `grit_lib::remote::transport::http::ureq_client::UreqHttpClient (unchanged; the snapshot lists this member under grit_lib::remote::http_client::UreqHttpClient)` |  |
| `grit_lib::remote::transport::http::ureq_client::UreqHttpClient::smart_http_enabled` | `grit_lib::remote::transport::http::ureq_client::UreqHttpClient (unchanged; the snapshot lists this member under grit_lib::remote::http_client::UreqHttpClient)` |  |
| `grit_lib::remote::transport::http::ureq_client::UreqHttpClient::with_credential_provider` | `grit_lib::remote::transport::http::ureq_client::UreqHttpClient (unchanged; the snapshot lists this member under grit_lib::remote::http_client::UreqHttpClient)` |  |
| `grit_lib::remote::transport::http::ureq_client::UreqHttpClient::with_credentials` | `grit_lib::remote::transport::http::ureq_client::UreqHttpClient (unchanged; the snapshot lists this member under grit_lib::remote::http_client::UreqHttpClient)` |  |
| `grit_lib::remote::transport::http::ureq_client::UreqHttpClient::with_git_protocol` | `grit_lib::remote::transport::http::ureq_client::UreqHttpClient (unchanged; the snapshot lists this member under grit_lib::remote::http_client::UreqHttpClient)` |  |
| `grit_lib::remote::transport::http::ureq_client::UreqHttpClient::with_user_agent` | `grit_lib::remote::transport::http::ureq_client::UreqHttpClient (unchanged; the snapshot lists this member under grit_lib::remote::http_client::UreqHttpClient)` |  |
| `grit_lib::repo::InitOptions::environment` | `grit_lib::config::environment::RepositoryOptions::environment` |  |
| `grit_lib::repo::InitOptions::with_environment` | `grit_lib::repo::InitOptions::with_repository_options` | use `InitOptions::with_repository_options(RepositoryOptions::with_environment(env))` |
| `grit_lib::repo::early_config_ignore_repo_reason_for_text` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::repo::worktree_config_enabled_for_text` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::repo_handles::configured_remote_names` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::repo_handles::configured_remote_names_in_config_order` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::repo_handles::configured_remotes` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::repo_handles::remote_skip_fetch_all` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::revwalk::history::DEFAULT_ABSORB_STACK_LIMIT` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::revwalk::history::commit_message_subject_line` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::revwalk::history::resolve_rebase_use_fork_point` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::revwalk::rev_list::CommitExtent::clone` | `grit_lib::revwalk::rev_list::CommitExtent (unchanged; the snapshot lists this member under grit_lib::revwalk::CommitExtent)` |  |
| `grit_lib::revwalk::rev_list::CommitExtent::default` | `grit_lib::revwalk::rev_list::CommitExtent (unchanged; the snapshot lists this member under grit_lib::revwalk::CommitExtent)` |  |
| `grit_lib::revwalk::rev_list::CommitExtent::eq` | `grit_lib::revwalk::rev_list::CommitExtent (unchanged; the snapshot lists this member under grit_lib::revwalk::CommitExtent)` |  |
| `grit_lib::revwalk::rev_list::CommitExtent::fmt` | `grit_lib::revwalk::rev_list::CommitExtent (unchanged; the snapshot lists this member under grit_lib::revwalk::CommitExtent)` |  |
| `grit_lib::revwalk::rev_list::shallow_boundary_oids` | `grit_lib::revwalk::handle::RevWalk::shallow_boundary_oids` |  |
| `grit_lib::revwalk::rev_list::url_encode_object_filter_subspec` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::revwalk::rev_parse::IndexPathEntry` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::revwalk::rev_parse::IndexPathEntry::clone` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::revwalk::rev_parse::IndexPathEntry::eq` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::revwalk::rev_parse::IndexPathEntry::fmt` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::revwalk::rev_parse::IndexPathEntry::mode` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::revwalk::rev_parse::IndexPathEntry::oid` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::revwalk::rev_parse::IndexPathEntry::path` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::revwalk::rev_parse::load_graft_parents` | `removed (crate-internal graft helpers)` |  |
| `grit_lib::revwalk::rev_parse::superproject_work_tree_from_nested_git_modules` | `removed (crate-internal submodule helper)` |  |
| `grit_lib::stash::FlatTreeEntry]` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::stash::add_stage_entry` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::stash::remove_empty_dirs` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::stash::write_regular_file_replacing_symlink` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::tree_sha256::path_matches_glob_bytes` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::BranchWorktreeCheckout` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::BranchWorktreeCheckout::admin_dir` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::BranchWorktreeCheckout::branch` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::BranchWorktreeCheckout::clone` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::BranchWorktreeCheckout::eq` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::BranchWorktreeCheckout::fmt` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::BranchWorktreeCheckout::worktree_path` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::LockWorktreeOptions::clone` | `grit_lib::worktree::LockWorktreeOptions (unchanged; the snapshot lists this member under grit_lib::repo_handles::LockWorktreeOptions)` |  |
| `grit_lib::worktree::LockWorktreeOptions::default` | `grit_lib::worktree::LockWorktreeOptions (unchanged; the snapshot lists this member under grit_lib::repo_handles::LockWorktreeOptions)` |  |
| `grit_lib::worktree::LockWorktreeOptions::fmt` | `grit_lib::worktree::LockWorktreeOptions (unchanged; the snapshot lists this member under grit_lib::repo_handles::LockWorktreeOptions)` |  |
| `grit_lib::worktree::add_linked_worktree` | `grit_lib::repo_handles::RepositoryWorktrees::add` |  |
| `grit_lib::worktree::add_linked_worktree_with_cancel` | `grit_lib::repo_handles::RepositoryWorktrees::add_with_cancel` |  |
| `grit_lib::worktree::allocate_worktree_admin_dir` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::common_git_dir` | `grit_lib::repo::Repository::common_git_dir` |  |
| `grit_lib::worktree::copy_filtered_worktree_config` | `removed (crate-internal worktree setup)` |  |
| `grit_lib::worktree::crlf::AttrRule` | `grit_lib::worktree::attributes::AttrRule` |  |
| `grit_lib::worktree::crlf::AttrRule::attr_base` | `grit_lib::worktree::attributes::AttrRule::attr_base` |  |
| `grit_lib::worktree::crlf::AttrRule::attrs` | `grit_lib::worktree::attributes::AttrRule::attrs` |  |
| `grit_lib::worktree::crlf::AttrRule::line` | `grit_lib::worktree::attributes::AttrRule::line` |  |
| `grit_lib::worktree::crlf::AttrRule::nowildcardlen` | `grit_lib::worktree::attributes::AttrRule::nowildcardlen` |  |
| `grit_lib::worktree::crlf::AttrRule::pattern` | `grit_lib::worktree::attributes::AttrRule::pattern` |  |
| `grit_lib::worktree::crlf::AttrRule::pattern_flags` | `grit_lib::worktree::attributes::AttrRule::pattern_flags` |  |
| `grit_lib::worktree::crlf::AttrRule::skip` | `grit_lib::worktree::attributes::AttrRule::skip` |  |
| `grit_lib::worktree::crlf::GitAttributes` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::crlf::WorkTreeEncodingError` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::crlf::WorkTreeEncodingError::UnsupportedEncoding` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::crlf::WorkTreeEncodingError::UnsupportedEncoding::label` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::crlf::WorkTreeEncodingError::clone` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::crlf::WorkTreeEncodingError::eq` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::crlf::WorkTreeEncodingError::fmt` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::ensure_branch_not_checked_out_elsewhere` | `grit_lib::repo_handles::RepositoryWorktrees::ensure_branch_not_checked_out_elsewhere` |  |
| `grit_lib::worktree::ensure_branch_not_checked_out_in_any_worktree` | `grit_lib::repo_handles::RepositoryWorktrees::ensure_branch_not_checked_out_in_any_worktree` |  |
| `grit_lib::worktree::find_worktree_with_branch_checked_out` | `grit_lib::repo_handles::RepositoryWorktrees::find_with_branch_checked_out` |  |
| `grit_lib::worktree::fsmonitor::FsmonitorRefreshOptions` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::fsmonitor::FsmonitorRefreshOptions::clone` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::fsmonitor::FsmonitorRefreshOptions::fmt` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::fsmonitor::FsmonitorRefreshOptions::query_boundary` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::fsmonitor::FsmonitorRefreshOptions::with_query_boundary` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::fsmonitor::FsmonitorRefreshResult` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::fsmonitor::FsmonitorRefreshResult::clone` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::fsmonitor::FsmonitorRefreshResult::eq` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::fsmonitor::FsmonitorRefreshResult::fmt` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::fsmonitor::FsmonitorRefreshResult::honor_valid_bits` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::fsmonitor::FsmonitorRefreshResult::index_changed` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::fsmonitor::entry_skips_worktree_stat` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::fsmonitor::fsmonitor_enabled` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::fsmonitor::git_style_windows_pipe_name_from_absolute_ipc` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::fsmonitor::refresh_index` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::fsmonitor::v1_hook_token` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::is_bare_repository` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::list_worktrees` | `grit_lib::repo_handles::RepositoryWorktrees::list` |  |
| `grit_lib::worktree::move_linked_worktree` | `grit_lib::repo_handles::RepositoryWorktrees::move_linked` |  |
| `grit_lib::worktree::pathspec::PathspecMatchContext` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::pathspec::PathspecMatchContext::clone` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::pathspec::PathspecMatchContext::default` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::pathspec::PathspecMatchContext::eq` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::pathspec::PathspecMatchContext::fmt` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::pathspec::PathspecMatchContext::globals` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::pathspec::PathspecMatchContext::is_directory` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::pathspec::PathspecMatchContext::is_git_submodule` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::pathspec::PathspecMatchContext::precompose_paths` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::pathspec::pathspecs_allow_bloom` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::prune_linked_worktrees` | `grit_lib::repo_handles::RepositoryWorktrees::prune` |  |
| `grit_lib::worktree::read_worktree_path` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::registered_worktree_count` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::remove_linked_worktree` | `grit_lib::repo_handles::RepositoryWorktrees::remove` |  |
| `grit_lib::worktree::repair_worktrees` | `grit_lib::repo_handles::RepositoryWorktrees::repair` |  |
| `grit_lib::worktree::sanitize_worktree_id_component` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |
| `grit_lib::worktree::worktree_admin_dir` | `grit_lib::repo_handles::RepositoryWorktrees::admin_dir` |  |
| `grit_lib::worktree::worktree_path_basename` | `removed (crate-internal in 0.6: no callers outside grit-lib)` |  |

<!-- api-migration:end -->
