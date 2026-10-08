---
title: Embedding grit-lib
summary: Run multiple repositories in one process with explicit Environment, sinks, subprocess injection, and typed errors.
---

Grit is designed to be **linked into other Rust programs**, not only invoked as the `grit` CLI. The library avoids hidden process globals: discovery variables, config identity, subprocesses, warnings, and wall-clock references are supplied through explicit types at the boundary.

## Environment

[`Environment`](rustdoc:grit_lib::environment::Environment) holds Git discovery and configuration variables (`GIT_DIR`, `GIT_WORK_TREE`, `GIT_CONFIG_*`, home paths, author/committer overrides, transport trace flags, and related fields). Construct it with:

- `Environment::empty()` — defaults only (`cwd = "."`).
- [`Environment::from_vars`](rustdoc:grit_lib::environment::Environment) — parse an iterator of `(key, value)` pairs plus an explicit working directory (no reads from `std::env` inside the library).
- `Environment::capture_process()` — **CLI boundary only**: snapshot the current process into an [`Environment`](rustdoc:grit_lib::environment::Environment).

Pass the same `Environment` into [`Repository`](rustdoc:grit_lib::repo::Repository) (`discover_with` / `open_with`) and into [`ConfigSet`](rustdoc:grit_lib::config::ConfigSet) loading.

## RepositoryOptions

[`RepositoryOptions`](rustdoc:grit_lib::environment::RepositoryOptions) bundles everything needed to open a repository besides paths:

| Field | Role |
| --- | --- |
| `environment` | Discovery/config/identity snapshot |
| `command_runner` | Injectable subprocess execution (hooks, filters, signing helpers) |
| `diagnostics` | [`DiagnosticSink`](rustdoc:grit_lib::diagnostics::DiagnosticSink) for warnings and trace events |
| `network_trace` | When true, emit network trace events via the diagnostics sink |
| `reference_unix_time` | Wall clock for rev-parse date selectors (`@{yesterday}`, etc.) |
| Test knobs | `test_assume_different_owner`, `force_split_index`, … |

Use `RepositoryOptions::with_environment` and `with_command_runner` on [`RepositoryOptions`](rustdoc:grit_lib::environment::RepositoryOptions); assign `diagnostics` when you need a custom sink.

## DiagnosticSink and ProgressSink

Non-fatal conditions are [`Warning`](rustdoc:grit_lib::diagnostics::Warning) values delivered to a [`DiagnosticSink`](rustdoc:grit_lib::diagnostics::DiagnosticSink). The default is [`NullDiagnostics`](rustdoc:grit_lib::diagnostics::NullDiagnostics). Tests and embedders often use [`CollectingDiagnostics`](rustdoc:grit_lib::diagnostics::CollectingDiagnostics) to assert warnings without parsing stderr.

Long-running work (status, fetch, pack indexing) takes a [`ProgressSink`](rustdoc:grit_lib::progress::ProgressSink) (for example [`NullProgress`](rustdoc:grit_lib::progress::NullProgress)). Progress is separate from diagnostics: progress reports throughput; diagnostics report Git-style warnings.

## CommandRunner

[`CommandRunner`](rustdoc:grit_lib::command_runner::CommandRunner) is the only production path that spawns OS processes from the library ([`SystemCommandRunner`](rustdoc:grit_lib::command_runner::SystemCommandRunner)). Hooks, smudge/clean filters, credential helpers, and signing helpers build a [`CommandSpec`](rustdoc:grit_lib::command_runner::CommandSpec) and call `spawn`.

For tests, [`RecordingRunner`](rustdoc:grit_lib::command_runner::RecordingRunner) records specs and returns scripted exit codes. Install it on `RepositoryOptions` before `Repository::open_with`.

## Typed errors

Library APIs return [`grit_lib::error::Result`](rustdoc:grit_lib::error::Result). Match on [`Error`](rustdoc:grit_lib::error::Error) variants (`RevParse`, `RevList`, `HookError`, `FilterError`, …) instead of parsing `"fatal:"` strings. The CLI maps variants to exit codes and human messages in its own words.

## Concurrency

Two or more [`Repository`](rustdoc:grit_lib::repo::Repository) handles may be used from different threads in one process when each handle is opened with its **own** `Environment`, `RepositoryOptions`, diagnostics sink, and command runner. Repository-scoped caches (config, attributes, pack read caches) do not leak between handles.

Integration coverage: `grit-lib/tests/concurrent_repos.rs` runs parallel commit loops and a fetch/merge/notes scenario with isolated config and sinks.

## Example

<!-- include: grit-examples/src/bin/guide_embedding.rs -->

Build and run:

```bash
cargo run -p grit-examples --bin guide_embedding
```
