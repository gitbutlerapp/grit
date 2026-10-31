---
title: Config
summary: Merged config reads and repository-local writes through ConfigHandle.
---

Configuration in grit-lib follows Git’s cascade (system, global, local, worktree, and environment overrides). Prefer `Repository::config_handle` for hot paths: it returns `ConfigHandle`, which reads the same merged snapshot as `Repository::config` and can update the repository-local file.

## Loading outside a handle

[`ConfigSet::load`](rustdoc:grit_lib::config::ConfigSet) parses the cascade from disk given an [`Environment`](rustdoc:grit_lib::config::environment::Environment) and optional git directory. The library does not read `std::env` during load — embedders pass variables explicitly (see [Embedding](../embedding/) and [Repository](../repository/)).

## Local writes

`ConfigHandle::set_local` writes one key to the repository-local config (or `commondir/config` for linked worktrees without a per-worktree config file), then reloads the in-memory snapshot on the open `Repository`.

Without an open `Repository`, find the git directory with [`RepositoryLocation::discover`](rustdoc:grit_lib::repo::RepositoryLocation) and pass it to [`set_config_value`](rustdoc:grit_lib::config::set_config_value) with `ConfigLocation::Local`. Discovery applies the same format and ownership checks as `Repository::discover` and resolves the same work tree. It reads the config cascade once, because `core.bare` and `core.worktree` from any scope affect the layout, but it does not open the object database or the refs. Then only the target file is rewritten. To read config next, call [`RepositoryLocation::discover_with_config`](rustdoc:grit_lib::repo::RepositoryLocation::discover_with_config), which also returns the merged `ConfigSet`. `grit config` uses this path.

Writes go through `config.lock` and an atomic rename and keep the file's permission bits. Like Git, they do not fsync the file, so a crash may lose the update, but readers never see a partial file.

## Example

<!-- include: grit-examples/src/bin/guide_config.rs -->

```bash
cargo run --bin guide_config /path/to/repo
git -C /path/to/repo config user.name
```

The integration test checks the printed `user.name` against `git config user.name`.
