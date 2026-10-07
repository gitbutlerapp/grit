---
title: Docs
summary: How to use grit, a simple Git client built on grit-lib. Start with the tutorial, then look up any command.
---

`grit` is a Git client with a small, modern command line. It works on any Git repository and talks to any Git remote, but its commands are its own: one obvious way to do the common thing, plain-language output, and `--json` on every command. If you're new, start with the [tutorial](tutorial/).

## Install

```
curl -fsSL https://grit-scm.com/install | sh
```

On Windows, run `irm https://grit-scm.com/install.ps1 | iex` in PowerShell. With a Rust toolchain you can also run `cargo install grit-cli`. Once installed, `grit update` keeps it current.

## Options for every command

| Option | Description |
| --- | --- |
| `--json` | Print one JSON object instead of human-readable text. Errors become `{"error": "..."}`. |
| `--filter <EXPR>` | Apply a jq expression to the JSON output. Requires `--json`. |
| `-h`, `--help` | Print help for `grit` or for a command. |
| `-V`, `--version` | Print the version of `grit`. |

Running `grit` with no command is the same as [`grit status`](status/).

## Scripting with grit

With `--json`, a command prints exactly one JSON value on stdout, so you can pipe it straight into other tools. The shape of each command's output is documented on its page. `--filter` runs a [jq](https://jqlang.org) expression over that value without needing `jq` installed. When a filter produces several values, they come back as one JSON array:

```
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

## Building on the library

Everything `grit` does is built on `grit-lib`, a Git library for Rust. To use Git from your own program, see the [grit-lib API documentation](https://docs.rs/grit-lib).
