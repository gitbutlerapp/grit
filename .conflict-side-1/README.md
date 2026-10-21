# Grit — Git in Rust

Grit is a **Git engine in idiomatic Rust**: a linkable **`grit-lib`** and a modern **`grit`** client (**`grit-cli`**) built on it. See **ROADMAP.md** for the plan.

The Grit project is brought to you by the mad geniuses at [GitButler ⧓](https://gitbutler.com).

## Motivation

Grit started as a reimplementation of the whole `git` command line, aimed at passing Git's own test suite. That carried workarounds, slow paths, and every legacy subcommand, so the Git-compatible CLI (`grit-git`) and the ported test harness have been removed. **The focus now:** [grit-lib](https://crates.io/crates/grit-lib) as a clean, **linkable** library and [grit-cli](https://crates.io/crates/grit-cli) as a **modern Git client** with human, **`--json`**, and **`--markdown`** output. Both stay compatible with Git where it matters: repositories on disk and the wire protocol. **Performance is the top priority**, measured against real `git` on large-repo scenarios.

## Approach

Core Git semantics live in **`grit-lib`** (pluggable ODB and ref backends over time). The install script ships **`grit`** from **`grit-cli`**. Behavior is validated with **Rust tests** (cross-checked against the system `git` binary) and benchmarks. Unused areas (archive, email workflow, foreign-VCS bridges) are dropped from active development. Docs and benchmarks stay in sync with each change. Background: [True Grit](https://blog.gitbutler.com/true-grit).

The headline CLI shipped by the install script is `grit`, a simpler, opinionated interface from the [grit-cli](https://crates.io/crates/grit-cli) crate. It is the only binary the install script installs, on every platform including Windows.

## Usability

The current goal is **speed and a clean library API** while keeping on-disk and wire compatibility with Git where the library and CLI implement a feature. Try it out and send a fix or report an issue for gaps you hit.

## Installation

To install the `grit` CLI via Bash, you can run our install script:

```sh
$ curl -fsSL https://grit-scm.com/install | sh
```

There are builds for Mac and Linux (aarch64 and x86_64 for both). Linux ships both glibc and statically-linked musl binaries. Windows installs the same `grit` CLI.

## Updating

To update your version of Grit, you can run `grit update` and it will re-run the install script.

## The `grit` CLI

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

It covers local work (`status`, `add`, `commit`, `branch`, `switch`, `merge`, `log`, `config`) plus remote basics (`remote add`, `clone`, `fetch`, `pull`, `push`) with plain-language output.

`grit` can also serve repositories: `grit upload-pack` and `grit receive-pack` are the server side of fetch and push, used over ssh and by `grit-http-server` for smart HTTP.

The Windows version also comes with `grit manager` which works as an interface to Windows Credential Manager to store `grit auth` tokens securely.

## Rust Crates

| Crate                                                 | Description                                                                                     |
| ----------------------------------------------------- | ----------------------------------------------------------------------------------------------- |
| [`grit-cli`](https://crates.io/crates/grit-cli)       | The `grit` binary — workflow-oriented CLI backed by `grit-lib` (shipped by the install script) |
| [`grit-lib`](https://crates.io/crates/grit-lib)       | Core library: object model, diff engine, index, refs, revision walking, merge, config, and more |
| `grit-protocol` / `grit-http-server`                  | Smart HTTP serving via in-process `grit-lib` upload-pack / receive-pack                       |
| `grit-examples`                                       | Runnable examples of library usage                                                              |
| `grit-test-support`                                   | Workspace-only helpers for integration tests                                                    |

## License

All code and crates in this repository, including `grit-lib` and `grit-cli`, are MIT licensed.

## Testing

```sh
cargo test -p grit-lib --lib
cargo test --workspace
```

See **TESTING.md** for the full strategy.
