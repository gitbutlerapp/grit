---
title: Agent guide
summary: Run grit from scripts and autonomous agents — JSON output, exit codes, non-interactive use, credentials, and where to read the docs.
---

This page is for programs and agents that drive [`grit`](../install/) in automation. It describes **existing** CLI behavior only. Machine-readable **v1 JSON Schemas** for each command's `--json` success object are published at `/schemas/<command>/v1.json` (see the [JSON schemas](../schemas/) index). Field-level tables on each command page remain the human-oriented reference; this guide covers the shared contract. Cross-command flag rules (limits, dry-run, messages, pathspecs) are summarized in [CLI conventions](../cli-conventions/).

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

## JSON schema versioning

Each command path has versioned schema files under `content/schemas/`: `<command>.vN.json` (dotted paths for subcommands, such as `stash.list.v1.json`). The live contract is whatever **`current`** version the per-command table in `grit-cli/src/json_schema.rs` lists; older **`frozen`** versions stay on disk at stable `$id` URLs and are never regenerated.

**Additive on the live version (no bump):** new optional properties, new enum variants, new `definitions`, adding an `enum` to a former free-form string (for example structured `kind`), or widening nullability.

**Breaking (requires publishing vN+1):** property removed or renamed, JSON type narrowed, a property newly required, enum variant removed, `additionalProperties: false` where extra keys were previously allowed, or nullability removed from a field.

CI runs three gates:

- `json_schemas_are_current` — generated live schemas match committed bytes (or run update mode below).
- `json_schema_backward_compatible` — every committed schema file (90 total) stays backward compatible: live files vs freshly generated types; frozen older files are checked against themselves so they never drift silently.
- `json_snapshots_match_schemas` — golden `--json` snapshots validate against the matching schema.

Regenerate live schemas after intentional **additive** contract changes:

```bash
GRIT_UPDATE_SCHEMAS=1 cargo test json_schemas_are_current -p grit-cli -- --nocapture
```

Update mode refuses to overwrite the live file when the diff would be **breaking**; the test output lists each breaking JSON path and tells you to publish `vN+1` (add `<command>.v{N+1}.json`, bump `current`, append `N` to `frozen`). Frozen files are never touched by update mode.

## Error object shape

Schema: [v1](/schemas/error/v1.json)

On failure with `--json`, stdout is a single object with an **`error`** string (the same message a human sees after `error:` on stderr). Some failures add a **`kind`** and optional fields (for example merge/pick **conflicts**, **`would_overwrite`** with `reason` and `paths` on switch refusal, or **`hash_collision`** with an **`oid`** digest when object ingestion detects an SHA-1 chosen-prefix collision); see the error schema for the plain vs structured union. When grit knows the next step, a **`hint`** names the command to run (`grit remote add origin <url>`, `grit push -u origin main`, `grit auth`, `grit merge --continue`, …), and **`paths`** lists the files an `unmerged_paths` or `dirty_worktree` failure is about. Branch on `kind`, not on the `error` text.

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
| **2** | Usage error (unknown option, invalid subcommand flags, bad `--hunks` ids, or `diff --exit-code` when diff fails). Clap still prints help to stderr. With **`--json`**, stdout is `{"error":"…"}` even for clap usage errors. Without `--json`, usage errors may have empty stdout. |
| **128** | Remote unreadable (clone/fetch), bundle missing prerequisites, or `merge --ff-only` refused. |
| **129** | Invalid `--author` / `--grep` / pickaxe pattern (`kind`: `invalid_pattern`). |

See [Exit codes](../exit-codes/) for the full table and structured `kind` values.

```console
$ grit --nope
error: unexpected argument '--nope' found

Usage: grit [OPTIONS] [COMMAND]

For more information, try '--help'.
```

```console
$ grit --json --nope
{"error":"unexpected argument '--nope' found"}
```

```console
$ grit status --filter '.branch'
error: --filter requires --json
```

[`grit push`](../push/) can exit **1** even after printing a full JSON outcome when a ref was rejected (same idea as Git: the outcome is reported, but the operation failed).

## Color and terminal detection

