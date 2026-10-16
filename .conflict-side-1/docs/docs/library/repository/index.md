# Repository

> Open and discover Git repositories with an explicit Environment, read git-dir vs work tree, load config, and handle grit_lib::Error.

A [`Repository`](https://docs.rs/grit-lib/latest/grit_lib/repo/struct.Repository.html) is the main handle for grit-lib. It carries the absolute [`git_dir`](https://docs.rs/grit-lib/latest/grit_lib/repo/struct.Repository.html) path, an optional [`work_tree`](https://docs.rs/grit-lib/latest/grit_lib/repo/struct.Repository.html) for non-bare repos, an [`Odb`](https://docs.rs/grit-lib/latest/grit_lib/odb/struct.Odb.html) for object reads and writes, and the [`Environment`](https://docs.rs/grit-lib/latest/grit_lib/environment/struct.Environment.html) used to discover or open it.

## Environment

[`Environment`](https://docs.rs/grit-lib/latest/grit_lib/environment/struct.Environment.html) holds discovery and config variables (`GIT_DIR`, `GIT_WORK_TREE`, `GIT_CEILING_DIRECTORIES`, `GIT_CONFIG_*`, home paths, `cwd`, and related fields) without reading the process environment inside the library. Construct one with `Environment::empty()` (defaults only) or `Environment::from_vars()` (parse an iterator of `(key, value)` pairs plus an explicit working directory). See [`Environment`](https://docs.rs/grit-lib/latest/grit_lib/environment/struct.Environment.html).

The `grit` CLI builds an environment from the process in `grit-cli` and passes it into discovery. Embedders should do the same: snapshot the variables you care about once, then call `Repository::discover_with` or `Repository::open_with` with [`RepositoryOptions`](https://docs.rs/grit-lib/latest/grit_lib/environment/struct.RepositoryOptions.html).

[`Repository::discover`](https://docs.rs/grit-lib/latest/grit_lib/repo/struct.Repository.html) and [`Repository::open`](https://docs.rs/grit-lib/latest/grit_lib/repo/struct.Repository.html) remain convenience entry points that use `Environment::empty()` (no overrides beyond `cwd = "."`).

[`ConfigSet::load`](https://docs.rs/grit-lib/latest/grit_lib/config/struct.ConfigSet.html) takes `&Environment` as its first argument and always parses the cascade from disk (no process-global cache). Repository-scoped memoization lives on each open handle: [`Repository::config`](https://docs.rs/grit-lib/latest/grit_lib/repo/struct.Repository.html) returns `Arc<ConfigSet>` backed by [`RepoCaches`](https://docs.rs/grit-lib/latest/grit_lib/repo_caches/struct.RepoCaches.html) (config cascade, gitattributes stacks, filter-process drivers, precompose flags, and related state).

## Command runner

Hooks, clean/smudge filters, credential helpers, signing, SSH transport, and similar features spawn subprocesses through a [`CommandRunner`](https://docs.rs/grit-lib/latest/grit_lib/command_runner/trait.CommandRunner.html) stored on the repository (via [`RepositoryOptions::command_runner`](https://docs.rs/grit-lib/latest/grit_lib/environment/struct.RepositoryOptions.html)). The default is [`SystemCommandRunner`](https://docs.rs/grit-lib/latest/grit_lib/command_runner/struct.SystemCommandRunner.html), which is the only non-test code path that calls `std::process::Command`. Tests and embedders can install [`RecordingRunner`](https://docs.rs/grit-lib/latest/grit_lib/command_runner/struct.RecordingRunner.html) or a custom runner to assert argv, environment, and stdin without executing real programs. Hook failures surface as [`HookError`](https://docs.rs/grit-lib/latest/grit_lib/hooks/enum.HookError.html); shell filter subprocess failures use [`FilterError`](https://docs.rs/grit-lib/latest/grit_lib/error/enum.FilterError.html).

## Discover vs open

Call `Repository::discover_with` when you have a working directory and want Git-style upward search. Call `Repository::open_with` when you already know the git directory and optionally the work tree path. See [`Repository`](https://docs.rs/grit-lib/latest/grit_lib/repo/struct.Repository.html).

Bare repositories have `work_tree: None`. Linked worktrees and gitfile indirection are handled during discovery so `git_dir` always points at the directory that contains `objects/`.

## Config

[`Repository::config`](https://docs.rs/grit-lib/latest/grit_lib/repo/struct.Repository.html) returns a lazily loaded snapshot of the merged cascade (system / global / local / worktree / environment overrides). Prefer it on hot paths instead of calling `ConfigSet::load` repeatedly. Keys use Git’s dotted names (`user.name`, `core.bare`, …).

## Errors

Library operations return [`grit_lib::error::Result`](https://docs.rs/grit-lib/latest/grit_lib/error/type.Result.html). The [`Error`](https://docs.rs/grit-lib/latest/grit_lib/error/enum.Error.html) enum covers I/O, missing objects, bad repository layout, and invalid user input. Match on variants in application code; the `grit` CLI maps them to exit codes and messages separately.

## Example

The program below discovers a repository (or opens `.git` in the current directory), prints paths, and reads `user.name` from config:

```rust
//! Open and discover repositories; load config from the git directory.
//!
//! Source for the library guide "Repository" page (included in the docs site).

use grit_lib::config::ConfigSet;
use grit_lib::environment::{Environment, RepositoryOptions};
use grit_lib::error::Error;
use grit_lib::repo::Repository;

fn main() -> Result<(), Error> {
    let cwd = std::env::current_dir().map_err(Error::Io)?;
    let env = Environment::from_vars(std::env::vars_os(), cwd.clone());
    let options = RepositoryOptions::with_environment(env.clone());

    let repo = match Repository::discover_with(&options, None) {
        Ok(r) => r,
        Err(Error::NotARepository(_)) => {
            let git_dir = cwd.join(".git");
            Repository::open_with(&options, &git_dir, Some(&cwd))?
        }
        Err(err) => return Err(err),
    };

    println!("git_dir={}", repo.git_dir.display());
    if let Some(wt) = &repo.work_tree {
        println!("work_tree={}", wt.display());
    } else {
        println!("work_tree=<bare>");
    }

    let cfg = ConfigSet::load(repo.environment(), Some(&repo.git_dir), true)?;
    let name = cfg.get("user.name").unwrap_or_default();
    if !name.is_empty() {
        println!("user.name={name}");
    }

    Ok(())
}
```

Run from any Git checkout:

```bash
cargo run --bin guide_repository
```
