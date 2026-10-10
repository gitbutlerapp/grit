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
