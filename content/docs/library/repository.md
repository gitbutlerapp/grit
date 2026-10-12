---
title: Repository
summary: Open and discover Git repositories with an explicit Environment, read git-dir vs work tree, load config, and handle grit_lib::Error.
---

A [`Repository`](rustdoc:grit_lib::repo::Repository) is the main handle for grit-lib. It carries the absolute [`git_dir`](rustdoc:grit_lib::repo::Repository) path, an optional [`work_tree`](rustdoc:grit_lib::repo::Repository) for non-bare repos, an [`Odb`](rustdoc:grit_lib::odb::Odb) for object reads and writes, and the [`Environment`](rustdoc:grit_lib::environment::Environment) used to discover or open it.

## Environment

[`Environment`](rustdoc:grit_lib::environment::Environment) holds discovery and config variables (`GIT_DIR`, `GIT_WORK_TREE`, `GIT_CEILING_DIRECTORIES`, `GIT_CONFIG_*`, home paths, `cwd`, and related fields) without reading the process environment inside the library. Construct one with `Environment::empty()` (defaults only) or `Environment::from_vars()` (parse an iterator of `(key, value)` pairs plus an explicit working directory). See [`Environment`](rustdoc:grit_lib::environment::Environment).

The `grit` CLI builds an environment from the process in `grit-cli` and passes it into discovery. Embedders should do the same: snapshot the variables you care about once, then call `Repository::discover_with` or `Repository::open_with` with [`RepositoryOptions`](rustdoc:grit_lib::environment::RepositoryOptions).

[`Repository::discover`](rustdoc:grit_lib::repo::Repository) and [`Repository::open`](rustdoc:grit_lib::repo::Repository) remain convenience entry points that use `Environment::empty()` (no overrides beyond `cwd = "."`).

[`ConfigSet::load`](rustdoc:grit_lib::config::ConfigSet) takes `&Environment` as its first argument and always parses the cascade from disk (no process-global cache). Repository-scoped memoization lives on each open handle: [`Repository::config`](rustdoc:grit_lib::repo::Repository) returns `Arc<ConfigSet>` backed by [`RepoCaches`](rustdoc:grit_lib::repo_caches::RepoCaches) (config cascade, gitattributes stacks, filter-process drivers, precompose flags, and related state).

## Command runner

Hooks, clean/smudge filters, credential helpers, signing, SSH transport, and similar features spawn subprocesses through a [`CommandRunner`](rustdoc:grit_lib::command_runner::CommandRunner) stored on the repository (via [`RepositoryOptions::command_runner`](rustdoc:grit_lib::environment::RepositoryOptions)). The default is [`SystemCommandRunner`](rustdoc:grit_lib::command_runner::SystemCommandRunner), which is the only non-test code path that calls `std::process::Command`. Tests and embedders can install [`RecordingRunner`](rustdoc:grit_lib::command_runner::RecordingRunner) or a custom runner to assert argv, environment, and stdin without executing real programs. Hook failures surface as [`HookError`](rustdoc:grit_lib::hooks::HookError); shell filter subprocess failures use [`FilterError`](rustdoc:grit_lib::error::FilterError).

## Discover vs open

Call `Repository::discover_with` when you have a working directory and want Git-style upward search. Call `Repository::open_with` when you already know the git directory and optionally the work tree path. See [`Repository`](rustdoc:grit_lib::repo::Repository).

Bare repositories have `work_tree: None`. Linked worktrees and gitfile indirection are handled during discovery so `git_dir` always points at the directory that contains `objects/`.

## Config

[`Repository::config`](rustdoc:grit_lib::repo::Repository) returns a lazily loaded snapshot of the merged cascade (system / global / local / worktree / environment overrides). Prefer it on hot paths instead of calling `ConfigSet::load` repeatedly. Keys use Git’s dotted names (`user.name`, `core.bare`, …).

## Typed domain handles

Beyond [`Repository::refs`](rustdoc:grit_lib::repo::Repository) and the object store on [`Repository::odb`](rustdoc:grit_lib::repo::Repository), the repository exposes scoped handles for larger subsystems:

| Method | Handle | Role |
| --- | --- | --- |
| [`Repository::remotes`](rustdoc:grit_lib::repo::Repository) | [`RepositoryRemotes`](rustdoc:grit_lib::repo_handles::RepositoryRemotes) | List/get remotes, `ls-remote`, fetch, push |
| [`Repository::worktrees`](rustdoc:grit_lib::repo::Repository) | [`RepositoryWorktrees`](rustdoc:grit_lib::repo_handles::RepositoryWorktrees) | List/add/remove/lock linked worktrees |
| [`Repository::stash`](rustdoc:grit_lib::repo::Repository) | [`RepositoryStash`](rustdoc:grit_lib::repo_handles::RepositoryStash) | Stash list/push/apply/pop/drop |
| [`Repository::notes`](rustdoc:grit_lib::repo::Repository) | [`RepositoryNotes`](rustdoc:grit_lib::repo_handles::RepositoryNotes) | Notes list/read/add/remove via [`NotesRef`](rustdoc:grit_lib::notes::NotesRef) |
| [`Repository::bundles`](rustdoc:grit_lib::repo::Repository) | [`RepositoryBundles`](rustdoc:grit_lib::repo_handles::RepositoryBundles) | Bundle create/verify/list-heads/unbundle |

Transport options, stash identity, and notes author/committer lines are passed in as arguments on these methods (no hidden wall-clock reads in the handle layer).

## Errors

Library operations return [`grit_lib::error::Result`](rustdoc:grit_lib::error::Result). The [`Error`](rustdoc:grit_lib::error::Error) enum covers I/O, missing objects, bad repository layout, and invalid user input. Match on variants in application code; the `grit` CLI maps them to exit codes and messages separately.

## Example

The program below discovers a repository (or opens `.git` in the current directory), prints paths, and reads `user.name` from config:

<!-- include: grit-examples/src/bin/guide_repository.rs -->

Run from any Git checkout:

```bash
cargo run --bin guide_repository
```
