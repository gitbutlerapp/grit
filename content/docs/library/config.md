---
title: Config
summary: Merged config reads and repository-local writes through ConfigHandle.
---

Configuration in grit-lib follows Git’s cascade (system, global, local, worktree, and environment overrides). Prefer [`Repository::config_handle`](rustdoc:grit_lib::repo::Repository) for hot paths: it returns [`ConfigHandle`](rustdoc:grit_lib::config::handle::ConfigHandle), which reads the same merged snapshot as [`Repository::config`](rustdoc:grit_lib::repo::Repository) and can update the repository-local file.

## Loading outside a handle

[`ConfigSet::load`](rustdoc:grit_lib::config::ConfigSet) parses the cascade from disk given an [`Environment`](rustdoc:grit_lib::config::environment::Environment) and optional git directory. The library does not read `std::env` during load — embedders pass variables explicitly (see [Embedding](../embedding/) and [Repository](../repository/)).

## Local writes

[`ConfigHandle::set_local`](rustdoc:grit_lib::config::handle::ConfigHandle) writes one key to the repository-local config (or `commondir/config` for linked worktrees without a per-worktree config file), then reloads the in-memory snapshot on the open [`Repository`](rustdoc:grit_lib::repo::Repository).

## Example

<!-- include: grit-examples/src/bin/guide_config.rs -->

```bash
cargo run --bin guide_config /path/to/repo
git -C /path/to/repo config user.name
```

The integration test checks the printed `user.name` against `git config user.name`.
