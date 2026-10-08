---
title: Agent guide
summary: Run grit from scripts and autonomous agents — JSON output, exit codes, non-interactive use, credentials, and where to read the docs.
---

This page is for programs and agents that drive [`grit`](../install/) in automation. It describes **existing** CLI behavior only. For field-level JSON schemas, use each command's docs page; this guide covers the shared contract.

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

Plain `grit` with no subcommand is the same as [`grit status`](../status/); `--json` works there too:

```console
$ grit --json --filter '{branch, clean}'
{
  "branch": "main",
  "clean": false
}
```

See [Scripting with grit](../scripting/) for more `--filter` patterns. Per-command keys are documented under [The grit CLI](../global-options/) command pages.

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

[`grit push`](../push/) can exit **1** even after printing a full JSON outcome when a ref was rejected (same idea as Git: the outcome is reported, but the operation failed).

## Color and terminal detection

Human output uses ANSI color only when **stdout is a terminal**, the environment supports ANSI (`grit_lib::terminal::ansi_supported()`), and **`NO_COLOR` is unset** ([no-color.org](https://no-color.org)). Piped or redirected stdout is plain text; set `NO_COLOR=1` to force plain text on a TTY.

JSON mode never depends on color; always pass `--json` for machines.

## Stay non-interactive

`grit` does **not** open an external editor for commit messages. [`grit commit`](../commit/) requires a message on the command line (positional or `-m`); there is no `$GIT_EDITOR` path.

Commands and flows that can **block on a person**:

| Area | Behavior | How to avoid blocking |
| --- | --- | --- |
| [`grit auth`](../auth/) | OAuth device flow: instructions on stderr, then polls GitHub until you authorize in a browser. | Run `grit auth` once in an interactive session before automation. Store the token via `credential.helper` (see below). |
| [`grit fetch`](../fetch/) / [`grit push`](../push/) / [`grit pull`](../pull/) | On HTTPS `github.com` auth failure, if **stdin is a TTY**, grit may ask `Sign in to GitHub now? [Y/n]` and run the device flow. | Use a stored token from `grit auth`, or ensure stdin is not a TTY so grit prints a hint and exits without prompting. |
| [`grit clone`](../clone/) / [`grit fetch`](../fetch/) / [`grit pull`](../pull/) / [`grit push`](../push/) over **SSH** | Grit runs `ssh` with protocol stdin/stdout piped but **stderr inherited**, so the SSH child can still talk to `/dev/tty` (host-key confirmation, passphrase prompts, `ssh-askpass`, and similar). `--json` does not disable this. | Preconfigure keys and known hosts; for unattended jobs set `GIT_SSH_COMMAND='ssh -o BatchMode=yes'` so SSH fails fast instead of prompting (grit resolves SSH from `GIT_SSH_COMMAND`, then `GIT_SSH`, then `ssh` on `PATH` — not `core.sshCommand`). |
| [`grit update`](../update/) | Runs the release installer (`curl \| sh` on Unix). | Do not invoke in unattended jobs unless you intend to upgrade the binary. |

There is no global `--yes` flag. For commits, always pass `-m` / a message argument.

[`grit auth logout`](../auth/) is non-interactive and reports whether a helper had a token to erase:

```console
$ grit auth logout --json
{
  "logged_out": false,
  "host": "github.com"
}
```

### Plumbing commands (not JSON)

These speak binary or line protocols on stdin/stdout and have **no** `--json` mode: [`grit manager`](../manager/), [`grit upload-pack`](../upload-pack/), [`grit receive-pack`](../receive-pack/). Agents should not wrap them as JSON commands.

`-V` / `--version` prints a version string and does not emit JSON even if `--json` is passed:

```console
$ grit --json --version
grit 0.5.1
```

## Credentials (HTTPS GitHub)

For `https://github.com` remotes, sign in once with [`grit auth`](../auth/). Grit uses GitHub's device flow, then stores an access token through your configured **`credential.helper`** (same mechanism as Git). Override the OAuth app id with `GRIT_GITHUB_CLIENT_ID` or `grit config --global grit.githubClientId …`.

Sign out with `grit auth logout`. You need a credential helper configured (for example `grit config --global credential.helper store` on Linux — see the [`grit auth`](../auth/) page).

SSH remotes do not use the GitHub device-flow token; they can still block on SSH prompts (see the SSH row above).

## Documentation for agents

| Resource | URL / path | Use |
| --- | --- | --- |
| **Agent skill** | [`grit skill`](../skill/) | A `SKILL.md` for coding agents, printed by the installed binary: `grit skill > .agents/skills/grit/SKILL.md`. |
| **llms.txt** | [grit-scm.com/llms.txt](https://grit-scm.com/llms.txt) | Curated index of every docs page (llmstxt.org format). |
| **llms-full.txt** | [grit-scm.com/llms-full.txt](https://grit-scm.com/llms-full.txt) | Full text of all docs Markdown twins in site order. |
| **Markdown twin** | Same URL as HTML but `index.md` instead of `index.html` (e.g. [status/index.md](https://grit-scm.com/docs/status/index.md)) | One page per command or guide; linked from HTML as "Markdown". |
| **Command JSON fields** | Under [The grit CLI](../global-options/) | Each command page lists real `--json` examples and field tables. |
| **grit-lib API** | [docs.rs/grit-lib](https://docs.rs/grit-lib) | Rust library reference when embedding grit in your own binary. |
| **Library guide** | [Library guide](../library/) | Narrative Rust workflows on top of `grit-lib`. |

Site generation keeps `docs/llms.txt` and `docs/llms-full.txt` in the repository in sync with [site.toml](https://github.com/gitbutlerapp/grit/blob/main/content/docs/site.toml); after local doc edits, run `make docs`.

## See also

- [Scripting with grit](../scripting/) — `--json`, `--filter`, and exit codes in brief
- [Global options](../global-options/) — flags shared by every command
- [Docs overview](../index/) — how the site is organized
