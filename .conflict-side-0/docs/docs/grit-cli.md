# grit: the command line

> Everything an agent needs to use `grit` as a Git client: install, the tutorial, global options, scripting with `--json` and `--filter`, the agent guide, and the reference page for every command. `grit` works on any Git repository and talks to any Git remote.

Each section below is one docs page; its heading is the page's URL.

# https://grit-scm.com/docs/index.md

# Docs

> How to use grit, a simple Git client built on grit-lib. Start with the tutorial, then look up any command or open the library guide.

Grit is two things:

- **`grit`** — a Git client with a small, modern command line. It works on any Git repository and talks to any Git remote, but its commands are its own: one obvious way to do the common thing, plain-language output, and `--json` on every command.
- **`grit-lib`** — a fast, linkable Git library for Rust. Everything the CLI does goes through the library; you can embed the same engine in your own tools.

## How these docs are organized

| Section | What you'll find |
| --- | --- |
| **Getting started** | [Install](https://grit-scm.com/docs/install/index.md) grit, follow the [tutorial](https://grit-scm.com/docs/tutorial/index.md), or try the [library quick start](https://grit-scm.com/docs/library-quickstart/index.md). |
| **The grit CLI** | [Global options](https://grit-scm.com/docs/global-options/index.md), [scripting with `--json`](https://grit-scm.com/docs/scripting/index.md), [agent guide](https://grit-scm.com/docs/agents/index.md), and a man page for every command. |
| **Library guide** | Rust-oriented guides for `grit-lib` (growing over time), plus the [API reference on docs.rs](https://docs.rs/grit-lib). |
| **Benchmarks** | How `grit` and `grit-lib` compare to system Git on core operations. |

If you're new to the command line, start with the [tutorial](https://grit-scm.com/docs/tutorial/index.md). If you're wiring Git into a Rust program, start with the [library quick start](https://grit-scm.com/docs/library-quickstart/index.md) and the [library guide overview](https://grit-scm.com/docs/library/index.md).

## CLI vs library

Use **`grit`** when you want a day-to-day Git client from the terminal or in scripts (`--json` / `--filter`). Use **`grit-lib`** when you need programmatic access — opening repositories, reading objects and refs, diffing, revwalking, fetch/push, and the rest of Git's core — inside a Rust binary or service.

On-disk formats and the wire protocol stay compatible with Git; the CLI's argv and messages are deliberately *not* a Git mirror.

See [Install](https://grit-scm.com/docs/install/index.md) for download options, [Global options](https://grit-scm.com/docs/global-options/index.md) for flags shared by every command, [Scripting with grit](https://grit-scm.com/docs/scripting/index.md) for `--json` and `--filter`, and the [Agent guide](https://grit-scm.com/docs/agents/index.md) for automation-focused details.

## For agents

- [grit-cli.md](https://grit-scm.com/docs/grit-cli.md): the whole CLI in one file, to learn grit for everyday Git work.
- [grit-lib.md](https://grit-scm.com/docs/grit-lib.md): the whole library guide in one file, to build Git-compatible Rust programs.
- [llms.txt](https://grit-scm.com/llms.txt): an index of every page with a one-line summary.
- [llms-full.txt](https://grit-scm.com/llms-full.txt): every docs page in one file.

Every page also has a Markdown twin: replace `index.html` with `index.md` in its URL. Run `grit skill` to print an agent skill for the CLI.

## Commands

### Getting started

| Command | Summary |
| --- | --- |
| [`grit init`](https://grit-scm.com/docs/init/index.md) | Create a new, empty repository. |
| [`grit clone`](https://grit-scm.com/docs/clone/index.md) | Copy a remote repository into a new directory. |
| [`grit status`](https://grit-scm.com/docs/status/index.md) | Show where you are and what's changed. This is what plain grit runs. |
| [`grit config`](https://grit-scm.com/docs/config/index.md) | Read, set, list or remove configuration values. |

### Making changes

| Command | Summary |
| --- | --- |
| [`grit add`](https://grit-scm.com/docs/add/index.md) | Stage changes. With no paths, stages everything. |
| [`grit commit`](https://grit-scm.com/docs/commit/index.md) | Stage every change and record a new commit. |
| [`grit diff`](https://grit-scm.com/docs/diff/index.md) | Show uncommitted changes, or the change a commit introduced. |
| [`grit restore`](https://grit-scm.com/docs/restore/index.md) | Restore working tree and/or index paths from HEAD, the index, or another revision. |

### History

| Command | Summary |
| --- | --- |
| [`grit log`](https://grit-scm.com/docs/log/index.md) | Show recent commits, newest first, a page at a time. |
| [`grit shortlog`](https://grit-scm.com/docs/shortlog/index.md) | List the commits on this branch that aren't on the target branch yet. |
| [`grit show`](https://grit-scm.com/docs/show/index.md) | Show a commit, tag or branch, and a summary of what it changed. |

### Branches and tags

| Command | Summary |
| --- | --- |
| [`grit branch`](https://grit-scm.com/docs/branch/index.md) | List branches, or create or delete one. |
| [`grit switch`](https://grit-scm.com/docs/switch/index.md) | Switch to another branch, or create one and switch to it. |
| [`grit merge`](https://grit-scm.com/docs/merge/index.md) | Merge another branch into the current one. |
| [`grit pick`](https://grit-scm.com/docs/pick/index.md) | Copy a single commit onto the current branch. |
| [`grit tag`](https://grit-scm.com/docs/tag/index.md) | List tags, or create or delete one. A new tag points at the current commit. |

### Remotes

| Command | Summary |
| --- | --- |
| [`grit remote`](https://grit-scm.com/docs/remote/index.md) | List remotes, add one, or list refs on a remote. |
| [`grit fetch`](https://grit-scm.com/docs/fetch/index.md) | Download new commits, branches and tags from a remote. |
| [`grit pull`](https://grit-scm.com/docs/pull/index.md) | Fetch from the remote and bring the current branch up to date. |
| [`grit push`](https://grit-scm.com/docs/push/index.md) | Publish the current branch, or your tags, to a remote. |
| [`grit auth`](https://grit-scm.com/docs/auth/index.md) | Sign in to GitHub so HTTPS pushes and fetches just work. |
| [`grit bundle`](https://grit-scm.com/docs/bundle/index.md) | Create, verify, and list git bundle files for offline transfer. |

### Maintenance

| Command | Summary |
| --- | --- |
| [`grit update`](https://grit-scm.com/docs/update/index.md) | Update grit to the latest release. |
| [`grit skill`](https://grit-scm.com/docs/skill/index.md) | Print an agent skill that explains how to use grit. |
| [`grit completions`](https://grit-scm.com/docs/completions/index.md) | Generate shell completion scripts for bash, zsh, and fish. |

### Plumbing

| Command | Summary |
| --- | --- |
| [`grit upload-pack`](https://grit-scm.com/docs/upload-pack/index.md) | Serve a fetch or clone of a repository over stdin and stdout. |
| [`grit receive-pack`](https://grit-scm.com/docs/receive-pack/index.md) | Accept a push to a repository over stdin and stdout. |
| [`grit manager`](https://grit-scm.com/docs/manager/index.md) | A credential helper that stores passwords and tokens in the Windows Credential Manager. |

# https://grit-scm.com/docs/install/index.md

# Install grit

> Install the grit CLI on your machine, try nightly builds, and keep your copy up to date.

## Stable release (recommended)

On macOS and Linux:

```bash
curl -fsSL https://grit-scm.com/install | sh
```

On Windows, run this in PowerShell:

```bash
irm https://grit-scm.com/install.ps1 | iex
```

The script downloads the latest release from GitHub, installs the `grit` binary (by default into `~/.local/bin` on Unix), and prints the version it installed.

## Shell completions

Tab completion helps you discover `grit`'s subcommands and flags. Generate a script with [`grit completions`](https://grit-scm.com/docs/completions/index.md) and load it from your shell config, or copy the pre-generated files from a release tarball's `completions/` directory.

**bash** — add to `~/.bashrc`:

```bash
eval "$(grit completions bash)"
```

**zsh** — add to `~/.zshrc`:

```bash
eval "$(grit completions zsh)"
```

Or install once under a site-functions directory:

```bash
grit completions zsh > ~/.local/share/zsh/site-functions/_grit
```

**fish** — write a completion file:

```bash
mkdir -p ~/.config/fish/completions
grit completions fish > ~/.config/fish/completions/grit.fish
```

After [`grit update`](https://grit-scm.com/docs/update/index.md), regenerate completions so new commands appear.

## Nightly builds

CI publishes a rolling **nightly** prerelease when the release workflow is triggered manually. To install that build instead of the latest stable release:

```bash
curl -fsSL https://grit-scm.com/install-nightly | sh
```

On Windows:

```bash
irm https://grit-scm.com/install-nightly.ps1 | iex
```

Nightlies track recent `main`; use them when you want to test fixes before the next tagged release.

## Install with Cargo

If you already have a Rust toolchain:

```bash
cargo install grit-cli
```

This builds and installs the `grit` executable into Cargo's bin directory (usually `~/.cargo/bin`). Make sure that directory is on your `PATH`.

## Keep grit up to date

After a script install, run [`grit update`](https://grit-scm.com/docs/update/index.md) to re-run the same installer and replace the binary you're running. If you installed with Cargo, update with:

```bash
cargo install grit-cli
```

## Build from source

Clone [the repository](https://github.com/gitbutlerapp/grit) and build the CLI crate:

```bash
cargo build --release -p grit-cli
```

The binary is `target/release/grit`. You can copy it anywhere on your `PATH`, or run it from the build tree.

### Toolchain

The workspace pins a Rust version in [`rust-toolchain.toml`](https://github.com/gitbutlerapp/grit/blob/main/rust-toolchain.toml) (currently **1.99.0** with `rustfmt` and `clippy`). `rustup` picks this up automatically when you build in the repo.

To use `grit-lib` in your own crate, add it from [crates.io](https://crates.io/crates/grit-lib) — see the [library quick start](https://grit-scm.com/docs/library-quickstart/index.md).

# https://grit-scm.com/docs/tutorial/index.md

# Tutorial

> Fifteen minutes with grit. Create a repository, record some changes, work on a branch, and share it with a remote.

This walkthrough covers the commands you'll use every day. It assumes you have [installed grit](https://grit-scm.com/docs/install/index.md) and know roughly what a commit and a branch are. The output shown is what `grit` prints, minus the colors.

Regenerated on 2026-10-07 by running the listed commands with `grit` built from this repository (`cargo build --release -p grit-cli`), with `NO_COLOR=1` and author identity `Ada Lovelace <ada@example.com>`.

## Tell grit who you are

Every commit records an author. Set your name and email once, in your global config:

```console
$ grit config --global user.name "Ada Lovelace"
$ grit config --global user.email ada@example.com
```

`grit` reads and writes the same config files as Git, so if you've already set these up for Git, you can skip this step.

## Create a repository

```console
$ grit init project
Initialized empty repository in /workspace/project/.git
$ cd project
```

To work on an existing project instead, copy it with [`grit clone`](https://grit-scm.com/docs/clone/index.md) and skip ahead to the next section:

```console
$ grit clone https://github.com/gitbutlerapp/grit.git
```

## Your home base: grit status

Running `grit` with no arguments shows where you are and what's changed. Add a couple of files and look:

```console
$ echo "# Notes" > README.md
$ echo "fn main() {}" > main.rs
$ grit
On main — no commits yet

Untracked
  ?  untracked     README.md
  ?  untracked     main.rs

→ grit add <file> to stage
```

The last line always suggests the next step. You'll come back to this screen a lot; [`grit status`](https://grit-scm.com/docs/status/index.md) (or `grit st`) shows the same thing.

## Record a commit

[`grit commit`](https://grit-scm.com/docs/commit/index.md) stages every change in the working tree and records it in one step:

```console
$ grit commit "Start the project"
[main 310fdb0] Start the project
2 changes committed
```

There is no separate staging step to remember. [`grit add`](https://grit-scm.com/docs/add/index.md) exists for when you want to stage particular files and check them in `grit status` first, but `grit commit` always records every change.

Look at the history with [`grit log`](https://grit-scm.com/docs/log/index.md):

```console
$ grit log
  310fdb0  ada  just now  Start the project
```

## Work on a branch

Create a branch and switch to it with [`grit switch -c`](https://grit-scm.com/docs/switch/index.md):

```console
$ grit switch -c feature
Created and switched to branch feature
```

Make a change and look at it with [`grit diff`](https://grit-scm.com/docs/diff/index.md) before committing:

```console
$ printf 'fn main() {\n    println!("hi");\n}\n' > main.rs
$ grit diff

main.rs
@@ -1 +1 @@
 1    │ - fn main() {}
    1 │ + fn main() {
    2 │ +     println!("hi");
    3 │ + }
```

The two number columns are the old and new line numbers. Commit the change:

```console
$ grit commit "Say hi"
[feature 2e409e2] Say hi
1 change committed
```

[`grit show`](https://grit-scm.com/docs/show/index.md) displays a commit, its message and the files it changed. With no argument, it shows the latest commit:

```console
$ grit show
branch feature
commit 2e409e26f8b370ea1928a8fb93dcf2e025418e1f
Author: Ada Lovelace <ada@example.com>
Date:   2026-10-07 14:55:47 +0000

    Say hi

 main.rs |   4 +++-
 1 file changed, 3 insertions(+), 1 deletion(-)
```

## Merge it back

Switch back to `main` and [merge](https://grit-scm.com/docs/merge/index.md) the branch in. Nothing else has happened on `main`, so `grit` just moves `main` forward:

```console
$ grit switch main
Switched to branch main
$ grit merge feature
Fast-forwarded feature → 2e409e2
```

The branch is done, so [delete it](https://grit-scm.com/docs/branch/index.md):

```console
$ grit branch -d feature
Deleted branch feature (was 2e409e2).
```

`grit branch -d` refuses to delete a branch whose commits haven't been merged into the branch you're on. Use `-D` when you really mean it.

## Share it with a remote

A remote is another copy of the repository, usually on a server. Add one called `origin` with [`grit remote add`](https://grit-scm.com/docs/remote/index.md), then [push](https://grit-scm.com/docs/push/index.md):

```console
$ grit remote add origin https://github.com/ada/project.git
Added remote origin → https://github.com/ada/project.git
$ grit push
  pushed main → origin refs/heads/main
```

`grit push` sends the current branch to a branch with the same name on `origin`, creating it if needed. There are no upstream flags to set. For GitHub over HTTPS, [`grit auth`](https://grit-scm.com/docs/auth/index.md) signs you in, and `grit` offers to run it if a push fails for lack of credentials.

To get other people's work, run [`grit pull`](https://grit-scm.com/docs/pull/index.md). It fetches from the remote and brings your branch up to date, fast-forwarding when it can and recording a merge commit when both sides have new commits:

```console
$ grit pull
Merged origin/main into the current branch (91275a2)
```

If someone pushed before you, `grit push` is rejected and tells you what to do:

```console
$ grit push
  rejected origin refs/heads/main: not a fast-forward — run `grit pull` first
```

## Tag a release

[`grit tag`](https://grit-scm.com/docs/tag/index.md) marks the current commit, and `grit push --tags` publishes your tags:

```console
$ grit tag v0.1
Created tag v0.1
$ grit push --tags
  pushed --tags → origin refs/tags/v0.1
```

## When things conflict

`grit merge`, `grit pull` and `grit pick` never leave a half-finished merge behind. If the two sides change the same lines, `grit` lists the conflicting files and leaves your branch and working tree as they were. To resolve the conflict, run the merge with `git`, fix the files and commit. Conflict resolution in `grit` itself is on the roadmap.

`grit` also refuses to switch branches, merge or pull while you have uncommitted changes, so work in progress can't get mixed into a merge. Commit first.

## Scripting

Every command takes `--json` and prints a single JSON object, which is handy for scripts and agents. `--filter` picks out the part you need:

```console
$ grit status --json --filter '{branch, clean}'
{
  "branch": "main",
  "clean": true
}
```

See [Scripting with grit](https://grit-scm.com/docs/scripting/index.md) for details, and each command's page for its JSON fields.

## Where to go next

Every command has a man page with its options and examples. Start from the [command list](https://grit-scm.com/docs/index.md#commands).

# https://grit-scm.com/docs/global-options/index.md

# Global options

> Options that apply to every grit command.

These options work on any command (and on `grit` itself when you pass no subcommand).

| Option | Description |
| --- | --- |
| `--json` | Print one JSON object instead of human-readable text. Errors become `{"error": "..."}`. |
| `--markdown` | Print agent-friendly Markdown instead of human-readable text. Most commands render structured headings, lists, and tables (or fenced diffs) rather than dumping JSON. Mutually exclusive with `--json`. |
| `--filter <EXPR>` | Apply a jq expression to the JSON output. Requires `--json`. |
| `-h`, `--help` | Print help for `grit` or for a command. |
| `-V`, `--version` | Print the version of `grit`. |

Running `grit` with no command is the same as [`grit status`](https://grit-scm.com/docs/status/index.md).

## Environment variables

The `grit` CLI snapshots the process environment into [`Environment`](https://grit-scm.com/docs/library/repository/index.md) when opening a repository. Library embedders should construct an [`Environment`](https://grit-scm.com/docs/library/repository/index.md) explicitly instead of relying on these variables.

| Variable | Effect |
| --- | --- |
| `GIT_DIR`, `GIT_WORK_TREE`, `GIT_CEILING_DIRECTORIES`, `GIT_PREFIX` | Repository discovery |
| `GIT_INDEX_FILE`, `GIT_OBJECT_DIRECTORY`, `GIT_ALTERNATE_OBJECT_DIRECTORIES` | Index and object store paths |
| `GIT_CONFIG`, `GIT_CONFIG_GLOBAL`, `GIT_CONFIG_SYSTEM`, `GIT_CONFIG_NOSYSTEM`, `GIT_CONFIG_COUNT`, `GIT_CONFIG_KEY_n`, `GIT_CONFIG_VALUE_n`, `GIT_CONFIG_PARAMETERS` | Config cascade |
| `HOME`, `XDG_CONFIG_HOME`, `USERPROFILE`, `USER`, `USERNAME` | Home and identity fallbacks |
| `GIT_AUTHOR_*`, `GIT_COMMITTER_*` | Author/committer name, email, and date overrides for new objects |
| `GIT_SSH`, `GIT_SSH_COMMAND` | SSH transport command |
| `GIT_HTTP_LOW_SPEED_LIMIT`, `GIT_HTTP_LOW_SPEED_TIME` | HTTP low-speed abort thresholds |
| `GIT_NOTES_REF`, `GIT_NOTES_DISPLAY_REF` | Notes ref selection |
| `GIT_ATTR_SOURCE`, `GIT_INDEX_VERSION`, `GIT_PRINT_SHA1_ELLIPSIS` | Attributes, index format, diff OID display |
| `GIT_NAMESPACE`, `GIT_REPLACE_REF_BASE`, `GIT_NO_REPLACE_OBJECTS` | Ref namespaces and replace refs |
| `GIT_TRACE`, `GIT_TRACE_SETUP`, `GIT_TRACE2_PERF` | Trace output |
| `PATH` | Helper lookup (e.g. signing programs) |
| `TZ` | Local timezone for dates when no explicit offset is given |
| `GIT_EXEC_PATH`, `GIT_INSTALL_ROOT` | Git helper discovery (credentials) |

Test-only knobs (`GIT_TEST_NO_WRITE_REV_INDEX`, `GIT_TEST_MIDX_WRITE_REV`, `GIT_TEST_REFTABLE_AUTOCOMPACTION`, `GIT_TEST_DATE_NOW`, and related values) are parsed into typed [`Environment`](https://grit-scm.com/docs/library/repository/index.md) fields when present; they are not read elsewhere in `grit-lib`.

# https://grit-scm.com/docs/scripting/index.md

# Scripting with grit

> Use --json and --filter to build scripts on top of grit.

With `--json`, a command prints exactly one JSON value on stdout, so you can pipe it straight into other tools. The shape of each command's output is documented on its page. `--filter` runs a [jq](https://jqlang.org) expression over that value without needing `jq` installed. When a filter produces several values, they come back as one JSON array:

```console
$ grit status --json --filter '{branch, clean}'
{
  "branch": "main",
  "clean": true
}

$ grit log --json --filter '.commits[].subject'
[
  "Say hi",
  "Start the project"
]
```

`grit` exits with status `0` on success and `1` when a command fails (including a push with a rejected ref). A mistake on the command line, such as an unknown option, exits with `2`.

Color is used only when stdout is a terminal. Set `NO_COLOR` to turn it off.

For autonomous agents — non-interactive auth, where to find `llms.txt`, and a fuller JSON contract — see the [Agent guide](https://grit-scm.com/docs/agents/index.md).

# https://grit-scm.com/docs/agents/index.md

# Agent guide

> Run grit from scripts and autonomous agents — JSON output, exit codes, non-interactive use, credentials, and where to read the docs.

This page is for programs and agents that drive [`grit`](https://grit-scm.com/docs/install/index.md) in automation. It describes **existing** CLI behavior only. For field-level JSON schemas, use each command's docs page; this guide covers the shared contract.

Examples below were captured on 2026-10-08 with `grit` built from this repository (`cargo build --release -p grit-cli`), in a temporary repository on branch `main`, unless noted.

## Prefer `--json` (and `--filter`)

Pass **`--json`** on every command when something other than a human reads the result. In that mode:

- **stdout** carries exactly **one** JSON value: the command's outcome object on success, or `{"error": "…"}` on failure.
- **stderr** carries progress lines, auth prompts, and human-style `error: …` diagnostics. Parsers should read **stdout only** for the outcome.
- The process still exits non-zero on failure, so you can branch on the exit code **or** the presence of an `error` key.

Use **`--filter`** with a [jq](https://jqlang.org)-like expression to narrow the JSON without piping through `jq`. It requires `--json`. Multiple filter results become one JSON array.

```console
$ grit status --json --filter '{branch, clean}'
{
  "branch": "main",
  "clean": false
}

$ grit log --json --filter '.commits[].subject'
[
  "Say hi",
  "Start the project"
]
```

Plain `grit` with no subcommand is the same as [`grit status`](https://grit-scm.com/docs/status/index.md); `--json` works there too:

```console
$ grit --json --filter '{branch, clean}'
{
  "branch": "main",
  "clean": false
}
```

See [Scripting with grit](https://grit-scm.com/docs/scripting/index.md) for more `--filter` patterns. Per-command keys are documented under [The grit CLI](https://grit-scm.com/docs/global-options/index.md) command pages.

## Error object shape

On failure with `--json`, stdout is a single object with an **`error`** string (the same message a human sees after `error:` on stderr):

```console
$ grit commit --json
{"error":"provide a commit message, e.g. grit commit \"what changed\""}
```

If `--filter` is set, it runs on `{"error": "…"}` like any other JSON value. A bad filter expression also yields `{"error": "…"}` on stdout:

```console
$ grit status --json --filter 'invalid!!!'
{"error":"invalid filter \"invalid!!!\": [(File { code: \"invalid!!!\", path: () }, Parse([(Nothing, \"!!!\")]))]"}
```

In **human** mode, errors print to **stderr** and stdout stays empty:

```console
$ grit commit
error: provide a commit message, e.g. grit commit "what changed"
```

## Exit codes

| Code | Meaning |
| --- | --- |
| **0** | Success (including some jq filters that evaluate to `null` when a path is missing). |
| **1** | Command failed (missing repo, rejected push, validation error, and similar). With `--json`, stdout still contains `{"error": "…"}`. |
| **2** | Usage error (unknown option or subcommand). Clap prints help to stderr; there is no JSON error object on stdout. |

```console
$ grit --nope
error: unexpected argument '--nope' found

Usage: grit [OPTIONS] [COMMAND]

For more information, try '--help'.
```

```console
$ grit status --filter '.branch'
error: --filter requires --json
```

[`grit push`](https://grit-scm.com/docs/push/index.md) can exit **1** even after printing a full JSON outcome when a ref was rejected (same idea as Git: the outcome is reported, but the operation failed).

## Color and terminal detection

Human output uses ANSI color only when **stdout is a terminal**, the environment supports ANSI (`grit_lib::terminal::ansi_supported()`), and **`NO_COLOR` is unset** ([no-color.org](https://no-color.org)). Piped or redirected stdout is plain text; set `NO_COLOR=1` to force plain text on a TTY.

JSON mode never depends on color; always pass `--json` for machines.

## Stay non-interactive

`grit` does **not** open an external editor for commit messages. [`grit commit`](https://grit-scm.com/docs/commit/index.md) requires a message on the command line (positional or `-m`); there is no `$GIT_EDITOR` path.

Commands and flows that can **block on a person**:

| Area | Behavior | How to avoid blocking |
| --- | --- | --- |
| [`grit auth`](https://grit-scm.com/docs/auth/index.md) | OAuth device flow: instructions on stderr, then polls GitHub until you authorize in a browser. | Run `grit auth` once in an interactive session before automation. Store the token via `credential.helper` (see below). |
| [`grit fetch`](https://grit-scm.com/docs/fetch/index.md) / [`grit push`](https://grit-scm.com/docs/push/index.md) / [`grit pull`](https://grit-scm.com/docs/pull/index.md) | On HTTPS `github.com` auth failure, if **stdin is a TTY**, grit may ask `Sign in to GitHub now? [Y/n]` and run the device flow. | Use a stored token from `grit auth`, or ensure stdin is not a TTY so grit prints a hint and exits without prompting. |
| [`grit clone`](https://grit-scm.com/docs/clone/index.md) / [`grit fetch`](https://grit-scm.com/docs/fetch/index.md) / [`grit pull`](https://grit-scm.com/docs/pull/index.md) / [`grit push`](https://grit-scm.com/docs/push/index.md) over **SSH** | Grit runs `ssh` with protocol stdin/stdout piped but **stderr inherited**, so the SSH child can still talk to `/dev/tty` (host-key confirmation, passphrase prompts, `ssh-askpass`, and similar). `--json` does not disable this. | Preconfigure keys and known hosts; for unattended jobs set `GIT_SSH_COMMAND='ssh -o BatchMode=yes'` so SSH fails fast instead of prompting (grit resolves SSH from `GIT_SSH_COMMAND`, then `GIT_SSH`, then `ssh` on `PATH` — not `core.sshCommand`). |
| [`grit update`](https://grit-scm.com/docs/update/index.md) | Runs the release installer (`curl \| sh` on Unix). | Do not invoke in unattended jobs unless you intend to upgrade the binary. |

There is no global `--yes` flag. For commits, always pass `-m` / a message argument.

[`grit auth logout`](https://grit-scm.com/docs/auth/index.md) is non-interactive and reports whether a helper had a token to erase:

```console
$ grit auth logout --json
{
  "logged_out": false,
  "host": "github.com"
}
```

### Plumbing commands (not JSON)

These speak binary or line protocols on stdin/stdout and have **no** `--json` mode: [`grit manager`](https://grit-scm.com/docs/manager/index.md), [`grit upload-pack`](https://grit-scm.com/docs/upload-pack/index.md), [`grit receive-pack`](https://grit-scm.com/docs/receive-pack/index.md). Agents should not wrap them as JSON commands.

`-V` / `--version` prints a version string and does not emit JSON even if `--json` is passed:

```console
$ grit --json --version
grit 0.5.1
```

## Credentials (HTTPS GitHub)

For `https://github.com` remotes, sign in once with [`grit auth`](https://grit-scm.com/docs/auth/index.md). Grit uses GitHub's device flow, then stores an access token through your configured **`credential.helper`** (same mechanism as Git). Override the OAuth app id with `GRIT_GITHUB_CLIENT_ID` or `grit config --global grit.githubClientId …`.

Sign out with `grit auth logout`. You need a credential helper configured (for example `grit config --global credential.helper store` on Linux — see the [`grit auth`](https://grit-scm.com/docs/auth/index.md) page).

SSH remotes do not use the GitHub device-flow token; they can still block on SSH prompts (see the SSH row above).

## Documentation for agents

| Resource | URL / path | Use |
| --- | --- | --- |
| **Agent skill** | [`grit skill`](https://grit-scm.com/docs/skill/index.md) | A `SKILL.md` for coding agents, printed by the installed binary: `grit skill > .agents/skills/grit/SKILL.md`. |
| **llms.txt** | [grit-scm.com/llms.txt](https://grit-scm.com/llms.txt) | Curated index of every docs page (llmstxt.org format). |
| **llms-full.txt** | [grit-scm.com/llms-full.txt](https://grit-scm.com/llms-full.txt) | Full text of all docs Markdown twins in site order. |
| **Markdown twin** | Same URL as HTML but `index.md` instead of `index.html` (e.g. [status/index.md](https://grit-scm.com/docs/status/index.md)) | One page per command or guide; linked from HTML as "Markdown". |
| **Command JSON fields** | Under [The grit CLI](https://grit-scm.com/docs/global-options/index.md) | Each command page lists real `--json` examples and field tables. |
| **grit-lib API** | [docs.rs/grit-lib](https://docs.rs/grit-lib) | Rust library reference when embedding grit in your own binary. |
| **Library guide** | [Library guide](https://grit-scm.com/docs/library/index.md) | Narrative Rust workflows on top of `grit-lib`. |

Site generation keeps `docs/llms.txt` and `docs/llms-full.txt` in the repository in sync with [site.toml](https://github.com/gitbutlerapp/grit/blob/main/content/docs/site.toml); after local doc edits, run `make docs`.

## See also

- [Scripting with grit](https://grit-scm.com/docs/scripting/index.md) — `--json`, `--filter`, and exit codes in brief
- [Global options](https://grit-scm.com/docs/global-options/index.md) — flags shared by every command
- [Docs overview](https://grit-scm.com/docs/index.md) — how the site is organized

# https://grit-scm.com/docs/init/index.md

# grit init

> Create a new, empty repository.

## Synopsis

```text
grit init [--bare] [--ref-format <files|reftable>] [<path>]
```

Global options [`--json`](https://grit-scm.com/docs/global-options/index.md) and [`--markdown`](https://grit-scm.com/docs/global-options/index.md) apply.

## Description

Creates an empty Git repository at `<path>`, or in the current directory when no path is given. The directory is created if it doesn't exist. A normal repository keeps its data in a `.git` directory next to your files. A bare repository has no working tree; the repository data is the directory itself, which is what you want for a repository that other people push to.

The first branch is `main`. It has no commits until you make one.

Use `--ref-format reftable` to initialize with Git's reftable ref backend (requires a reftable-capable Git for interoperability). The default is `files` (loose refs under `refs/`).

## Options

| Option | Description |
| --- | --- |
| `<path>` | Where to create the repository. Defaults to the current directory. |
| `--bare` | Create a bare repository, with no working tree. |
| `--ref-format` | Ref storage backend: `files` (default) or `reftable`. |

## Examples

Start a new project in a new directory:

```console
$ grit init project
Initialized empty repository in /home/ada/project/.git (ref-format: files)
```

Turn the current directory into a repository:

```console
$ grit init
```

Create a bare repository to use as a shared remote:

```console
$ grit init --bare /srv/git/project.git
Initialized empty bare repository in /srv/git/project.git (ref-format: files)
```

Initialize with the reftable ref backend:

```console
$ grit init --ref-format reftable reftable-demo
Initialized empty repository in /home/ada/reftable-demo/.git (ref-format: reftable)
```

## JSON output

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `initialized` | boolean | Whether the repository was created successfully. |
| `path` | string | Repository directory (the `.git` directory for a normal repository). |
| `bare` | boolean | Whether the repository is bare. |
| `branch` | string | Initial branch name. |
| `ref_format` | string | Ref storage backend (`files` or `reftable`). |

Example:

```json
{
  "initialized": true,
  "path": "/home/ada/project/.git",
  "bare": false,
  "branch": "main",
  "ref_format": "files"
}
```

## Markdown output

Pass [`--markdown`](https://grit-scm.com/docs/global-options/index.md) for a short prose summary:

```text
Initialized empty repository at `/home/ada/project/.git` (default branch `main`, ref-format `reftable`).
```

## See also

[grit clone](https://grit-scm.com/docs/clone/index.md), [grit remote](https://grit-scm.com/docs/remote/index.md)

# https://grit-scm.com/docs/clone/index.md

# grit clone

> Copy a remote repository into a new directory.

## Synopsis

```text
grit clone <url> [<dir>]
```

## Description

Copies the repository at `<url>` into a new directory and checks out the remote's default branch. The remote is saved as `origin`, so [`grit fetch`](https://grit-scm.com/docs/fetch/index.md), [`grit pull`](https://grit-scm.com/docs/pull/index.md) and [`grit push`](https://grit-scm.com/docs/push/index.md) work with no further setup.

`<url>` can be an `https://` or `http://` URL, an `ssh://` URL or `user@host:path` address, a `git://` URL, a `file://` URL, a path to a local repository, or a path to a [git bundle](https://git-scm.com/docs/git-bundle) file (recognized by its header signature). A local path is stored as an absolute path so that later fetches work from anywhere.

Without `<dir>`, the directory is named after the last part of the URL with any `.git` suffix removed, so `https://github.com/gitbutlerapp/grit.git` clones into `grit`. The destination must not exist or must be empty.

For private GitHub repositories over HTTPS, sign in first with [`grit auth`](https://grit-scm.com/docs/auth/index.md).

You can clone from a shallow repository (including one created with `git clone --depth`); grit copies the shallow boundary into the new repository so history matches the source.

## Options

| Option | Description |
| --- | --- |
| `<url>` | The repository to clone: a URL or a local path. |
| `<dir>` | The directory to clone into. Defaults to the repository's name. |

## Examples

Clone a repository from GitHub:

```console
$ grit clone https://github.com/gitbutlerapp/grit.git
Cloning into 'grit' ...
Cloned into 'grit' on branch main.
```

Clone into a directory with a different name:

```console
$ grit clone git@github.com:gitbutlerapp/grit.git grit-src
```

Clone a local repository:

```console
$ grit clone /srv/git/project.git
```

## JSON output

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `url` | string | The URL or path that was cloned. |
| `path` | string | Directory the clone was created in. |
| `branch` | string | Branch that was checked out. |

Example:

```json
{
  "url": "/srv/git/project.git",
  "path": "project",
  "branch": "main"
}
```

## See also

[grit init](https://grit-scm.com/docs/init/index.md), [grit remote](https://grit-scm.com/docs/remote/index.md), [grit fetch](https://grit-scm.com/docs/fetch/index.md), [grit auth](https://grit-scm.com/docs/auth/index.md)

# https://grit-scm.com/docs/status/index.md

# grit status

> Show where you are and what's changed. This is what plain grit runs.

## Synopsis

```text
grit
grit status
grit st
```

## Description

The status screen is your home base. It shows, from top to bottom:

- **Where you are.** The current branch and how it compares with its target branch: how many commits ahead it is, followed by those commits (up to ten). A branch with no commits says `no commits yet`, and a detached HEAD shows the commit it's on.
- **Staged** changes, which are ready to commit.
- **Changed (not staged)** files, which have been modified since they were last staged.
- **Untracked** files, which `grit` isn't tracking yet.
- **A hint** with the next command to run.

Each change has a label: `new`, `modified`, `deleted`, `renamed`, `copied`, `type changed` or `conflict`.

Paths are shown relative to the directory you run `grit` from.

### The target branch

The target is the branch your work is headed for. `grit` uses the first of these that exists:

1. the branch named in the `target.branch` config setting
2. `origin/master`
3. `origin/main`
4. `master`
5. `main`

To compare against something else, set it for the repository:

```console
$ grit config target.branch origin/develop
```

## Options

`grit status` takes no options beyond the [global ones](https://grit-scm.com/docs/global-options/index.md).

## Examples

A branch with work in progress:

```console
$ grit
On feature  ·  2 ahead of origin/main

  cf18394  ada  2 hours ago  Say hi
  9a1c2e0  ada  3 hours ago  Add a greeting test

Staged
  +  new           notes.md

Changed (not staged)
  ~  modified      main.rs

Untracked
  ?  untracked     scratch.txt

→ grit add <file> to stage  ·  grit commit "message" to commit
```

Everything committed and pushed:

```console
$ grit st
On main  ·  even with origin/main

Nothing to commit — working tree clean.
```

Merge in progress with a conflict:

```console
$ grit status
On main  ·  merging — resolve conflicts

Staged
  !  conflict      f

→ resolve conflicts, then grit commit "message"
```

Check from a script whether the working tree is clean:

```console
$ grit status --json --filter .clean
true
```

## JSON output

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `branch` | string or null | Current branch, or `null` when HEAD is detached. |
| `detached` | boolean | Whether HEAD is detached. |
| `head` | string or null | Full id of the current commit, or `null` before the first commit. |
| `target` | string or null | Target branch, or `null` when none was found. |
| `ahead` | number | Commits on the branch that the target does not have. |
| `commits` | array | Newest of those commits, up to ten, each with `oid` and `subject`. |
| `staged` | array | Staged changes with `path` and `status`. |
| `unstaged` | array | Unstaged changes with `path` and `status`. |
| `untracked` | array | Paths of untracked files. |
| `clean` | boolean | `true` when there is nothing to commit and nothing untracked. |
| `merging` | boolean | `true` when a merge is in progress (`MERGE_HEAD` exists). |
| `in_progress` | array | Stable operation ids while work is paused (for example `merge`, `rebase`). Omitted when empty. |
| `conflicts` | array | Paths with unmerged index stages. Omitted when empty. |

Example:

```json
{
  "branch": "feature",
  "detached": false,
  "head": "cf18394a62c3f845bd9c44927a5a55e014b2a99d",
  "target": "origin/main",
  "ahead": 2,
  "commits": [
    { "oid": "cf18394a62c3f845bd9c44927a5a55e014b2a99d", "subject": "Say hi" },
    { "oid": "9a1c2e0d4b6f8a1c3e5d7f9b0a2c4e6d8f0a1b3c", "subject": "Add a greeting test" }
  ],
  "staged": [
    { "path": "notes.md", "status": "added" }
  ],
  "unstaged": [
    { "path": "main.rs", "status": "modified" }
  ],
  "untracked": ["scratch.txt"],
  "clean": false
}
```

## Markdown output

Pass [`--markdown`](https://grit-scm.com/docs/global-options/index.md) for headed sections that mirror the status screen:

```text
On **`feature`** · **2** ahead of `origin/main`

- `cf18394` Say hi (ada, 1 hour ago)

Staged
- **new** `notes.md`

Changed (not staged)
- **modified** `main.rs`

Untracked
- `scratch.txt`
```

## See also

[grit shortlog](https://grit-scm.com/docs/shortlog/index.md), [grit diff](https://grit-scm.com/docs/diff/index.md), [grit add](https://grit-scm.com/docs/add/index.md), [grit commit](https://grit-scm.com/docs/commit/index.md)

# https://grit-scm.com/docs/config/index.md

# grit config

> Read, set, list or remove configuration values.

## Synopsis

```text
grit config [--global] <key>
grit config [--global] <key> <value>
grit config [--global] --unset <key>
grit config [--global] --list
```

## Description

Reads and writes Git configuration. `grit` uses the same config files as Git: the repository's `.git/config`, and your per-user `~/.gitconfig`. Settings you've made with Git apply to `grit`, and the other way around.

With just a key, `grit config` prints its value. Reads look at every config file, with the repository's settings taking priority over your global ones. With a key and a value, it sets the value in the repository's config. Add `--global` to read or write your per-user config instead.

Keys are written as `section.name`, for example `user.email`.

### Settings grit uses

| Key | Used for |
| --- | --- |
| `user.name`, `user.email` | The author and committer of new commits. |
| `target.branch` | The branch [`grit status`](https://grit-scm.com/docs/status/index.md) and [`grit shortlog`](https://grit-scm.com/docs/shortlog/index.md) compare against. |
| `branch.<name>.remote` | Which remote [`grit push`](https://grit-scm.com/docs/push/index.md) and [`grit pull`](https://grit-scm.com/docs/pull/index.md) use for a branch. Defaults to `origin`. |
| `branch.<name>.merge` | Which remote branch a branch pushes to and pulls from. Defaults to the same name. |
| `credential.helper` | Where HTTPS credentials are stored and looked up. See [`grit auth`](https://grit-scm.com/docs/auth/index.md). |
| `grit.githubClientId` | The GitHub OAuth app [`grit auth`](https://grit-scm.com/docs/auth/index.md) signs in with. |
| `receive.denyNonFastForwards` | Refuse pushes that would discard commits. Enforced by [`grit receive-pack`](https://grit-scm.com/docs/receive-pack/index.md). |
| `receive.denyDeletes` | Refuse pushes that delete branches or tags. |
| `receive.denyCurrentBranch` | Refuse pushes to the branch checked out in a non-bare repository. On by default. |

## Options

| Option | Description |
| --- | --- |
| `<key>` | The setting to read, set or remove. |
| `<value>` | The value to set. Omit it to read the current value. |
| `--global` | Use your per-user config (`~/.gitconfig`) instead of the repository's. |
| `-l`, `--list` | List every setting as `key=value`. |
| `--unset` | Remove the setting. |

Reading or removing a key that isn't set is an error.

## Examples

Set your identity for every repository:

```console
$ grit config --global user.name "Ada Lovelace"
$ grit config --global user.email ada@example.com
```

Read a value:

```console
$ grit config user.email
ada@example.com
```

Use a different email address in one repository:

```console
$ grit config user.email ada@work.example
```

List everything:

```console
$ grit config --list
user.name=Ada Lovelace
user.email=ada@example.com
core.repositoryformatversion=0
core.bare=false
remote.origin.url=https://github.com/ada/project.git
remote.origin.fetch=+refs/heads/*:refs/remotes/origin/*
```

Remove a setting:

```console
$ grit config --unset target.branch
```

## JSON output

Pass `--json` for stable, scripting-friendly output. The object's `action` field says what happened:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `action` | string | `get`, `set`, `unset`, or `list`. |
| `key` | string | Configuration key for `get`, `set`, or `unset`. |
| `value` | string | Value for `get` or `set`. |
| `entries` | array | For `list`, each entry with `key` and optional `value`. |

Reading a value:

```json
{
  "action": "get",
  "key": "user.name",
  "value": "Ada Lovelace"
}
```

Listing values:

```json
{
  "action": "list",
  "entries": [
    { "key": "user.name", "value": "Ada Lovelace" }
  ]
}
```

## See also

[grit auth](https://grit-scm.com/docs/auth/index.md), [grit status](https://grit-scm.com/docs/status/index.md)

# https://grit-scm.com/docs/add/index.md

# grit add

> Stage changes. With no paths, stages everything.

## Synopsis

```text
grit add [<path>...]
```

## Description

Stages changes so they show up under **Staged** in [`grit status`](https://grit-scm.com/docs/status/index.md). With no paths, `grit add` stages everything `grit status` reports: modified files, deleted files and untracked files. With paths, it stages only those files, or everything under those directories.

Paths are relative to your current directory, so from inside `src/`, `grit add main.rs` stages `src/main.rs`. Paths like `../README.md` and absolute paths inside the repository work too. A path that matches nothing is an error, and nothing is staged.

[`grit commit`](https://grit-scm.com/docs/commit/index.md) stages every change before it records a commit, so you don't need `grit add` to commit. It's useful for starting to track a new file and checking what will be committed.

## Options

| Option | Description |
| --- | --- |
| `<path>...` | Files or directories to stage. Omit to stage every change. |

## Examples

Stage everything:

```console
$ grit add
Staged 3 changes.
```

Stage one file and a directory:

```console
$ grit add README.md src/
Staged 2 changes.
```

## JSON output

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `staged` | number | How many paths were staged by this invocation. |

Example:

```json
{
  "staged": 2
}
```

## See also

[grit status](https://grit-scm.com/docs/status/index.md), [grit commit](https://grit-scm.com/docs/commit/index.md)

# https://grit-scm.com/docs/commit/index.md

# grit commit

> Stage every change and record a new commit.

## Synopsis

```text
grit commit [-a] <message>
grit commit [-a] -m <message>
```

## Description

Stages every change in the working tree (modified, deleted and untracked files) and records a new commit on the current branch with the given message. This is the same as running [`grit add`](https://grit-scm.com/docs/add/index.md) followed by a commit.

The message can be given as an argument or with `-m`. A message with several lines is recorded as is; its first line is the subject that [`grit log`](https://grit-scm.com/docs/log/index.md) shows. A commit needs a message: without one, `grit commit` stages your changes and stops with an error.

The author and committer come from the `user.name` and `user.email` settings (see [`grit config`](https://grit-scm.com/docs/config/index.md)). To set the recorded time, use the `GIT_AUTHOR_DATE` and `GIT_COMMITTER_DATE` environment variables.

When `commit.gpgsign` is true (or `commit.gpgSign`), `grit commit` signs the new commit object the same way Git does: OpenPGP and X.509 via your configured `gpg`/`gpgsm` program, or SSH via `ssh-keygen -Y sign` when `gpg.format` is `ssh`. Signing uses `user.signingkey` and the same `gpg.*` settings Git reads. [`grit merge`](https://grit-scm.com/docs/merge/index.md) and [`grit pick`](https://grit-scm.com/docs/pick/index.md) honor the same policy when they create commits.

`grit commit` fails when there's nothing to commit, when HEAD is detached rather than on a branch, and when the index still has **unmerged** paths (for example after a merge conflict). Resolve conflicts, stage the result with [`grit add`](https://grit-scm.com/docs/add/index.md), then commit again.

When a merge is in progress (`MERGE_HEAD` is present) and every conflict is resolved in the index, `grit commit` records a **merge commit** with two parents (your branch tip and the merged tip) and clears merge state the same way Git does after `git commit`.

## Options

| Option | Description |
| --- | --- |
| `<message>` | The commit message. |
| `-m`, `--message <message>` | The commit message, as an option. Use either this or the argument, not both. |
| `-a`, `--all` | Stage every change first. This is always what `grit commit` does; the option is accepted so that `git` habits like `-am` keep working. |

## Examples

Commit everything:

```console
$ grit commit "Add the greeting"
[main 217c6f9] Add the greeting
2 changes committed
```

The same, written the way you'd write it for `git`:

```console
$ grit commit -am "Add the greeting"
```

A longer message with a body:

```console
$ grit commit -m "Add the greeting

Prints hi on startup so we know the binary runs."
```

## JSON output

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `oid` | string | Full commit object id (40 hex chars for SHA-1) |
| `branch` | string | Short branch name (e.g. `main`) |
| `subject` | string | First line of the commit message |
| `changes` | number | Path changes vs the parent commit tree |

Example:

```json
{
  "oid": "92501f188ae0815af09a0cef6e121eddda4113cf",
  "branch": "main",
  "subject": "second",
  "changes": 1
}
```

## See also

[grit add](https://grit-scm.com/docs/add/index.md), [grit status](https://grit-scm.com/docs/status/index.md), [grit log](https://grit-scm.com/docs/log/index.md), [grit config](https://grit-scm.com/docs/config/index.md)

# https://grit-scm.com/docs/diff/index.md

# grit diff

> Show uncommitted changes, or the change a commit introduced.

## Synopsis

```text
grit diff [<commit>]
```

## Description

With no argument, shows every uncommitted change: the working tree, staged or not, compared with the last commit. With a commit, shows the change that commit introduced, compared with its first parent.

Each file starts with its path, and each hunk with its position. Lines have two number columns, the line number before and after the change. In a terminal, removed lines are red, added lines are green, and the words that changed within a line are highlighted. When the output isn't a terminal, or `NO_COLOR` is set, lines are marked with `-` and `+` instead. Three lines of unchanged context are shown around each change. Binary files are reported but not shown.

`<commit>` can be anything that names a commit: a full or short commit id, a branch or tag name, or an expression like `HEAD~2`.

## Options

| Option | Description |
| --- | --- |
| `<commit>` | The commit whose change to show. Omit to show uncommitted changes. |

## Examples

What have I changed since the last commit?

```console
$ grit diff

main.rs
@@ -1 +1 @@
 1    │ - fn main() {}
    1 │ + fn main() {
    2 │ +     println!("hi");
    3 │ + }
```

What did the commit before the last one change?

```console
$ grit diff HEAD~1
```

List the files changed by a commit:

```console
$ grit diff v0.1 --json --filter '.files[].path'
[
  "README.md",
  "main.rs"
]
```

## JSON output

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `files` | array | Changed files, each with `path`, `status`, `binary`, and `hunks`. |

Nested fields include `files[].status` (`added`, `modified`, `deleted`, and so on), `hunks[].lines[].kind` (`context`, `add`, or `del`), line numbers on `old` and `new`, and `segments` with optional `emphasis` on intra-line changes.

Example:

```json
{
  "files": [
    {
      "path": "main.rs",
      "status": "modified",
      "binary": false,
      "hunks": [
        {
          "old_start": 1,
          "new_start": 1,
          "lines": [
            {
              "kind": "del",
              "old": 1,
              "segments": [
                { "text": "fn main() ", "emphasis": false },
                { "text": "{}", "emphasis": true }
              ]
            },
            {
              "kind": "add",
              "new": 1,
              "segments": [
                { "text": "fn main() ", "emphasis": false },
                { "text": "{", "emphasis": true }
              ]
            }
          ]
        }
      ]
    }
  ]
}
```

## Markdown output

Pass [`--markdown`](https://grit-scm.com/docs/global-options/index.md) for one `diff`-fenced unified patch per changed file:

```text
File: main.rs

@@ -1,1 +1,2 @@
 fn main() {
+    println!("hello");
 }
```

Each changed file is introduced with a level-2 heading on stdout, followed by a `diff`-fenced patch.

## See also

[grit status](https://grit-scm.com/docs/status/index.md), [grit show](https://grit-scm.com/docs/show/index.md), [grit log](https://grit-scm.com/docs/log/index.md)

# https://grit-scm.com/docs/restore/index.md

# grit restore

> Restore working tree and/or index paths from HEAD, the index, or another revision.

## Synopsis

```text
grit restore [--staged] [--worktree] [--source=<rev>] -- <path>…
```

## Description

Discards local changes for tracked paths you name. By default, `grit restore` resets the **working tree** to match the **index** (the same idea as unstaging worktree edits while keeping the index as-is).

- `--staged` resets the **index** from `HEAD` (or from `--source` when you pass it). On an unborn branch, staged paths are removed from the index.
- `--worktree` resets the **working tree**. When you omit both flags, only the worktree is restored.
- `--source=<rev>` reads content from a commit or tree instead of the usual default (`HEAD` for `--staged`, the index for the worktree).
- Paths that are missing in the source are removed from the target. Untracked files are never touched.

## Options

| Option | Description |
| --- | --- |
| `<path>…` | One or more pathspecs (required). |
| `-S`, `--staged` | Restore the index. |
| `-W`, `--worktree` | Restore the working tree. |
| `--source` | Commit or tree to restore from. |

## Examples

Discard unstaged edits to a file (worktree ← index):

```console
$ grit restore README.md
Restored README.md
```

Unstage a file (index ← HEAD):

```console
$ grit restore --staged README.md
Restored README.md
```

Restore the working tree from the previous commit:

```console
$ grit restore --source=HEAD~1 -- README.md
Restored README.md
```

## JSON output

Pass `--json` for scripting:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `restored` | array of strings | Paths created or updated. |
| `removed` | array of strings | Paths removed because the source had no entry. |

Example:

```json
{
  "restored": ["README.md"],
  "removed": []
}
```

## Markdown output

Pass `--markdown` for agent-friendly bullet lists of restored and removed paths.

## See also

[grit add](https://grit-scm.com/docs/add/index.md), [grit status](https://grit-scm.com/docs/status/index.md)

# https://grit-scm.com/docs/log/index.md

# grit log

> Show recent commits, newest first, a page at a time.

## Synopsis

```text
grit log [--before <commit>]
```

## Description

Lists the commits reachable from the current commit, newest first, ten at a time. Each line shows the short commit id, the author, when the commit was made and its subject (the first line of its message). The author is shown by the part of their email address before the `@`, or by name when there's no email.

When there's more history, the last line shows the command for the next page.

## Options

| Option | Description |
| --- | --- |
| `--before <commit>` | Start listing at this commit instead of the current one. Use the commit from the `→ more` line to see the next page. |

## Examples

Show recent history:

```console
$ grit log
  b52cca6  ada  2 minutes ago  Handle empty input
  5fbefea  ada  1 hour ago     Add a greeting test
  ...
  2efcc2d  ada  3 days ago     Start the project

→ more: grit log --before=a7e3a5e
```

Show the next page:

```console
$ grit log --before=a7e3a5e
```

Get the subjects of the last ten commits:

```console
$ grit log --json --filter '.commits[].subject'
```

## JSON output

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `commits` | array | Up to ten commits, newest first, each with `oid` and `subject`. |
| `next` | string or null | Full commit id to pass to `--before` for the next page, or `null` when there is no more history. |

Example:

```json
{
  "commits": [
    {
      "oid": "92501f188ae0815af09a0cef6e121eddda4113cf",
      "subject": "second"
    },
    {
      "oid": "919c45f33de5e5c0bd05f8ffb089f697f4644976",
      "subject": "initial"
    }
  ],
  "next": null
}
```

## Markdown output

Pass [`--markdown`](https://grit-scm.com/docs/global-options/index.md) for a bullet list of commits (short oid, subject, author, relative date):

```text
- `b52cca6` Handle empty input (ada, 2 minutes ago)
- `5fbefea` Add a greeting test (ada, 1 hour ago)

More history: run `grit log --before=a7e3a5e`
```

## See also

[grit shortlog](https://grit-scm.com/docs/shortlog/index.md), [grit show](https://grit-scm.com/docs/show/index.md)

# https://grit-scm.com/docs/shortlog/index.md

# grit shortlog

> List the commits on this branch that aren't on the target branch yet.

## Synopsis

```text
grit shortlog
grit sl
```

## Description

Shows the current branch, its target branch, and every commit on the current branch that the target doesn't have, newest first. It's the list of what you would be proposing if you opened a pull request now.

The target branch is found the same way as for [`grit status`](https://grit-scm.com/docs/status/index.md#the-target-branch): the `target.branch` setting, then `origin/master`, `origin/main`, `master` and `main`. Unlike `grit status`, which shows at most ten commits, `grit shortlog` lists all of them.

## Options

`grit shortlog` takes no options beyond the [global ones](https://grit-scm.com/docs/global-options/index.md).

## Examples

```console
$ grit shortlog
On feature
Ahead of origin/main by 2 commits
  cf18394  ada  2 hours ago  Say hi
  9a1c2e0  ada  3 hours ago  Add a greeting test
```

Count the commits that aren't on the target yet:

```console
$ grit sl --json --filter .ahead
2
```

## JSON output

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `branch` | string | Current branch name. |
| `target` | string or null | Target branch used for comparison, or `null` when none was found. |
| `ahead` | number | How many commits are on the branch but not on the target. |
| `commits` | array | Those commits, newest first, each with `oid` and `subject`. |

Example:

```json
{
  "branch": "feature",
  "target": "main",
  "ahead": 0,
  "commits": []
}
```

## Markdown output

Pass [`--markdown`](https://grit-scm.com/docs/global-options/index.md) for a branch summary and commit bullets:

```text
# Branch `feature`

Ahead of `main` by **2** commits.

- `cf18394` Say hi (ada, 1 hour ago)
- `9a1c2e0` Add a greeting test (ada, 2 hours ago)
```

## See also

[grit status](https://grit-scm.com/docs/status/index.md), [grit log](https://grit-scm.com/docs/log/index.md), [grit config](https://grit-scm.com/docs/config/index.md)

# https://grit-scm.com/docs/show/index.md

# grit show

> Show a commit, tag or branch, and a summary of what it changed.

## Synopsis

```text
grit show [<object>]
```

## Description

Shows a commit's id, author, date and full message, followed by a summary of the files it changed and how many lines were added and removed in each. With no argument, it shows the current commit.

`<object>` can be a commit id (full or short), an expression like `HEAD~2`, a branch or a tag. For a branch or tag, the first line names it. For an annotated tag, the tag's own author, date and message come before the commit it points at.

To see the full change a commit made, use [`grit diff <commit>`](https://grit-scm.com/docs/diff/index.md).

## Options

| Option | Description |
| --- | --- |
| `<object>` | The commit, branch or tag to show. Defaults to the current commit. |

## Examples

Show the latest commit:

```console
$ grit show
branch feature
commit cf18394a62c3f845bd9c44927a5a55e014b2a99d
Author: Ada Lovelace <ada@example.com>
Date:   2026-10-07 10:00:00 +0000

    Say hi

 main.rs |   4 +++-
 1 file changed, 3 insertions(+), 1 deletion(-)
```

Show what a tag points at:

```console
$ grit show v0.1
```

Get the subject of a commit:

```console
$ grit show HEAD~1 --json --filter .commit.subject
"Start the project"
```

## JSON output

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `kind` | string | `commit`, `branch`, `tag`, or `annotated_tag`. |
| `ref_name` | string | Branch or tag name when you named a ref; omitted for a raw commit id. |
| `tag` | object | For an annotated tag: `name`, `tagger`, and `message`. |
| `commit` | object | Commit id, parents, author, committer, subject, and full message. |
| `stat` | object | Change stats with `files`, `files_changed`, `insertions`, and `deletions`. |

Example:

```json
{
  "kind": "branch",
  "ref_name": "feature",
  "commit": {
    "oid": "92501f188ae0815af09a0cef6e121eddda4113cf",
    "parents": ["919c45f33de5e5c0bd05f8ffb089f697f4644976"],
    "author": {
      "name": "Ada Lovelace",
      "email": "ada@example.com",
      "date": "2026-10-07 14:54:02 +0000"
    },
    "committer": {
      "name": "Ada Lovelace",
      "email": "ada@example.com",
      "date": "2026-10-07 14:54:02 +0000"
    },
    "subject": "second",
    "message": "second"
  },
  "stat": {
    "files": [
      {
        "path": "README.md",
        "status": "modified",
        "insertions": 1,
        "deletions": 0,
        "binary": false
      }
    ],
    "files_changed": 1,
    "insertions": 1,
    "deletions": 0
  }
}
```

## Markdown output

Pass [`--markdown`](https://grit-scm.com/docs/global-options/index.md) for headings, commit metadata, message body, and a diffstat table:

```text
# Commit `b52cca6`

**Oid:** `92501f188ae0815af09a0cef6e121eddda4113cf`
**Author:** Ada Lovelace <ada@example.com>
**Date:** 2026-10-07 14:54:02 +0000

Subject line as a level-2 heading, then a "Changes" section with a table:

| File | + | − |
| --- | ---: | ---: |
| `README.md` | 1 | 0 |

1 file changed, 1 insertion(+)
```

## See also

[grit diff](https://grit-scm.com/docs/diff/index.md), [grit log](https://grit-scm.com/docs/log/index.md), [grit tag](https://grit-scm.com/docs/tag/index.md)

# https://grit-scm.com/docs/branch/index.md

# grit branch

> List branches, or create or delete one.

## Synopsis

```text
grit branch
grit branch <name>
grit branch -d <name>
grit branch -D <name>
```

## Description

With no arguments, lists your local branches and marks the current one with `*`.

With a name, creates a branch at the current commit. It doesn't switch to the new branch; use [`grit switch -c`](https://grit-scm.com/docs/switch/index.md) to create a branch and switch to it in one step.

Branch names follow the same rules as `git check-ref-format --branch` (no spaces, `..`, `~`, `.lock`, reserved names like `HEAD`, and other characters Git rejects). Invalid names fail before anything is written under `.git/refs`.

With `-d`, deletes a branch. `grit` refuses if the branch has commits that aren't in the current branch, so you can't lose work by accident; `-D` deletes it anyway. You can't delete the branch you're on.

## Options

| Option | Description |
| --- | --- |
| `<name>` | The branch to create or delete. Omit to list branches. |
| `-d`, `--delete` | Delete the branch. It must be fully merged into the current branch. |
| `-D`, `--force` | Delete the branch even if it isn't merged. |

## Examples

```console
$ grit branch
  feature
* main

$ grit branch experiment
Created branch experiment

$ grit branch -d experiment
Deleted branch experiment (was cf18394).
```

Deleting a branch with unmerged work:

```console
$ grit branch -d spike
error: the branch 'spike' is not fully merged.
If you are sure you want to delete it, run 'grit branch -D spike'

$ grit branch -D spike
Deleted branch spike (was a7020c2).
```

## JSON output

Pass `--json` for stable, scripting-friendly output. The object's `action` field says what happened:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `action` | string | `list`, `create`, or `delete`. |
| `current` | string or null | For `list`, the current branch, or `null` when detached. |
| `branches` | array | For `list`, each branch with `name` and `current`. |
| `name` | string | For `create` or `delete`, the branch name. |
| `oid` | string | For `delete`, full id of the tip before deletion. |
| `short_oid` | string | For `delete`, abbreviated tip id. |

Listing branches:

```json
{
  "action": "list",
  "current": "main",
  "branches": [
    { "name": "feature", "current": false },
    { "name": "main", "current": true }
  ]
}
```

Creating a branch:

```json
{
  "action": "create",
  "name": "experiment"
}
```

## See also

[grit switch](https://grit-scm.com/docs/switch/index.md), [grit merge](https://grit-scm.com/docs/merge/index.md)

# https://grit-scm.com/docs/switch/index.md

# grit switch

> Switch to another branch, or create one and switch to it.

## Synopsis

```text
grit switch <branch>
grit switch -c <branch>
```

`grit checkout` and `grit co` do the same thing.

## Description

Switches to a local branch and updates the files in your working tree to match it.

`grit switch` won't run while you have uncommitted changes, staged or not, so that nothing gets carried over to the wrong branch by accident. Commit first. Untracked files stay where they are, unless the other branch has a file at the same path; then `grit` stops rather than overwrite it.

With `-c`, creates the branch at the current commit and switches to it.

## Options

| Option | Description |
| --- | --- |
| `<branch>` | The branch to switch to, or to create with `-c`. |
| `-c`, `--create` | Create the branch first, then switch to it. Fails if the branch already exists. |

## Examples

```console
$ grit switch -c feature
Created and switched to branch feature

$ grit switch main
Switched to branch main
```

With uncommitted changes:

```console
$ grit switch main
error: you have uncommitted changes — commit them before switching
```

## JSON output

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `branch` | string | The branch you switched to. |
| `created` | boolean | `true` when `-c` created the branch; otherwise `false`. |

Example:

```json
{
  "branch": "feature",
  "created": true
}
```

## See also

[grit branch](https://grit-scm.com/docs/branch/index.md), [grit merge](https://grit-scm.com/docs/merge/index.md), [grit status](https://grit-scm.com/docs/status/index.md)

# https://grit-scm.com/docs/merge/index.md

# grit merge

> Merge another branch into the current one.

## Synopsis

```text
grit merge <branch>
```

## Description

Brings the commits from `<branch>` into the current branch:

- If the current branch already has everything, nothing changes.
- If the current branch has no commits of its own since they diverged, it's moved forward to `<branch>` (a fast-forward). No new commit is made.
- Otherwise both sides are merged and a merge commit is recorded, with the message `Merge <branch>`. When `commit.gpgsign` is enabled, that merge commit is signed like [`grit commit`](https://grit-scm.com/docs/commit/index.md).

`<branch>` can be a local branch or a remote-tracking branch such as `origin/main`. Local branches are looked up first.

If both sides changed the same lines, `grit` lists the conflicting files, exits with an error, and leaves the branch and working tree exactly as they were. `grit` can't resolve conflicts yet; to finish the merge, run `git merge <branch>`, fix the conflicts and commit.

`grit merge` won't run with uncommitted changes, on a branch with no commits, or with a detached HEAD. It also refuses when an untracked file in the working tree would be replaced by a path the merge would check out (same rule as [`grit switch`](https://grit-scm.com/docs/switch/index.md)); the branch and file are left unchanged.

## Options

| Option | Description |
| --- | --- |
| `<branch>` | The branch to merge into the current one. |

## Examples

```console
$ grit merge feature
Fast-forwarded feature → cf18394

$ grit merge origin/main
Merged origin/main into the current branch (acd1d4b)
```

When both branches change the same lines:

```console
$ grit merge topic
error: merge has conflicts in:
  README.md

Nothing was changed. grit can't resolve conflicts yet — run `git merge topic` to resolve them.
```

When an untracked file would be overwritten:

```console
$ grit merge feature
error: untracked file 'notes.txt' would be overwritten — move or remove it first
```

## JSON output

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `result` | string | `up_to_date`, `fast_forward`, `merged`, or `set_upstream`. |
| `branch` | string | The branch that was merged in (or set as upstream). |
| `oid` | string or null | Commit the current branch points at after the merge, when applicable. |
| `upstream` | string or null | For `set_upstream`, the remote-tracking branch that was adopted. |

Example:

```json
{
  "result": "up_to_date",
  "branch": "main"
}
```

## See also

[grit pull](https://grit-scm.com/docs/pull/index.md), [grit pick](https://grit-scm.com/docs/pick/index.md), [grit branch](https://grit-scm.com/docs/branch/index.md)

# https://grit-scm.com/docs/pick/index.md

# grit pick

> Copy a single commit onto the current branch.

## Synopsis

```text
grit pick <commit>
```

## Description

Takes the change `<commit>` made and applies it to the current branch as a new commit (a cherry-pick). The new commit keeps the original author and message. When `commit.gpgsign` is enabled, the new commit is signed like [`grit commit`](https://grit-scm.com/docs/commit/index.md).

`<commit>` can be a full or short commit id, a branch name (to pick the commit at its tip) or an expression like `feature~2`.

`grit pick` works on one commit at a time and keeps it simple. It refuses, and changes nothing, when:

- you have uncommitted changes, or the current branch has no commits, or HEAD is detached
- `<commit>` is a merge commit
- `<commit>` doesn't change anything, or its change is already on the current branch
- the change conflicts with the current branch; the conflicting files are listed
- an untracked file would be replaced by a path the pick would check out (same rule as [`grit switch`](https://grit-scm.com/docs/switch/index.md))

## Options

| Option | Description |
| --- | --- |
| `<commit>` | The commit to copy onto the current branch. |

## Examples

Pick the latest commit from another branch:

```console
$ grit pick feature
Picked cf18394 → 4be20a1 Say hi
```

Pick a commit by id:

```console
$ grit pick 9a1c2e0
```

When an untracked file would be overwritten:

```console
$ grit pick feature
error: untracked file 'notes.txt' would be overwritten — move or remove it first
```

## JSON output

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `source` | string | Full id of the commit that was picked. |
| `oid` | string | Full id of the new commit on the current branch. |
| `subject` | string | Subject line of the picked commit. |

Example:

```json
{
  "source": "6e513d5cd6ab8d618238e22d7ca55d942b55964a",
  "oid": "6e513d5cd6ab8d618238e22d7ca55d942b55964a",
  "subject": "two"
}
```

## See also

[grit merge](https://grit-scm.com/docs/merge/index.md), [grit log](https://grit-scm.com/docs/log/index.md)

# https://grit-scm.com/docs/tag/index.md

# grit tag

> List tags, or create or delete one. A new tag points at the current commit.

## Synopsis

```text
grit tag
grit tag <name>
grit tag -d <name>
```

## Description

With no arguments, lists your tags. With a name, creates a tag pointing at the current commit; tags created by `grit` are lightweight tags, with no message of their own. With `-d`, deletes a tag.

Tag names must be valid as `refs/tags/<name>` under Git's ref-name rules (same checks as `git check-ref-format refs/tags/<name>`). Invalid names are rejected before writing under `.git/refs`.

Tags are local until you publish them with [`grit push --tags`](https://grit-scm.com/docs/push/index.md).

## Options

| Option | Description |
| --- | --- |
| `<name>` | The tag to create or delete. Omit to list tags. |
| `-d`, `--delete` | Delete the tag. |

## Examples

```console
$ grit tag v0.1
Created tag v0.1

$ grit tag
  v0.1

$ grit push --tags
  pushed --tags → origin refs/tags/v0.1

$ grit tag -d v0.1
Deleted tag v0.1
```

## JSON output

Pass `--json` for stable, scripting-friendly output. The object's `action` field says what happened:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `action` | string | `list`, `create`, or `delete`. |
| `tags` | array | For `list`, each tag with `name` and `oid`. |
| `name` | string | For `create` or `delete`, the tag name. |
| `oid` | string | For `create`, full id of the commit the tag points at. |

Listing tags:

```json
{
  "action": "list",
  "tags": [
    {
      "name": "v0.1",
      "oid": "92501f188ae0815af09a0cef6e121eddda4113cf"
    }
  ]
}
```

Creating a tag:

```json
{
  "action": "create",
  "name": "v0.1",
  "oid": "92501f188ae0815af09a0cef6e121eddda4113cf"
}
```

## See also

[grit push](https://grit-scm.com/docs/push/index.md), [grit show](https://grit-scm.com/docs/show/index.md)

# https://grit-scm.com/docs/remote/index.md

# grit remote

> List remotes, add one, or list refs on a remote.

## Synopsis

```text
grit remote
grit remote add <name> <url>
grit remote refs <remote-or-url> [--heads] [--tags] [<prefix>…]
```

## Description

A remote is another copy of the repository that you fetch from and push to, usually on a server. With no arguments, `grit remote` lists your remotes and their URLs.

`grit remote add` adds a remote. [`grit fetch`](https://grit-scm.com/docs/fetch/index.md) then stores the remote's branches as remote-tracking branches named `<name>/<branch>`, such as `origin/main`. [`grit clone`](https://grit-scm.com/docs/clone/index.md) adds a remote called `origin` for you.

`grit remote refs` lists references on a configured remote name or a literal URL/path (same transports as [`grit clone`](https://grit-scm.com/docs/clone/index.md)), matching `git ls-remote` ordering.

## Options

| Option | Description |
| --- | --- |
| `add <name> <url>` | Add a remote called `<name>` at `<url>`. Fails if a remote with that name exists. |
| `refs <remote-or-url>` | List refs on the remote (or URL). |
| `--heads` | With `refs`, only `refs/heads/`. |
| `--tags` | With `refs`, only `refs/tags/`. |
| `<prefix>…` | With `refs`, optional ref prefixes (same rules as `git ls-remote`). |

## Examples

```console
$ grit remote add origin https://github.com/ada/project.git
Added remote origin → https://github.com/ada/project.git

$ grit remote
origin	https://github.com/ada/project.git

$ grit remote refs origin
a1b2c3d4e5f6789012345678901234567890abcd	refs/heads/main
```

## JSON output

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `action` | string | `list`, `add`, or `refs`. |
| `remotes` | array | For `list`, each remote with `name` and `url`. |
| `name` | string | For `add`, the remote name. |
| `url` | string | For `add`, the remote URL or path. |
| `refs` | array | For `refs`, each entry with `name`, `oid`, optional `peeled`, optional `symref_target`. |

Listing remotes:

```json
{
  "action": "list",
  "remotes": [
    {
      "name": "origin",
      "url": "https://github.com/ada/project.git"
    }
  ]
}
```

Adding a remote:

```json
{
  "action": "add",
  "name": "origin",
  "url": "https://github.com/ada/project.git"
}
```

Listing refs:

```json
{
  "action": "refs",
  "refs": [
    {
      "name": "refs/heads/main",
      "oid": "a1b2c3d4e5f6789012345678901234567890abcd",
      "peeled": null,
      "symref_target": null
    }
  ]
}
```

## Markdown output

Pass `--markdown` for an agent-friendly Markdown table of refs (a “Remote refs” heading, then columns — not JSON). Example shape:

```text
Remote refs

| Ref | Object | Peeled | Symref target |
| --- | --- | --- | --- |
| `refs/heads/main` | `a1b2c3…` | — | — |
| `refs/tags/v1` | `tagoid…` | `commitoid…` | — |
```

## See also

[grit fetch](https://grit-scm.com/docs/fetch/index.md), [grit push](https://grit-scm.com/docs/push/index.md), [grit clone](https://grit-scm.com/docs/clone/index.md)

# https://grit-scm.com/docs/fetch/index.md

# grit fetch

> Download new commits, branches and tags from a remote.

## Synopsis

```text
grit fetch [<remote>]
```

## Description

Downloads everything new from a remote and updates your remote-tracking branches, such as `origin/main`, along with any new tags. Your own branches and working tree are left alone; to bring the new commits into your branch, use [`grit merge`](https://grit-scm.com/docs/merge/index.md) or [`grit pull`](https://grit-scm.com/docs/pull/index.md).

`<remote>` may be a configured remote name or a path to a [git bundle](https://git-scm.com/docs/git-bundle) file (detected by the bundle signature, not the file extension). Bundle fetches verify that prerequisite commits are already in your repository before applying the pack.

Each updated ref is listed with its old and new commit.

Repositories created as shallow clones with Git (for example `git clone --depth 1`) are supported: `grit fetch` respects the existing shallow boundary, does not create tag refs to missing commits, and leaves a clean repository when there is nothing new to download.

## Options

| Option | Description |
| --- | --- |
| `<remote>` | Configured remote name, or a path/URL to a repository or bundle file. Defaults to `origin`. |

## Examples

```console
$ grit fetch
  refs/remotes/origin/main  a8e620a → b52cca6
Fetched 1 update from origin.

$ grit fetch
Already up to date with origin.
```

Fetch from another remote:

```console
$ grit fetch upstream
```

## JSON output

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `remote` | string | Remote that was fetched. |
| `updates` | array | Ref updates, each with `ref`, `old_oid`, and `new_oid`. |
| `updated` | number | Count of refs that changed. |

`old_oid` is `null` for a ref that is new, and `new_oid` is `null` for one that was removed.

Example:

```json
{
  "remote": "origin",
  "updates": [],
  "updated": 0
}
```

## See also

[grit pull](https://grit-scm.com/docs/pull/index.md), [grit merge](https://grit-scm.com/docs/merge/index.md), [grit remote](https://grit-scm.com/docs/remote/index.md)

# https://grit-scm.com/docs/pull/index.md

# grit pull

> Fetch from the remote and bring the current branch up to date.

## Synopsis

```text
grit pull
```

## Description

Runs [`grit fetch`](https://grit-scm.com/docs/fetch/index.md), then merges the remote's copy of the current branch into it, as [`grit merge`](https://grit-scm.com/docs/merge/index.md) would: a fast-forward when you have no new commits of your own, and a merge commit when both sides do. On a branch with no commits yet, it simply takes the remote's.

The remote is the one in `branch.<name>.remote`, or `origin`. The remote branch is the one in `branch.<name>.merge`, or the branch with the same name as yours.

Like `grit merge`, `grit pull` won't run with uncommitted changes or a detached HEAD, and stops without changing anything if there are conflicts.

## Options

`grit pull` takes no options beyond the [global ones](https://grit-scm.com/docs/global-options/index.md).

## Examples

```console
$ grit pull
Fast-forwarded origin/main → b52cca6

$ grit pull
Merged origin/main into the current branch (acd1d4b)

$ grit pull
Already up to date.
```

## JSON output

The same shape as [`grit merge`](https://grit-scm.com/docs/merge/index.md#json-output). Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `result` | string | `up_to_date`, `fast_forward`, `merged`, or `set_upstream`. |
| `branch` | string | Remote branch that was integrated. |
| `oid` | string or null | Commit the current branch points at after the pull, when applicable. |
| `upstream` | string or null | For `set_upstream`, the remote-tracking branch that was adopted. |

Example:

```json
{
  "result": "fast_forward",
  "branch": "origin/main",
  "oid": "b52cca65378cf17309f0b1f917c6dc4fdd9256f2"
}
```

## See also

[grit fetch](https://grit-scm.com/docs/fetch/index.md), [grit merge](https://grit-scm.com/docs/merge/index.md), [grit push](https://grit-scm.com/docs/push/index.md)

# https://grit-scm.com/docs/push/index.md

# grit push

> Publish the current branch, or your tags, to a remote.

## Synopsis

```text
grit push
grit push --tags
```

## Description

Sends the current branch to the remote, creating the branch there if it doesn't exist. There are no arguments and no upstream to set: the branch goes to `origin`, under the same name. To push a branch somewhere else, set `branch.<name>.remote` and `branch.<name>.merge` with [`grit config`](https://grit-scm.com/docs/config/index.md).

`grit push` never overwrites commits on the remote. If the remote branch has commits you don't have, the push is rejected and `grit` exits with an error; run [`grit pull`](https://grit-scm.com/docs/pull/index.md) and push again.

With `--tags`, pushes every local tag to `origin` instead of the current branch.

For GitHub over HTTPS, if a push fails because you aren't signed in, `grit` offers to run [`grit auth`](https://grit-scm.com/docs/auth/index.md) and then tries again.

## Options

| Option | Description |
| --- | --- |
| `-t`, `--tags` | Push all tags instead of the current branch. |

## Examples

```console
$ grit push
  pushed main → origin refs/heads/main

$ grit push
  origin refs/heads/main already up to date

$ grit push --tags
  pushed --tags → origin refs/tags/v0.1
```

When someone else pushed first:

```console
$ grit push
  rejected origin refs/heads/main: not a fast-forward — run `grit pull` first
```

## JSON output

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `remote` | string | Remote that was pushed to. |
| `branch` | string | Branch that was pushed, or `--tags`. |
| `results` | array | Per-ref outcomes with `ref` and `status` (`ok`, `up_to_date`, or a rejection). |
| `rejected` | boolean | `true` if any ref was rejected (exit status is also `1`). |

Example:

```json
{
  "remote": "origin",
  "branch": "main",
  "results": [
    { "ref": "refs/heads/main", "status": "ok" }
  ],
  "rejected": false
}
```

## See also

[grit pull](https://grit-scm.com/docs/pull/index.md), [grit tag](https://grit-scm.com/docs/tag/index.md), [grit auth](https://grit-scm.com/docs/auth/index.md)

# https://grit-scm.com/docs/auth/index.md

# grit auth

> Sign in to GitHub so HTTPS pushes and fetches just work.

## Synopsis

```text
grit auth
grit auth logout
```

## Description

Signs you in to GitHub and saves the token, so that [`grit push`](https://grit-scm.com/docs/push/index.md), [`grit fetch`](https://grit-scm.com/docs/fetch/index.md) and [`grit clone`](https://grit-scm.com/docs/clone/index.md) work with private repositories over HTTPS.

`grit auth` uses GitHub's device flow. It prints a web address and a short code. Open `https://github.com/login/device` in your browser, enter the code and approve access. `grit` waits until you do, then saves the token with your credential helper. Every request goes straight to github.com.

A credential helper has to be configured to store the token. On Windows, if none is set, `grit` uses its built-in one ([`grit manager`](https://grit-scm.com/docs/manager/index.md)). Elsewhere, set one with [`grit config`](https://grit-scm.com/docs/config/index.md):

```console
$ grit config --global credential.helper osxkeychain   # macOS
$ grit config --global credential.helper libsecret     # Linux
$ grit config --global credential.helper store         # a plain-text file, any system
```

`grit auth logout` removes the saved GitHub token from your credential helper.

`grit` signs in with its own GitHub OAuth app. To use a different one, set its client id in the `GRIT_GITHUB_CLIENT_ID` environment variable or the `grit.githubClientId` setting.

## Options

| Option | Description |
| --- | --- |
| `logout` | Remove the saved GitHub token. |

## Examples

```console
$ grit auth
To authorize grit, open this page in your browser:

    https://github.com/login/device

and enter the code:

    1A2B-3C4D

Waiting for you to authorize… (press Ctrl-C to cancel)

✓ Signed in to GitHub — token stored for github.com.

$ grit auth logout
✓ Signed out of GitHub — removed the stored token for github.com.
```

## JSON output

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `authenticated` | boolean | For sign-in, whether a token was stored. |
| `logged_out` | boolean | For `logout`, whether a stored token was removed. |
| `host` | string | GitHub host the credential applies to (usually `github.com`). |

After sign-in:

```json
{
  "authenticated": true,
  "host": "github.com"
}
```

After logout:

```json
{
  "logged_out": true,
  "host": "github.com"
}
```

`logged_out` is `false` when no credential helper is configured, since there's nowhere a token could be stored.

## See also

[grit push](https://grit-scm.com/docs/push/index.md), [grit config](https://grit-scm.com/docs/config/index.md), [grit manager](https://grit-scm.com/docs/manager/index.md)

# https://grit-scm.com/docs/bundle/index.md

# grit bundle

> Create, verify, and list git bundle files for offline transfer.

## Synopsis

```text
grit bundle create <file> <rev>…
grit bundle verify <file>
grit bundle list <file>
```

## Description

Git bundles pack refs and objects into a single file. Use them to move history without a network remote, or to seed a clone from removable media.

`create` walks the given revision specs (same rules as [`grit log`](https://grit-scm.com/docs/log/index.md)), writes prerequisite commits the recipient must already have, lists tip refs in the header, and appends a pack stream. Bundles are detected by the `# v2 git bundle` / `# v3 git bundle` signature, not by the `.bundle` file extension alone.

`verify` checks the pack checksum and, when run inside a repository, confirms prerequisite commits are present and connected to your history.

`list` prints the refs recorded in the bundle header.

You can also treat a bundle path like a remote: [`grit fetch`](https://grit-scm.com/docs/fetch/index.md) and [`grit clone`](https://grit-scm.com/docs/clone/index.md) accept a path to a bundle file.

## Options

`create`:

| Argument | Description |
| --- | --- |
| `<file>` | Output bundle path. |
| `<rev>…` | One or more positive revision specs (commits, branches, ranges). |

`verify` / `list`:

| Argument | Description |
| --- | --- |
| `<file>` | Existing bundle file. |

## Examples

```console
$ grit bundle create backup.bundle main
Wrote bundle backup.bundle (1 refs).

$ grit bundle verify backup.bundle
backup.bundle is valid
The bundle uses this hash algorithm: sha1
The bundle contains 1 refs:
f4e623f540cd491f8856e9487a3da5f8242307f0 refs/heads/main

$ grit bundle list backup.bundle
f4e623f540cd491f8856e9487a3da5f8242307f0 refs/heads/main
```

## JSON output

`grit bundle verify --json`:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `path` | string | Bundle file path. |
| `ok` | boolean | Whether verification succeeded. |
| `hash_algorithm` | string | Object hash used by the pack (`sha1` or `sha256`). |
| `references` | array | Each entry has `name` and `oid`. |
| `prerequisites` | array of strings | Required commit ids (hex). |
| `missing_prerequisites` | array of strings | Prerequisite ids absent from the current repo (empty when not in a repo or when `ok` is true). |

```json
{
  "path": "backup.bundle",
  "ok": true,
  "hash_algorithm": "sha1",
  "references": [
    {
      "name": "refs/heads/main",
      "oid": "f4e623f540cd491f8856e9487a3da5f8242307f0"
    }
  ],
  "prerequisites": [],
  "missing_prerequisites": []
}
```

`grit bundle list --json`:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `path` | string | Bundle file path. |
| `references` | array | Refs from the bundle header (`name`, `oid`). |

```json
{
  "path": "backup.bundle",
  "references": [
    {
      "name": "refs/heads/main",
      "oid": "f4e623f540cd491f8856e9487a3da5f8242307f0"
    }
  ]
}
```

`grit bundle create --json`:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `path` | string | Output bundle path. |
| `refs` | number | Count of refs written to the header. |

## Markdown output

Pass `--markdown` on any `grit bundle` subcommand for the same JSON document wrapped for agents (see [Global options](https://grit-scm.com/docs/global-options/index.md)).

## See also

[grit fetch](https://grit-scm.com/docs/fetch/index.md), [grit clone](https://grit-scm.com/docs/clone/index.md), [grit remote](https://grit-scm.com/docs/remote/index.md)

# https://grit-scm.com/docs/update/index.md

# grit update

> Update grit to the latest release.

## Synopsis

```text
grit update
```

## Description

Downloads the latest release of `grit` and installs it over the copy you're running. It does this by running the same install script as the one-line install on the [Install](https://grit-scm.com/docs/install/index.md) page: `curl -fsSL https://grit-scm.com/install | sh` on macOS and Linux, which needs `sh` and `curl`, and `irm https://grit-scm.com/install.ps1 | iex` in PowerShell on Windows.

The installer prints its own progress. If you installed `grit` with `cargo install`, update it with `cargo install grit-cli` instead.

## Options

`grit update` takes no options beyond the [global ones](https://grit-scm.com/docs/global-options/index.md).

## Examples

```console
$ grit update
Updating grit (current: 0.5.0)
Install directory: /home/ada/.local/bin
...
```

## JSON output

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `updated` | boolean | Whether the install script was started successfully. |
| `version` | string | Version of `grit` that ran the update (not necessarily the version installed). |

Example:

```json
{
  "updated": true,
  "version": "0.5.0"
}
```

With `--json`, the installer's progress goes to stderr so stdout carries only the JSON object.

## See also

[Docs overview](https://grit-scm.com/docs/index.md)

# https://grit-scm.com/docs/skill/index.md

# grit skill

> Print an agent skill that explains how to use grit.

## Synopsis

```text
grit skill
```

## Description

Prints a `SKILL.md` file for coding agents that explains how to use `grit`: the JSON output contract, the everyday commands, how `grit commit` differs from `git commit`, how to stay non-interactive, and what to do with `git` instead. It uses the [Agent Skills](https://agentskills.io) format, with `name` and `description` front matter, so agents that load skills pick it up when a task involves version control.

The skill is built into the binary and names the version that printed it, so regenerate it after [`grit update`](https://grit-scm.com/docs/update/index.md) to keep it current.

Save it where your agent looks for skills, for example `.agents/skills/grit/SKILL.md` in a project, or `~/.claude/skills/grit/SKILL.md` for Claude Code across all projects.

## Options

`grit skill` takes no options besides the [global options](https://grit-scm.com/docs/global-options/index.md).

## Examples

Install the skill in the current project:

```console
$ mkdir -p .agents/skills/grit
$ grit skill > .agents/skills/grit/SKILL.md
```

Read the start of it:

```console
$ grit skill | head -4
---
name: grit
description: Use the grit CLI for version control in Git repositories — status, diffs, commits, branches, merges, cherry-picks, fetch/pull/push and tags — with JSON output for scripts and agents. Use when a task involves committing, branching, syncing with a remote or reading history and `grit` is installed.
---
```

## JSON output

Pass `--json` to get the skill text along with where to save it:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `name` | string | The skill's name, `grit`. |
| `version` | string | The grit version that generated the skill. |
| `path` | string | A conventional place to save it, relative to a project root. |
| `content` | string | The full `SKILL.md` text, front matter included. |

Example (with `content` shortened):

```json
{
  "name": "grit",
  "version": "0.5.2",
  "path": ".agents/skills/grit/SKILL.md",
  "content": "---\nname: grit\ndescription: Use the grit CLI for version control …"
}
```

## Markdown output

Pass [`--markdown`](https://grit-scm.com/docs/global-options/index.md) to print the full `SKILL.md` text on stdout (same bytes as human mode, suitable for saving or parsing):

```text
---
name: grit
description: Use the grit CLI for version control …
---
```

## See also

[Agent guide](https://grit-scm.com/docs/agents/index.md), [grit update](https://grit-scm.com/docs/update/index.md), [Global options](https://grit-scm.com/docs/global-options/index.md)

# https://grit-scm.com/docs/completions/index.md

# grit completions

> Generate shell completion scripts for bash, zsh, and fish.

## Synopsis

```text
grit completions <shell>
```

## Description

Prints a shell completion script for the `grit` subcommands and flags in the running binary. Redirect stdout to a file your shell loads, or evaluate it in the current session during setup.

Supported shells: `bash`, `elvish`, `fish`, `powershell`, and `zsh` (whatever `clap_complete` exposes for the installed `grit` version).

The script always matches the CLI you built or installed. After [`grit update`](https://grit-scm.com/docs/update/index.md), regenerate completions so new commands and flags appear.

Release archives also ship pre-generated scripts under `completions/` when you install from a tarball.

## Options

| Argument | Meaning |
| -------- | ------- |
| `<shell>` | One of `bash`, `elvish`, `fish`, `powershell`, or `zsh`. |

`grit completions` rejects the global [`--json`](https://grit-scm.com/docs/global-options/index.md) and [`--markdown`](https://grit-scm.com/docs/global-options/index.md) flags; stdout is always the raw completion script.

## Examples

Generate bash completions for the current session:

```console
$ grit completions bash > /tmp/grit.bash
$ source /tmp/grit.bash
```

Install zsh completions for your user (typical layout):

```bash
grit completions zsh > ~/.local/share/zsh/site-functions/_grit
```

Install fish completions:

```bash
mkdir -p ~/.config/fish/completions
grit completions fish > ~/.config/fish/completions/grit.fish
```

## JSON output

There is no JSON form. `--json` and `--markdown` exit with an error; use the human completion script only.

## Markdown output

None. `--markdown` is rejected for the same reason as `--json`.

## See also

[Install grit](https://grit-scm.com/docs/install/index.md), [grit update](https://grit-scm.com/docs/update/index.md), [Global options](https://grit-scm.com/docs/global-options/index.md)

# https://grit-scm.com/docs/upload-pack/index.md

# grit upload-pack

> Serve a fetch or clone of a repository over stdin and stdout.

## Synopsis

```text
grit upload-pack [--stateless-rpc] [--advertise-refs] <directory>
```

## Description

The server side of fetch and clone. It speaks the Git wire protocol on stdin and stdout, so any Git client, `git` or `grit`, can fetch from a repository it serves. You don't normally run it yourself: an SSH server runs it when a client connects, and `grit-http-server` answers smart HTTP requests by calling the same logic in `grit-lib` in-process (no separate `grit` binary next to the server). It doesn't appear in `grit --help`.

`<directory>` is the repository to serve: a bare repository, a working tree, or a path that names one once `.git` is added. Unlike other commands, `grit upload-pack` doesn't search parent directories, so it serves exactly the repository it was given.

It supports Git protocol versions 0, 1 and 2, chosen by the client through the `GIT_PROTOCOL` environment variable. Refs matching `uploadpack.hideRefs` or `transfer.hideRefs` are not shown to clients.

Shallow and deepen requests (`deepen`, `deepen-since`, `deepen-not`, `deepen-relative`, client `shallow` lines) are honored in protocol v0/v1 and in v2 `fetch` (including the `shallow-info` section and sideband-all framing). Partial-clone filters (`filter blob:none`, `blob:limit=<n>`, `tree:<depth>`, and combinations) follow `uploadpack.allowFilter` and `uploadpackfilter.*` policy. Protocol v2 `want-ref` is available when `uploadpack.allowRefInWant` is set. Optional `want` rules follow `uploadpack.allowTipSha1InWant` and `uploadpack.allowReachableSha1InWant`.

## Options

| Option | Description |
| --- | --- |
| `<directory>` | The repository to serve. |
| `--stateless-rpc` | Answer a single request and exit, without advertising refs first. Used for smart HTTP. |
| `--advertise-refs` | Print the ref advertisement and exit. Used for the first request of smart HTTP. Also accepted as `--http-backend-info-refs`. |

## Examples

Clone over SSH from a server that has `grit` but not `git` installed:

```console
$ git clone --upload-pack='grit upload-pack' ssh://example.com/srv/git/project.git
```

Set it for a remote you already have:

```console
$ git config remote.origin.uploadpack 'grit upload-pack'
```

## JSON output

None. `--json` has no effect, since stdout carries the Git protocol.

## See also

[grit receive-pack](https://grit-scm.com/docs/receive-pack/index.md), [grit clone](https://grit-scm.com/docs/clone/index.md), [grit fetch](https://grit-scm.com/docs/fetch/index.md)

# https://grit-scm.com/docs/receive-pack/index.md

# grit receive-pack

> Accept a push to a repository over stdin and stdout.

## Synopsis

```text
grit receive-pack [--stateless-rpc] [--advertise-refs] <directory>
```

## Description

The server side of push. It speaks the Git wire protocol on stdin and stdout, receives the objects a client sends, and updates the repository's refs. You don't normally run it yourself: an SSH server runs it when a client pushes, and `grit-http-server` answers smart HTTP requests by calling the same logic in `grit-lib` in-process (no separate `grit` binary next to the server). It doesn't appear in `grit --help`.

`<directory>` is the repository to push to: a bare repository, a working tree, or a path that names one once `.git` is added. It doesn't search parent directories.

Every ref update is checked before it's applied, and the client is told which updates were accepted and why any were refused. These settings in the served repository's config control what's allowed:

| Setting | Effect |
| --- | --- |
| `receive.denyNonFastForwards` | Refuse updates that would discard commits from a branch. |
| `receive.denyDeletes` | Refuse to delete refs. |
| `receive.denyCurrentBranch` | Refuse to update the branch checked out in a non-bare repository. On by default; set it to `ignore` or `warn` to allow it. |
| `receive.hideRefs`, `transfer.hideRefs` | Hide refs from pushing clients and refuse updates to them. |

Pushes that ask for an atomic update are applied all or nothing. Server-side hooks are not supported yet.

## Options

| Option | Description |
| --- | --- |
| `<directory>` | The repository to push to. |
| `--stateless-rpc` | Answer a single request and exit, without advertising refs first. Used for smart HTTP. |
| `--advertise-refs` | Print the ref advertisement and exit. Used for the first request of smart HTTP. Also accepted as `--http-backend-info-refs`. |

## Examples

Push over SSH to a server that has `grit` but not `git` installed:

```console
$ git push --receive-pack='grit receive-pack' origin main
```

Set it for a remote you already have:

```console
$ git config remote.origin.receivepack 'grit receive-pack'
```

## JSON output

None. `--json` has no effect, since stdout carries the Git protocol.

## See also

[grit upload-pack](https://grit-scm.com/docs/upload-pack/index.md), [grit push](https://grit-scm.com/docs/push/index.md)

# https://grit-scm.com/docs/manager/index.md

# grit manager

> A credential helper that stores passwords and tokens in the Windows Credential Manager.

## Synopsis

```text
grit manager (get | store | erase)
```

## Description

A Git credential helper backed by the Windows Credential Manager. You don't run it yourself: Git and `grit` run it when they need a password or token. It speaks Git's [credential helper protocol](https://git-scm.com/docs/gitcredentials), reading the request on stdin and, for `get`, writing the credentials it found on stdout.

[`grit auth`](https://grit-scm.com/docs/auth/index.md) sets `credential.helper` to `grit manager` on Windows when no other helper is configured. To set it up yourself:

```text
> grit config --global credential.helper "grit manager"
```

It works only on Windows. On other systems it exits with an error.

## Options

| Option | Description |
| --- | --- |
| `get` | Look up stored credentials for the request. |
| `store` | Save the credentials in the request. |
| `erase` | Remove the credentials matching the request. |

## Examples

```text
> echo "protocol=https`nhost=github.com`n" | grit manager get
protocol=https
host=github.com
username=x-access-token
password=gho_...
```

## JSON output

None. `grit manager` always speaks the credential helper protocol.

## See also

[grit auth](https://grit-scm.com/docs/auth/index.md), [grit config](https://grit-scm.com/docs/config/index.md)
