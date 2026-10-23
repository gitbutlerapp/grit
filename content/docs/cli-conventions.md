---
title: CLI conventions
summary: Shared rules for grit command paths, limits, dry-run, messages, pathspecs, and force flags — enforced by the cli_contract tests in grit-cli.
---

Grit's CLI is intentionally small, but the flags that appear on many commands follow one contract. The `grit-cli` crate encodes that contract in `cli_contract.rs`: every **leaf command path** in the clap tree is classified, and tests fail when a new command is added without a classification or when a rule is broken without a tracked exception.

This page states the rules in grit's own words. It does **not** change runtime behavior; later roadmap steps align commands that still have exceptions.

## Command kinds

Each leaf path (for example `stash.list`, `gc.repack`, or top-level `commit`) has exactly one kind:

| Kind | Meaning |
| --- | --- |
| **List** | Prints a bounded collection. Must expose `-n` as a **limit** (see below). |
| **Mutation** | Changes refs, the index, the working tree, config, or remote state. Must expose long-only `--dry-run` to preview, except where noted. |
| **Query** | Read-only inspection (diff, show, status, fsck, …). |
| **Plumbing** | Wire protocols or credential plumbing (`upload-pack`, `receive-pack`, `manager`). |
| **Generator** | Emits shell completion scripts or the agent skill (`completions`, `skill`). |

Parent commands that run without a subcommand (for example bare `grit stash` for push, `grit remote` for list) are separate leaf paths alongside explicit subcommands such as `stash.push`.

## Limit (`-n`)

On **list** commands, `-n` always means **limit** — never “dry-run” or anything else.

- The short flag is `-n`.
- The long name is `--limit`, or `--max-count` when `--limit` is registered as a long alias (as on [`grit log`](../log/) and [`grit revs`](../revs/) today).
- No other argument on the same command may use `-n` for a different meaning.

## Dry-run (`--dry-run`)

On **mutation** commands, preview mode is long-only **`--dry-run`** (no `-n` shortcut). Mutations that support partial selection (`add`, `restore`, `commit` with hunks, …) use `--dry-run` the same way.

**`grit clean`** is a mutation with different defaults: without `-f` / `--force`, it only prints what would be removed. Passing **`--force`** performs the deletion. That is the documented special case instead of an exception entry.

## Message input (`-m`, `-F`)

Commands classified with **takes message** accept:

- `-m` / `--message` for inline text, and
- `-F` / `--message-file` for a file path (`-` reads stdin),

with the two flags mutually exclusive. [`grit merge`](../merge/) uses the shared message group for merge commits; history subcommands such as [`grit history reword`](../history/) follow the same pattern where a message is optional or required per subcommand.

## Pathspecs (`--` paths)

Commands classified with **takes paths** accept:

- trailing `-- <paths>` arguments, plus
- `--pathspec-from-file` and `--pathspec-file-nul` (Git-compatible path lists),

shared via the same Rust argument group in the CLI. Commands that only take positional paths today are tracked as exceptions until step 7 of the CLI roadmap lands the shared group on [`grit log`](../log/), [`grit revs`](../revs/), [`grit diff`](../diff/), and [`grit show`](../show/).

## Force (`-f`)

Every **`--force`** flag exposes short **`-f`**, unless the command page and `EXCEPTIONS` in `cli_contract.rs` document a deliberate deviation.

## Enforcement and exceptions

Two tests guard the contract:

- `cli_contract_conventions` — every leaf path is classified; clap flags match the rules unless listed in `EXCEPTIONS`.
- `cli_contract_exceptions_are_live` — each exception still describes a real violation; remove the row when the command is fixed.

Exception rows name an **owner** (a later plan step such as `s4 limit`, `s5 dry-run`, or a roadmap bucket like `workspace-extras`). When you fix a violation, delete its exception and rely on the convention test.

Adding a command requires a new row in `COMMAND_CONTRACTS` inside `grit-cli/src/cli_contract.rs`. CI fails with a message that names the path and the rule if you omit it.

## See also

- [Scripting with grit](../scripting/) — `--json`, exit codes, and stdout discipline.
- [Agent guide](../agents/) — non-interactive use and schema links.
- [Global options](../global-options/) — `--json`, `--markdown`, `--filter`, and `--threads`.
