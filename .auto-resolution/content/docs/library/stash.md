---
title: Stash
summary: Stash stack read/write through RepositoryStash and porcelain stash types.
---

The stash stack is Git-compatible ref and commit machinery under [`stash`](rustdoc:grit_lib::stash) and [`porcelain::stash`](rustdoc:grit_lib::porcelain::stash). For embedders, [`Repository::stash`](rustdoc:grit_lib::repo::Repository) returns [`RepositoryStash`](rustdoc:grit_lib::repo_handles::RepositoryStash): push, list, apply, pop, and drop with explicit author/committer identity strings (no hidden wall-clock reads in the handle layer).

## Push and list

[`StashPushOptions`](rustdoc:grit_lib::repo_handles::StashPushOptions) carries the message, whether to include untracked files, and the identity line used for stash commits. [`RepositoryStash::push`](rustdoc:grit_lib::repo_handles::RepositoryStash) returns `Ok(None)` when there is nothing to stash.

## Example

<!-- include: grit-examples/src/bin/guide_stash.rs -->

After a dirty file and `cargo run --bin guide_stash`, `git stash list` must show the entry. The test also runs `git fsck --strict`.
