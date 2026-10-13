---
title: Scripting with grit
summary: Use --json and --filter to build scripts on top of grit.
---

With `--json`, a command prints exactly one JSON value on stdout, so you can pipe it straight into other tools. The shape of each command's outcome is documented on its command page and in the committed v1 JSON Schemas under [`/schemas/`](../schemas/). `--filter` runs a [jq](https://jqlang.org) expression over that value without needing `jq` installed. When a filter produces several values, they come back as one JSON array:

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

## JSON contract

| Topic | Rule |
| --- | --- |
| **Success stdout** | Exactly one JSON value: the command outcome object. |
| **Failure stdout** | One object `{"error": "…"}` (see [error schema](/schemas/error/v1.json)). Some failures add structured fields such as `kind`; parsers should accept unknown keys. |
| **stderr** | Progress, auth hints, and human `error:` lines. Scripts should parse **stdout only** for the outcome when `--json` is set. |
| **`--filter`** | Requires `--json`. Runs jq-like logic on the outcome; multiple results become one JSON array. |
| **Exit codes** | `0` success, `1` command failure (JSON error object may still be on stdout), `2` usage error (no JSON on stdout). |

Pass `--json` on every command you script. Plumbing commands ([`grit manager`](../manager/), [`grit upload-pack`](../upload-pack/), [`grit receive-pack`](../receive-pack/)) speak wire protocols instead of JSON.

## Markdown output (`--markdown`)

For agents and tools that prefer readable text, pass [`--markdown`](../global-options/) instead of `--json`. The same field contract applies: each command page links the v1 JSON Schema in **both** the JSON output and Markdown output sections so parsers know the stable keys even when stdout is Markdown.

Grit renders `--markdown` with these conventions (shared across commands):

- **Headings** — `##` sections group related output (for example “Staged changes”, “Untracked”).
- **Lists** — bullet lines for commits, paths, and simple enumerations; inline `` `short-oid` `` for object ids where helpful.
- **Tables** — pipe-separated rows for tabular data. Cell text escapes `\|`, `\n`, and `\r` so pipes inside branch names or paths do not break the table.
- **Fenced diffs** — unified patches use a `diff` code fence. If the patch body contains triple backticks, the opening and closing fence markers lengthen (four or more backticks) so the fence never terminates early.

`--markdown` and `--json` are mutually exclusive; `--filter` applies only with `--json`.

Color is used only when stdout is a terminal. Set `NO_COLOR` to turn it off.

For autonomous agents — non-interactive auth, where to find `llms.txt`, credentials, and the full automation contract — see the [Agent guide](../agents/).
