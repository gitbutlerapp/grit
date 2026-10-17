---
title: Maintenance
summary: Object fsck validation and repository hooks.
---

Repository maintenance helpers live in [`maintenance`](rustdoc:grit_lib::maintenance):

- `Repository::write_commit_graph` — atomic monolithic `objects/info/commit-graph` with optional changed-path Bloom filters (`CommitGraphOptions`, `CommitGraphWriteReport`).
- `Repository::write_midx` — write `multi-pack-index` (`MidxOptions`, `MidxWriteReport`).
- `References::pack` — pack loose refs into `packed-refs` or compact reftable stacks (`PackRefsOptions`).
- `repack_all`, `gc`, and `fsck` orchestrate repack, prune, and validation.

Maintenance APIs split into **repository fsck**, **object validation**, and **hook execution**:

- `maintenance::repository_fsck` — repository-wide integrity checks (loose/pack objects, connectivity from `reachability_roots`, dangling objects, refs, MIDX, commit-graph). Progress via `ProgressSink`; severity follows Git defaults and `fsck.<msg-id>` config.
- `maintenance::fsck` — validate loose object bytes before hashing (`fsck_object`, `FsckError` with Git-compatible message ids). Connectivity walks for receive-pack and bundles are part of the maintenance fsck pipeline.
- `maintenance::repack` — all-into-one repack using `reachability_roots`, optional `.rev`/`.bitmap` sidecars, pack pruning, and unreachable loose-object pruning via `RepackOptions` / `RepackReport`. `Repository::repack` is a thin alias. Incremental repack (`RepackOptions::incremental`) keeps existing packs; when combined with `write_bitmap` and multiple packs, grit writes a multi-pack-index reachability bitmap instead of a single-pack sidecar.
- `hooks` — resolve and run hook scripts through the repository `CommandRunner` (`run_hook`, `HookResult`). `Repository::hooks` lists scripts via `RepositoryHooks::list` (`HookInfo`, `HookSource`). Tests often install `RecordingRunner` via `RepositoryOptions`.
- `patch_ids` — `Repository::patch_id` and `compute_patch_id` for stable/unstable commit equivalence (merge commits return `None`).

The `grit` CLI maps fsck and hook failures to its own exit codes and messages; the library returns typed results only.

## Example

<!-- include: grit-examples/src/bin/guide_maintenance.rs -->

This example fsck-validates `HEAD`’s commit object, installs a `pre-commit` hook that exits 0, and runs it through `run_hook`. The test expects `git fsck --strict` to stay clean afterward.
