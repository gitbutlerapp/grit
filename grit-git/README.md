# grit-git

A from-scratch reimplementation of the `git` command-line tool in Rust, built
on top of [`grit-lib`](https://crates.io/crates/grit-lib).

Grit aims for **behavioral compatibility** with Git where commands are implemented: flags, output shape, and exit codes should match user expectations. Development is validated with **Rust tests** in this crate and **`grit-lib`**, not a vendored upstream shell harness.

## Design philosophy

- **Behavioral compatibility with Git** where a command exists. Where Git's behavior is surprising, Grit generally reproduces the surprise.
- **No unsafe code.** The entire workspace forbids `unsafe`.
- **Fast startup.** Global options (`-C`, `--git-dir`, `--work-tree`, `-c`,
  `--bare`) are parsed by hand from argv before clap is invoked, so you only
  pay for the argument parser of the subcommand you actually run.

## Install

```sh
cargo install grit-git
```

This puts a `grit-git` binary on your `$PATH`.

## Usage

`grit-git` is used like `git`:

```sh
grit-git init myrepo
cd myrepo
echo "hello" > file.txt
grit-git add file.txt
grit-git commit -m "first commit"
grit-git log
```

## License

GPL-2.0-only (see workspace `LICENSE` split). Shell helper scripts under `shell-lib/` are derived from Git's GPL-2.0 helpers for `git-sh-setup` / `git-sh-i18n` compatibility.
