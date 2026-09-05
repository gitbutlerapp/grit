# Grit — Git in Rust

Grit is a **from-scratch reimplementation of Git** in idiomatic Rust. The goal is to match Git's behavior closely enough that the upstream test suite (under `git/t/`) can be ported and run against this tool.

The Grit project is brought to you by the mad geniuses at [GitButler ⧓](https://gitbutler.com).

## Progress

![Harness test progress](docs/test-progress.svg)

## Motivation

Why rewrite Git functionality into Rust? It's not about replacing Git, it's about having a feature-complete linkable library. It's similar to Gitoxide or libgit2/git2-rs, but using LLMs to try to achieve total feature parity by targeting the Git testing suite.

## Approach

This implementation has been written nearly entirely by AI coding agents with the goal of entirely passing the C Git testing suite. For details on how we accomplished this, see our [blog post](https://blog.gitbutler.com/true-grit).

The implementation is entirely in Rust, with most of the generic logic in the [grit-lib](https://crates.io/crates/grit-lib) library crate, and the Git-compatible CLI in the [grit-git](https://crates.io/crates/grit-git) crate (binary `grit-git`), which uses the library to provide a UI that passes the Git tests.

The headline CLI shipped by the install script is `grit`, a simpler, opinionated interface from the [grit-cli](https://crates.io/crates/grit-cli) crate. It is the only binary the install script installs, on every platform including Windows.

## Usability

While the `grit-git` command emulates `git` functionality enough to successfully run over 42k of it's tests, it has been nearly entirely written by agents and has not been used for realsies. It's probably currently unusably slow or completely broken in ways that are not exercised in the test suite.

Our current goal is to get all the tests to pass and then refactor to real usability (speed, API surface, etc) while being able to successfully test for regression easily. Try it out and either send a fix or report an issue for anything you find or ways you want to use it that it doesn't successfully do.

## Installation

To install the `grit` CLI via Bash, you can run our install script:

```sh
$ curl -fsSL https://grit-scm.com/install | sh
```

There are builds for Mac and Linux, (aarch64 and x86_64 for both). Linux ships both glibc and statically-linked musl binaries, so the installer works on distros like Alpine too — it auto-detects which one your system needs. Windows installs the same `grit` CLI. The Git-compatible `grit-git` binary is not installed by the script — install it with `cargo install grit-git`.

## Updating

To update your version of Grit, you can run `grit update` and it will re-run the install script.

## The `grit` CLI

test
line two
line three
omg, more lines

The `grit` binary (from the `grit-cli` crate) is what the install script ships. It is not a Git-identical CLI; it is a simpler interface for common developer workflows built on `grit-lib`, portable to Windows.

`grit` treats status as the home screen and keeps the common path terse:

```sh
grit auth   # authenticate into github and store https auth tokens for fetch/push as your user
grit clone https://github.com/user/project
grit        # status dashboard
grit commit "message" # commit changes
grit switch -c topic
grit push
grit pull
```

It covers local work (`status`, `add`, `commit`, `branch`, `switch`, `merge`, `log`, `config`) plus remote basics (`remote add`, `clone`, `fetch`, `pull`, `push`) with plain-language output. Use `grit-git` when you need Git-compatible command behavior; use `grit` when you want the smaller workflow-oriented interface.

The Windows version also comes with `grit manager` which works as an interface to Windows Credential Manager to store `grit auth` tokens securely.

## Rust Crates

| Crate                                                 | Description                                                                                     |
| ----------------------------------------------------- | ----------------------------------------------------------------------------------------------- |
| [`grit-cli`](https://crates.io/crates/grit-cli)       | The `grit` binary — a smaller workflow-oriented CLI backed by `grit-lib` (shipped by the install script) |
| [`grit-lib`](https://crates.io/crates/grit-lib)       | Core library: object model, diff engine, index, refs, revision walking, merge, config, and more |
| [`grit-git`](https://crates.io/crates/grit-git) | The `grit-git` binary — a drop-in CLI reimplementation of `git` with 140+ commands (`cargo install grit-git`) |
| `grit-examples`                                       | Runnable examples of simple lib usage (add, cat-file, write-tree, hash-object, etc)             |
| `grit-test-support`                                   | Workspace-only helpers for integration tests                                                    |

## License

The `grit-git` code is GPL-2.0, all other code and crates, including `grit-lib` are MIT licensed.

testing
