---
title: Repository
summary: Open and discover Git repositories with an explicit Environment, read git-dir vs work tree, load config, and handle grit_lib::Error.
---

A `Repository` is the main handle for grit-lib. Use `Repository::git_dir`, `Repository::work_tree`, and `Repository::odb` for paths and the object store; discovery provenance is available through `DiscoveryInfo` via `Repository::discovery_info`. The `Environment` used to open the handle is returned by `Repository::environment`.

## Environment

`Environment` holds discovery and config variables (`GIT_DIR`, `GIT_WORK_TREE`, `GIT_CEILING_DIRECTORIES`, `GIT_CONFIG_*`, home paths, `cwd`, and related fields) without reading the process environment inside the library. Construct one with `Environment::empty()` (defaults only) or `Environment::from_vars()` (parse an iterator of `(key, value)` pairs plus an explicit working directory). See `Environment`.

The `grit` CLI builds an environment from the process in `grit-cli` and passes it into discovery. Embedders should do the same: snapshot the variables you care about once, then call `Repository::discover` or `Repository::open` with `RepositoryOptions`. Create new repositories with `Repository::init` and `InitOptions`. Custom ODB backends use `RepositoryBuilder`.

`ConfigSet::load` takes `&Environment` as its first argument and always parses the cascade from disk (no process-global cache). Repository-scoped memoization lives on each open handle: `Repository::config` returns `Arc<ConfigSet>` backed by `RepoCaches` (config cascade, gitattributes stacks, filter-process drivers, precompose flags, and related state).

## Command runner

Hooks, clean/smudge filters, credential helpers, signing, SSH transport, and similar features spawn subprocesses through a `CommandRunner` stored on the repository (via `RepositoryOptions::command_runner`). The default is `SystemCommandRunner`, which is the only non-test code path that calls `std::process::Command`. Tests and embedders can install `RecordingRunner` or a custom runner to assert argv, environment, and stdin without executing real programs. Hook failures surface as `HookError`; shell filter subprocess failures use `FilterError`.

## Discover vs open

Call `Repository::discover` when you have a working directory and want Git-style upward search. Call `Repository::open` when you already know the git directory; set `RepositoryOptions::work_tree` for non-bare layouts, `RepositoryOptions::for_serving` for smart-HTTP paths, and `RepositoryOptions::without_format_validation` when discovery must open before format checks.

Bare repositories return `None` from `Repository::work_tree`. Linked worktrees and gitfile indirection are handled during discovery so `Repository::git_dir` always points at the directory that contains `objects/`.

## Config

`Repository::config` returns a lazily loaded snapshot of the merged cascade (system / global / local / worktree / environment overrides). Prefer it on hot paths instead of calling `ConfigSet::load` repeatedly. Keys use Git’s dotted names (`user.name`, `core.bare`, …).

## Typed handles

Core engine areas use thin handles on `Repository` (no duplicated logic):

| Method | Handle | Role |
| --- | --- | --- |
| `objects` | `Objects` | read/write/contains for the ODB |
| `references` | `References` | resolve, list, update refs; read reflog |
| `reflogs` | `RepositoryReflogs` | list reflog refs, show/delete/expire entries |
| `index_handle` | `IndexHandle` | load/write the index |
| `config_handle` | `ConfigHandle` | merged get; local set + reload |
| `rev_walk` | `RevWalk` | rev-parse specs; rev-list walks |
| `diff` | `Diff` | tree↔tree, index↔tree, worktree↔index |

`Repository::refs` still returns the pluggable `RefStore` trait object for embedders and low-level ref backends.

Larger subsystems use [`repo_handles`](rustdoc:grit_lib::repo_handles):

| Method | Handle | Role |
| --- | --- | --- |
| `Repository::remotes` | `RepositoryRemotes` | List/get remotes, `ls-remote`, fetch, push |
| `Repository::worktrees` | `RepositoryWorktrees` | List/add/remove/lock/unlock linked worktrees |
| `Repository::stash` | `RepositoryStash` | Stash list/push/apply/pop/drop |
| `Repository::notes` | `RepositoryNotes` | Notes list/read/add/remove via `NotesRef` |
| `Repository::bundles` | `RepositoryBundles` | Bundle create/verify/list-heads/unbundle |

Transport options, stash identity, and notes author/committer lines are passed in as arguments on those methods (no hidden wall-clock reads in the handle layer).

## Errors

Library operations return `grit_lib::error::Result`. The `Error` enum covers I/O, missing objects, bad repository layout, and invalid user input. Match on variants in application code; the `grit` CLI maps them to exit codes and messages separately.

## Example

The program below discovers a repository (or opens `.git` in the current directory), prints paths, and reads `user.name` from config:

<!-- include: grit-examples/src/bin/guide_repository.rs -->

Run from any Git checkout:

```bash
cargo run --bin guide_repository
```
