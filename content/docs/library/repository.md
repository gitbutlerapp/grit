---
title: Repository
summary: Open and discover Git repositories, read git-dir vs work tree, load config, and handle grit_lib::Error.
---

A [`Repository`](rustdoc:grit_lib::repo::Repository) is the main handle for grit-lib. It carries the absolute [`git_dir`](rustdoc:grit_lib::repo::Repository) path, an optional [`work_tree`](rustdoc:grit_lib::repo::Repository) for non-bare repos, and an [`Odb`](rustdoc:grit_lib::odb::Odb) for object reads and writes.

## Discover vs open

Call [`Repository::discover`](rustdoc:grit_lib::repo::Repository) when you have a working directory and want the same upward search Git uses (respecting `GIT_DIR` and `GIT_WORK_TREE`). Call [`Repository::open`](rustdoc:grit_lib::repo::Repository) when you already know the git directory and optionally the work tree path.

Bare repositories have `work_tree: None`. Linked worktrees and gitfile indirection are handled during discovery so `git_dir` always points at the directory that contains `objects/`.

## Config

Repository code does not cache a full config snapshot on the struct. Load settings when you need them with [`ConfigSet::load`](rustdoc:grit_lib::config::ConfigSet), passing `Some(&repo.git_dir)` for repository-local files. Keys use Git’s dotted names (`user.name`, `core.bare`, …).

## Errors

Library operations return [`grit_lib::error::Result`](rustdoc:grit_lib::error::Result). The [`Error`](rustdoc:grit_lib::error::Error) enum covers I/O, missing objects, bad repository layout, and invalid user input. Match on variants in application code; the `grit` CLI maps them to exit codes and messages separately.

## Example

The program below discovers a repository (or opens `.git` in the current directory), prints paths, and reads `user.name` from config:

<!-- include: grit-examples/src/bin/guide_repository.rs -->

Run from any Git checkout:

```bash
cargo run --bin guide_repository
```
