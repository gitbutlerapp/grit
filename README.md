# Grit — Git in Rust

Grit is a **from-scratch Git engine in idiomatic Rust**: a fast, linkable **`grit-lib`**, a modern **`grit`** CLI, and **`grit-git`** as a Git-compatible compatibility bed. See **ROADMAP.md** for the ordered plan.

The Grit project is brought to you by the mad geniuses at [GitButler ⧓](https://gitbutler.com).

## Progress

![Harness test progress](docs/test-progress.svg)

## Motivation

We want a **fast, clean Git library** you can embed in tools and agents—not a wholesale replacement for the `git` binary on every machine. **`grit-lib`** holds core semantics; **`grit-cli`** ships a modern workflow with **`--json`** / **`--markdown`** on every command; **`grit-git`** keeps upstream-style compatibility so we can catch regressions.

## Approach

Most logic lives in [grit-lib](https://crates.io/crates/grit-lib). The install script ships [grit-cli](https://crates.io/crates/grit-cli) as **`grit`**; [grit-git](https://crates.io/crates/grit-git) exercises the same library against the ported upstream harness as a **regression gate**, while new coverage moves into **Rust tests** on the library API. Performance is measured against real `git` in **`bench/`**. For background on how the project started, see the [True Grit](https://blog.gitbutler.com/true-grit) post.

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