Human output uses ANSI color only when **stdout is a terminal**, the environment supports ANSI (`grit_lib::porcelain::terminal::ansi_supported()`), and **`NO_COLOR` is unset** ([no-color.org](https://no-color.org)). Piped or redirected stdout is plain text; set `NO_COLOR=1` to force plain text on a TTY.

JSON mode never depends on color; always pass `--json` for machines.

## Markdown output (`--markdown`)

When a human-readable transcript is easier to consume than raw JSON, pass **`--markdown`**. It is mutually exclusive with `--json`; use JSON for strict parsing and Markdown for agent-oriented prose.

Each command page documents real `--markdown` stdout under **Markdown output** and links the same v1 JSON Schema as the JSON section (for example `Schema: [v1](/schemas/status/v1.json)`). Treat that schema as the stable field contract even when stdout is Markdown.

Grit formats `--markdown` consistently:

| Convention | Behavior |
| --- | --- |
| **Headings** | `##` titles introduce sections (status groups, file lists, subcommand results). |
| **Bullets** | `-` lines for commits, paths, and simple lists; short oids often appear in backticks. |
| **Tables** | Markdown tables for rows of refs, remotes, worktrees, and similar; `\|`, `\n`, and `\r` inside cells are escaped. |
| **Diff fences** | One fenced `diff` block per changed file when a command emits patches; fence backticks lengthen if the patch text contains ``` so the fence stays unambiguous. |

Do not combine `--markdown` with `--filter`. See [Scripting with grit](../scripting/) for exit codes, filters, and the shared JSON error object.

## Stay non-interactive

`grit` does **not** open an external editor for commit messages. [`grit commit`](../commit/) requires a message on the command line (positional or `-m`); there is no `$GIT_EDITOR` path.

Commands and flows that can **block on a person**:

| Area | Behavior | How to avoid blocking |
| --- | --- | --- |
| [`grit auth`](../auth/) | OAuth device flow: instructions on stderr, then polls GitHub until you authorize in a browser. | Run `grit auth` once in an interactive session before automation. Store the token via `credential.helper` (see below). |
| [`grit fetch`](../fetch/) / [`grit push`](../push/) / [`grit pull`](../pull/) | On HTTPS `github.com` auth failure, if **stdin is a TTY**, grit may ask `Sign in to GitHub now? [Y/n]` and run the device flow. | Use a stored token from `grit auth`, or ensure stdin is not a TTY so grit prints a hint and exits without prompting. |
| [`grit clone`](../clone/) / [`grit fetch`](../fetch/) / [`grit pull`](../pull/) / [`grit push`](../push/) over **SSH** | Grit runs `ssh` with protocol stdin/stdout piped but **stderr inherited**, so the SSH child can still talk to `/dev/tty` (host-key confirmation, passphrase prompts, `ssh-askpass`, and similar). `--json` does not disable this. | Preconfigure keys and known hosts; for unattended jobs set `GIT_SSH_COMMAND='ssh -o BatchMode=yes'` so SSH fails fast instead of prompting. Grit resolves the ssh command like Git: `GIT_SSH_COMMAND`, then `core.sshCommand` from config, then `GIT_SSH`, then `ssh` on `PATH`. |
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

`-v` / `--version` (or `-V`) prints a version string and does not emit JSON even if `--json` is passed:

```console
$ grit --json --version
grit 0.5.1
```

## Credentials (HTTPS GitHub)

For `https://github.com` remotes, sign in once with [`grit auth`](../auth/). Grit uses GitHub's device flow, then stores an access token through your configured **`credential.helper`** (same mechanism as Git). Override the OAuth app id with `GRIT_GITHUB_CLIENT_ID` or `grit config --global grit.githubClientId …`.

Sign out with `grit auth logout`. You need a credential helper configured (for example `grit config --global credential.helper store` on Linux — see the [`grit auth`](../auth/) page).

The same helpers serve any HTTPS host, not only GitHub. In CI, a host-level line such as `https://ci:TOKEN@git.example.com` in the store file (`~/.git-credentials`) authenticates `grit clone`, `fetch` and `push` for every repository on that host, exactly as it does for Git.

SSH remotes do not use the GitHub device-flow token; they can still block on SSH prompts (see the SSH row above).

## Documentation for agents

| Resource | URL / path | Use |
| --- | --- | --- |
| **Agent skill** | [`grit skill`](../skill/) | A `SKILL.md` for coding agents, printed by the installed binary: `grit skill > .agents/skills/grit/SKILL.md`. Includes CI-tested **Recipes** (precise commit, merge conflicts, rebase + lease push, fetch PR, shallow clone) and links to [CLI conventions](../cli-conventions/). |
| **llms.txt** | [grit-scm.com/llms.txt](https://grit-scm.com/llms.txt) | Curated index of every docs page (llmstxt.org format). |
| **llms-full.txt** | [grit-scm.com/llms-full.txt](https://grit-scm.com/llms-full.txt) | Full text of all docs Markdown twins in site order. |
| **Markdown twin** | Same URL as HTML but `index.md` instead of `index.html` (e.g. [status/index.md](https://grit-scm.com/docs/status/index.md)) | One page per command or guide; linked from HTML as "Markdown". |
| **Command JSON fields** | Under [The grit CLI](../global-options/) | Each command page lists real `--json` examples and field tables. |
| **grit-lib API** | [docs.rs/grit-lib](https://docs.rs/grit-lib) | Rust library reference when embedding grit in your own binary. |
| **Library guide** | [Library guide](../library/) | Narrative Rust workflows on top of `grit-lib`. |

[llms.txt](https://grit-scm.com/llms.txt), [llms-full.txt](https://grit-scm.com/llms-full.txt), the two one-file bundles ([grit-cli.md](https://grit-scm.com/docs/grit-cli.md) and [grit-lib.md](https://grit-scm.com/docs/grit-lib.md)) and every Markdown twin are built from the same sources as the HTML pages on each deploy, so they never drift from the docs.

## See also

- [Scripting with grit](../scripting/) — `--json`, `--filter`, and exit codes in brief
- [Global options](../global-options/) — flags shared by every command
- [Docs overview](../index/) — how the site is organized
