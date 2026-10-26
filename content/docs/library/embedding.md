---
title: Embedding grit-lib
summary: Run multiple repositories in one process with explicit Environment, sinks, subprocess injection, and typed errors.
---

Grit is designed to be **linked into other Rust programs**, not only invoked as the `grit` CLI. The library avoids hidden process globals: discovery variables, config identity, subprocesses, warnings, and wall-clock references are supplied through explicit types at the boundary.

## Environment

[`Environment`](rustdoc:grit_lib::config::environment::Environment) holds Git discovery and configuration variables (`GIT_DIR`, `GIT_WORK_TREE`, `GIT_CONFIG_*`, home paths, author/committer overrides, transport trace flags, and related fields). Construct it with:

- `Environment::empty()` — defaults only (`cwd = "."`).
- [`Environment::from_vars`](rustdoc:grit_lib::config::environment::Environment) — parse an iterator of `(key, value)` pairs plus an explicit working directory (no reads from `std::env` inside the library).
- `Environment::capture_process()` — **CLI boundary only**: snapshot the current process into an [`Environment`](rustdoc:grit_lib::config::environment::Environment).

Pass the same `Environment` into `Repository` (`discover` / `open`) and into `ConfigSet` loading.

## RepositoryOptions

[`RepositoryOptions`](rustdoc:grit_lib::config::environment::RepositoryOptions) bundles everything needed to open a repository besides paths:

| Field | Role |
| --- | --- |
| `environment` | Discovery/config/identity snapshot |
| `command_runner` | Injectable subprocess execution (hooks, filters, signing helpers) |
| `diagnostics` | `DiagnosticSink` for warnings and trace events |
| `network_trace` | When true, emit network trace events via the diagnostics sink |
| `reference_unix_time` | Wall clock for rev-parse date selectors (`@{yesterday}`, etc.) |
| Test knobs | `test_assume_different_owner`, … |

Use `RepositoryOptions::with_environment` and `with_command_runner` on [`RepositoryOptions`](rustdoc:grit_lib::config::environment::RepositoryOptions); assign `diagnostics` when you need a custom sink.

## DiagnosticSink and ProgressSink

Non-fatal conditions are `Warning` values delivered to a `DiagnosticSink`. The default is `NullDiagnostics`. Tests and embedders often use `CollectingDiagnostics` to assert warnings without parsing stderr.

Long-running work (status, staging, checkout and switch, fetch, pack indexing) takes a [`ProgressSink`](rustdoc:grit_lib::progress::ProgressSink). Pass [`NullProgress`](rustdoc:grit_lib::progress::NullProgress) when you don't want progress updates. Progress is separate from diagnostics: progress reports throughput; diagnostics report Git-style warnings.

## CommandRunner

`CommandRunner` is the only production path that spawns OS processes from the library (`SystemCommandRunner`). Hooks, smudge/clean filters, credential helpers, and signing helpers build a `CommandSpec` and call `spawn`.

Shell commands use a `CommandSpec` whose program is `sh`. `SystemCommandRunner` runs that the way Git does: `/bin/sh` on Unix. On Windows it uses the `sh` of the Git for Windows install that owns the `git` on the child's `PATH` (`usr\bin`, then `bin`), then any `sh` on that `PATH`. Filters and helpers therefore work when only Git's `cmd` directory is on `PATH`.

For tests, [`RecordingRunner`](rustdoc:grit_lib::command_runner::RecordingRunner) records specs and returns scripted exit codes. Install it on [`RepositoryOptions`](rustdoc:grit_lib::config::environment::RepositoryOptions) before [`Repository::open`](rustdoc:grit_lib::repo::Repository) or [`Repository::discover`](rustdoc:grit_lib::repo::Repository).

## Typed errors

Library APIs return `grit_lib::error::Result`. Match on `Error` variants (`RevParse`, `RevList`, `HookError`, `FilterError`, …) instead of parsing `"fatal:"` strings. The CLI maps variants to exit codes and human messages in its own words.

Common conditions use dedicated variants rather than `Error::Message`: bare or missing work tree → [`Error::RequiresWorkTree`](rustdoc:grit_lib::error::Error::RequiresWorkTree); empty commit body → [`Error::EmptyCommitMessage`](rustdoc:grit_lib::error::Error::EmptyCommitMessage); path missing from a revision tree → [`RevParseError::PathNotInRevision`](rustdoc:grit_lib::revwalk::rev_parse_error::RevParseError::PathNotInRevision); parent navigation past root → [`RevParseError::RevisionOutOfRange`](rustdoc:grit_lib::revwalk::rev_parse_error::RevParseError::RevisionOutOfRange).

## Concurrency

Two or more `Repository` handles may be used from different threads in one process when each handle is opened with its **own** `Environment`, `RepositoryOptions`, diagnostics sink, and command runner. Repository-scoped caches (config, attributes, pack read caches) do not leak between handles.

Object writes are safe to race. Every loose object is staged in its own uniquely named `objects/xx/tmp_obj_*` file and renamed into place, so handles, threads or processes writing the same object at once all get its id back (whichever rename lands first publishes it, the same as Git). Pack installs are safe to race too, so parallel fetches into one repository all succeed: [`install_pack_path`](rustdoc:grit_lib::objects::index_pack::install_pack_path) stages each install in its own directory, and fetches from a local remote hard-link or copy its packs through a private temp file. Both publish without replacing a `.pack` or `.idx` that is already there under the same content-addressed name (the `.idx` always lands after its `.pack`), and a failed or cancelled install removes only its private staging files, never a published `.pack` or `.idx` another writer may already rely on. Ref updates such as adding a note are compare-and-swap: when two writers race, one wins and the other gets a typed ref-store error instead of losing the first update.

Integration coverage: `grit-lib/tests/concurrent_repos.rs` runs parallel commit loops and a fetch/merge/notes scenario with isolated config and sinks; `grit-lib/tests/loose_write_concurrent_issue_1875.rs` races identical small and large blob writes from separate handles and checks the result with `git fsck --strict`; `grit-lib/tests/index_pack_parallel.rs` installs one pack from several threads into one repository and checks it the same way.

## Example

<!-- include: grit-examples/src/bin/guide_embedding.rs -->

Build and run:

```bash
cargo run -p grit-examples --bin guide_embedding
```
