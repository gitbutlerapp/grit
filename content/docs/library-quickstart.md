---
title: Library quick start
summary: Add grit-lib to a Rust project, open a repository, and read the current commit from HEAD.
---

This page walks through a minimal program that discovers a Git repository in the current directory (or a parent), resolves `HEAD`, and prints the commit id and subject line. The source below is the real example binary in the Grit repository; it is included automatically so the docs cannot drift from compiled code.

## Add the dependency

In your crate:

```bash
cargo add grit-lib
```

Or add to `Cargo.toml`:

```toml
[dependencies]
grit-lib = "0.5.0"
```

Use the [latest version on crates.io](https://crates.io/crates/grit-lib) if the number above is stale.

## Example program

Save as `src/main.rs` (or copy from `grit-examples` in the Grit repo):

<!-- include: grit-examples/src/bin/quickstart.rs -->

## Run it

From the root of any Git repository with at least one commit:

```bash
cargo run
```

Example output (your commit id will differ):

```text
217c6f9a1b2c3d4e5f6789012345678901234567
Start the project
```

The first line is the full object id of `HEAD`; the second is the first line of the commit message.

## The environment

The program opens the repository with [`RepositoryOptions::from_process`](rustdoc:grit_lib::config::environment::RepositoryOptions::from_process), which snapshots the process environment (`HOME`, `GIT_*`, Windows `ProgramFiles`) so system and global config apply exactly as they do for `git` and `grit`: `core.autocrlf`, `user.name` and `user.email`, `safe.directory`, `core.excludesFile` and credential helpers. The library itself never reads the process environment. [`RepositoryOptions::empty`](rustdoc:grit_lib::config::environment::RepositoryOptions::empty) is the hermetic alternative for tests: no home directory and no global config. See [Embedding grit-lib](../library/embedding/#matching-git-and-grit-on-user-repositories).

## Next steps

- [Library guide overview](../library/) — longer-form guides for objects, refs, diff, and network code.
- [grit-lib on docs.rs](https://docs.rs/grit-lib) — API reference for `Repository` and the rest of the public surface.
- [Tutorial](../tutorial/) — the same repository operations from the `grit` CLI.
