---
title: Repository
summary: Open and discover Git repositories with an explicit Environment, read git-dir vs work tree, load config, and handle grit_lib::Error.
---

A [`Repository`](rustdoc:grit_lib::repo::Repository) is the main handle for grit-lib. It carries the absolute [`git_dir`](rustdoc:grit_lib::repo::Repository) path, an optional [`work_tree`](rustdoc:grit_lib::repo::Repository) for non-bare repos, an [`Odb`](rustdoc:grit_lib::odb::Odb) for object reads and writes, and the [`Environment`](rustdoc:grit_lib::environment::Environment) used to discover or open it.

## Environment

[`Environment`](rustdoc:grit_lib::environment::Environment) holds discovery and config variables (`GIT_DIR`, `GIT_WORK_TREE`, `GIT_CEILING_DIRECTORIES`, `GIT_CONFIG_*`, home paths, `cwd`, and related fields) without reading the process environment inside the library. Construct one with `Environment::empty()` (defaults only) or `Environment::from_vars()` (parse an iterator of `(key, value)` pairs plus an explicit working directory). See [`Environment`](rustdoc:grit_lib::environment::Environment).

The `grit` CLI builds an environment from the process in `grit-cli` and passes it into discovery. Embedders should do the same: snapshot the variables you care about once, then call `Repository::discover_with` or `Repository::open_with` with [`RepositoryOptions`](rustdoc:grit_lib::environment::RepositoryOptions).

[`Repository::discover`](rustdoc:grit_lib::repo::Repository) and [`Repository::open`](rustdoc:grit_lib::repo::Repository) remain convenience entry points that use `Environment::empty()` (no overrides beyond `cwd = "."`).

[`ConfigSet::load`](rustdoc:grit_lib::config::ConfigSet) takes `&Environment` as its first argument so config caching and global/system file resolution match the same snapshot as discovery.

## Discover vs open

Call `Repository::discover_with` when you have a working directory and want Git-style upward search. Call `Repository::open_with` when you already know the git directory and optionally the work tree path. See [`Repository`](rustdoc:grit_lib::repo::Repository).

Bare repositories have `work_tree: None`. Linked worktrees and gitfile indirection are handled during discovery so `git_dir` always points at the directory that contains `objects/`.

## Config

[`Repository::config`](rustdoc:grit_lib::repo::Repository) returns a lazily loaded snapshot of the merged cascade (system / global / local / worktree / environment overrides). Prefer it on hot paths instead of calling `ConfigSet::load` repeatedly. Keys use Git’s dotted names (`user.name`, `core.bare`, …).

## Errors

Library operations return [`grit_lib::error::Result`](rustdoc:grit_lib::error::Result). The [`Error`](rustdoc:grit_lib::error::Error) enum covers I/O, missing objects, bad repository layout, and invalid user input. Match on variants in application code; the `grit` CLI maps them to exit codes and messages separately.

## Example

The program below discovers a repository (or opens `.git` in the current directory), prints paths, and reads `user.name` from config:

<!-- include: grit-examples/src/bin/guide_repository.rs -->

Run from any Git checkout:

```
cargo run --bin guide_repository
```
