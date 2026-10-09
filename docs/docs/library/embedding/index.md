# Embedding grit-lib

> Run multiple repositories in one process with explicit Environment, sinks, subprocess injection, and typed errors.

Grit is designed to be **linked into other Rust programs**, not only invoked as the `grit` CLI. The library avoids hidden process globals: discovery variables, config identity, subprocesses, warnings, and wall-clock references are supplied through explicit types at the boundary.

## Environment

[`Environment`](https://docs.rs/grit-lib/latest/grit_lib/environment/struct.Environment.html) holds Git discovery and configuration variables (`GIT_DIR`, `GIT_WORK_TREE`, `GIT_CONFIG_*`, home paths, author/committer overrides, transport trace flags, and related fields). Construct it with:

- `Environment::empty()` — defaults only (`cwd = "."`).
- [`Environment::from_vars`](https://docs.rs/grit-lib/latest/grit_lib/environment/struct.Environment.html) — parse an iterator of `(key, value)` pairs plus an explicit working directory (no reads from `std::env` inside the library).
- `Environment::capture_process()` — **CLI boundary only**: snapshot the current process into an [`Environment`](https://docs.rs/grit-lib/latest/grit_lib/environment/struct.Environment.html).

Pass the same `Environment` into [`Repository`](https://docs.rs/grit-lib/latest/grit_lib/repo/struct.Repository.html) (`discover_with` / `open_with`) and into [`ConfigSet`](https://docs.rs/grit-lib/latest/grit_lib/config/struct.ConfigSet.html) loading.

## RepositoryOptions

[`RepositoryOptions`](https://docs.rs/grit-lib/latest/grit_lib/environment/struct.RepositoryOptions.html) bundles everything needed to open a repository besides paths:

| Field | Role |
| --- | --- |
| `environment` | Discovery/config/identity snapshot |
| `command_runner` | Injectable subprocess execution (hooks, filters, signing helpers) |
| `diagnostics` | [`DiagnosticSink`](https://docs.rs/grit-lib/latest/grit_lib/diagnostics/trait.DiagnosticSink.html) for warnings and trace events |
| `network_trace` | When true, emit network trace events via the diagnostics sink |
| `reference_unix_time` | Wall clock for rev-parse date selectors (`@{yesterday}`, etc.) |
| Test knobs | `test_assume_different_owner`, `force_split_index`, … |

Use `RepositoryOptions::with_environment` and `with_command_runner` on [`RepositoryOptions`](https://docs.rs/grit-lib/latest/grit_lib/environment/struct.RepositoryOptions.html); assign `diagnostics` when you need a custom sink.

## DiagnosticSink and ProgressSink

Non-fatal conditions are [`Warning`](https://docs.rs/grit-lib/latest/grit_lib/diagnostics/enum.Warning.html) values delivered to a [`DiagnosticSink`](https://docs.rs/grit-lib/latest/grit_lib/diagnostics/trait.DiagnosticSink.html). The default is [`NullDiagnostics`](https://docs.rs/grit-lib/latest/grit_lib/diagnostics/struct.NullDiagnostics.html). Tests and embedders often use [`CollectingDiagnostics`](https://docs.rs/grit-lib/latest/grit_lib/diagnostics/struct.CollectingDiagnostics.html) to assert warnings without parsing stderr.

Long-running work (status, fetch, pack indexing) takes a [`ProgressSink`](https://docs.rs/grit-lib/latest/grit_lib/progress/trait.ProgressSink.html) (for example [`NullProgress`](https://docs.rs/grit-lib/latest/grit_lib/progress/struct.NullProgress.html)). Progress is separate from diagnostics: progress reports throughput; diagnostics report Git-style warnings.

## CommandRunner

[`CommandRunner`](https://docs.rs/grit-lib/latest/grit_lib/command_runner/trait.CommandRunner.html) is the only production path that spawns OS processes from the library ([`SystemCommandRunner`](https://docs.rs/grit-lib/latest/grit_lib/command_runner/struct.SystemCommandRunner.html)). Hooks, smudge/clean filters, credential helpers, and signing helpers build a [`CommandSpec`](https://docs.rs/grit-lib/latest/grit_lib/command_runner/struct.CommandSpec.html) and call `spawn`.

For tests, [`RecordingRunner`](https://docs.rs/grit-lib/latest/grit_lib/command_runner/struct.RecordingRunner.html) records specs and returns scripted exit codes. Install it on `RepositoryOptions` before `Repository::open_with`.

## Typed errors

Library APIs return [`grit_lib::error::Result`](https://docs.rs/grit-lib/latest/grit_lib/error/type.Result.html). Match on [`Error`](https://docs.rs/grit-lib/latest/grit_lib/error/enum.Error.html) variants (`RevParse`, `RevList`, `HookError`, `FilterError`, …) instead of parsing `"fatal:"` strings. The CLI maps variants to exit codes and human messages in its own words.

## Concurrency

Two or more [`Repository`](https://docs.rs/grit-lib/latest/grit_lib/repo/struct.Repository.html) handles may be used from different threads in one process when each handle is opened with its **own** `Environment`, `RepositoryOptions`, diagnostics sink, and command runner. Repository-scoped caches (config, attributes, pack read caches) do not leak between handles.

Integration coverage: `grit-lib/tests/concurrent_repos.rs` runs parallel commit loops and a fetch/merge/notes scenario with isolated config and sinks.

## Example

```rust
//! Open two repositories in one process with isolated environments and sinks.
//!
//! Source for the library guide "Embedding" page (included in the docs site).

use std::sync::Arc;

use grit_lib::command_runner::RecordingRunner;
use grit_lib::diagnostics::{CollectingDiagnostics, DiagnosticSink};
use grit_lib::environment::{Environment, RepositoryOptions};
use grit_lib::error::Error;
use grit_lib::repo::{init_repository, Repository};

fn main() -> Result<(), Error> {
    let base = tempfile::tempdir().map_err(Error::Io)?;

    let open_named =
        |name: &str, email: &str| -> Result<(Repository, Arc<CollectingDiagnostics>), Error> {
            let root = base.path().join(name);
            init_repository(&root, false, "main", None, "files")?;

            let home = base.path().join(format!("home-{name}"));
            std::fs::create_dir_all(&home).map_err(Error::Io)?;
            let global = home.join(".gitconfig");
            std::fs::write(
                &global,
                format!("[user]\n\tname = {name}\n\temail = {email}\n"),
            )
            .map_err(Error::Io)?;

            let mut env = Environment::empty();
            env.cwd = root.clone();
            env.home = Some(home.into());
            env.git_config_global = Some(global.to_string_lossy().into_owned());
            env.git_config_nosystem = Some("true".into());
            env.git_config_system = Some("/dev/null".into());

            let diagnostics = Arc::new(CollectingDiagnostics::new());
            let sink: Arc<dyn DiagnosticSink + Send + Sync> = diagnostics.clone();
            let runner = RecordingRunner::always_success();
            let mut options = RepositoryOptions::with_environment(env).with_command_runner(runner);
            options.diagnostics = sink;

            let git_dir = root.join(".git");
            let repo = Repository::open_with(&options, &git_dir, Some(&root))?;
            Ok((repo, diagnostics))
        };

    let (repo_a, sink_a) = open_named("alpha", "a@example.com")?;
    let (repo_b, sink_b) = open_named("beta", "b@example.com")?;

    let name_a = repo_a.config()?.get("user.name").unwrap_or_default();
    let name_b = repo_b.config()?.get("user.name").unwrap_or_default();
    println!("alpha user.name={name_a}");
    println!("beta user.name={name_b}");
    println!("alpha warnings={}", sink_a.warnings().len());
    println!("beta warnings={}", sink_b.warnings().len());

    Ok(())
}
```

Build and run:

```bash
cargo run -p grit-examples --bin guide_embedding
```
