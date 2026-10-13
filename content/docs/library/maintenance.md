---
title: Maintenance
summary: Object fsck validation and repository hooks.
---

Maintenance APIs split into **object validation** and **hook execution**:

- [`fsck_standalone`](rustdoc:grit_lib::fsck_standalone) — validate loose object bytes before hashing ([`fsck_object`](rustdoc:grit_lib::fsck_standalone::fsck_object), [`FsckError`](rustdoc:grit_lib::fsck_standalone::FsckError) with Git-compatible message ids). Connectivity walks for receive-pack and bundles live in [`connectivity`](rustdoc:grit_lib::connectivity).
- [`hooks`](rustdoc:grit_lib::hooks) — resolve and run hook scripts through the repository [`CommandRunner`](rustdoc:grit_lib::command_runner::CommandRunner) ([`run_hook`](rustdoc:grit_lib::hooks::run_hook), [`HookResult`](rustdoc:grit_lib::hooks::HookResult)). [`Repository::hooks`](rustdoc:grit_lib::repo::Repository::hooks) lists scripts via [`RepositoryHooks::list`](rustdoc:grit_lib::repo_handles::RepositoryHooks::list) ([`HookInfo`](rustdoc:grit_lib::repo_handles::HookInfo), [`HookSource`](rustdoc:grit_lib::repo_handles::HookSource)). Tests often install [`RecordingRunner`](rustdoc:grit_lib::command_runner::RecordingRunner) via [`RepositoryOptions`](rustdoc:grit_lib::environment::RepositoryOptions).
- [`patch_ids`](rustdoc:grit_lib::diff::patch_ids) — [`Repository::patch_id`](rustdoc:grit_lib::repo::Repository::patch_id) and [`compute_patch_id`](rustdoc:grit_lib::diff::patch_ids::compute_patch_id) for stable/unstable commit equivalence (merge commits return `None`).

The `grit` CLI maps fsck and hook failures to its own exit codes and messages; the library returns typed results only.

## Example

<!-- include: grit-examples/src/bin/guide_maintenance.rs -->

This example fsck-validates `HEAD`’s commit object, installs a `pre-commit` hook that exits 0, and runs it through `run_hook`. The test expects `git fsck --strict` to stay clean afterward.
