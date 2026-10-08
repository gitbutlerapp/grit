# Repository

> Open and discover Git repositories, read git-dir vs work tree, load config, and handle grit_lib::Error.

A [`Repository`](https://docs.rs/grit-lib/latest/grit_lib/repo/struct.Repository.html) is the main handle for grit-lib. It carries the absolute [`git_dir`](https://docs.rs/grit-lib/latest/grit_lib/repo/struct.Repository.html) path, an optional [`work_tree`](https://docs.rs/grit-lib/latest/grit_lib/repo/struct.Repository.html) for non-bare repos, and an [`Odb`](https://docs.rs/grit-lib/latest/grit_lib/odb/struct.Odb.html) for object reads and writes.

## Discover vs open

Call [`Repository::discover`](https://docs.rs/grit-lib/latest/grit_lib/repo/struct.Repository.html) when you have a working directory and want the same upward search Git uses (respecting `GIT_DIR` and `GIT_WORK_TREE`). Call [`Repository::open`](https://docs.rs/grit-lib/latest/grit_lib/repo/struct.Repository.html) when you already know the git directory and optionally the work tree path.

Bare repositories have `work_tree: None`. Linked worktrees and gitfile indirection are handled during discovery so `git_dir` always points at the directory that contains `objects/`.

## Config

Repository code does not cache a full config snapshot on the struct. Load settings when you need them with [`ConfigSet::load`](https://docs.rs/grit-lib/latest/grit_lib/config/struct.ConfigSet.html), passing `Some(&repo.git_dir)` for repository-local files. Keys use Git’s dotted names (`user.name`, `core.bare`, …).

## Errors

Library operations return [`grit_lib::error::Result`](https://docs.rs/grit-lib/latest/grit_lib/error/type.Result.html). The [`Error`](https://docs.rs/grit-lib/latest/grit_lib/error/enum.Error.html) enum covers I/O, missing objects, bad repository layout, and invalid user input. Match on variants in application code; the `grit` CLI maps them to exit codes and messages separately.

## Example

The program below discovers a repository (or opens `.git` in the current directory), prints paths, and reads `user.name` from config:

```rust
//! Open and discover repositories; load config from the git directory.
//!
//! Source for the library guide "Repository" page (included in the docs site).

use grit_lib::config::ConfigSet;
use grit_lib::error::Error;
use grit_lib::repo::Repository;

fn main() -> Result<(), Error> {
    let repo = match Repository::discover(None) {
        Ok(r) => r,
        Err(Error::NotARepository(_)) => {
            let here = std::env::current_dir().map_err(Error::Io)?;
            let git_dir = here.join(".git");
            Repository::open(&git_dir, Some(&here))?
        }
        Err(err) => return Err(err),
    };

    println!("git_dir={}", repo.git_dir.display());
    if let Some(wt) = &repo.work_tree {
        println!("work_tree={}", wt.display());
    } else {
        println!("work_tree=<bare>");
    }

    let cfg = ConfigSet::load(Some(&repo.git_dir), true)?;
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
