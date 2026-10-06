# grit-git

Git-compatible command-line binary built on [`grit-lib`](https://crates.io/crates/grit-lib).

**`grit-git`** exists as a compatibility test bed: the shell harness under `tests/` runs it as `git` to catch regressions in formats, protocols, and command behavior. The product focus is **`grit-lib`** (linkable library) and **`grit-cli`** (modern `grit` client).

## Design philosophy

- **Behavioral compatibility with Git** where the harness and library tests require it — flags, output shape, and exit codes for implemented commands.
- **No unsafe code.** The entire workspace forbids `unsafe`.
- **Fast startup.** Global options (`-C`, `--git-dir`, `--work-tree`, `-c`, `--bare`) are parsed by hand from argv before clap is invoked, so you only pay for the argument parser of the subcommand you actually run.

## Install

```sh
cargo install grit-git
```

This puts a `grit-git` binary on your `$PATH`.

## Usage

`grit-git` is used exactly like `git`:

```sh
grit-git init myrepo
cd myrepo
echo "hello" > file.txt
grit-git add file.txt
grit-git commit -m "first commit"
grit-git log
```

## Implemented commands

Grit currently implements over 140 Git commands spanning porcelain, plumbing, and network operations. See the crate source under `src/commands/` for the live list.

## Using as a library

The binary crate itself is not intended for library use. If you want to work with Git repositories programmatically from Rust, depend on
[`grit-lib`](https://crates.io/crates/grit-lib) instead — it exposes the
full object model, diff engine, revision walker, index handling, and more.

## License

GPL-2.0 (Git-compatible CLI strings and harness-oriented behavior live here, not in MIT-licensed `grit-lib`).
