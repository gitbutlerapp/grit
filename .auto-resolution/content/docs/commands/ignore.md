---
title: grit ignore
summary: Query whether paths match exclude (gitignore) rules.
group: Plumbing
order: 6
---

## Synopsis

```text
grit ignore check <PATH>...
grit ignore check --all <PATH>...
```

## Description

The `check` subcommand reports whether paths are ignored by `.gitignore`, `.git/info/exclude`, and `core.excludesfile`, using the same precedence as Git. Tracked index entries are never treated as ignored.

By default only **ignored** paths are printed. Pass `--all` to also list paths that are not ignored.

The command exits with status **0** when at least one path is ignored, and **1** when none are. Scripts can branch on this exit code.

Human output uses a verbose-style line per ignored path:

```text
path  ← .gitignore:3 *.log
```

## Options

| Option | Description |
| --- | --- |
| `--all` | Also print paths that are not ignored. |
| `<PATH>` | One or more paths (from the current directory or repository-relative). |

## Examples

Query a single ignored file:

```console
$ grit ignore check build/output.log
build/output.log  ← .gitignore:1 *.log
```

List ignored and non-ignored paths:

```console
$ grit ignore check --all src/main.rs tmp/cache/
src/main.rs
tmp/cache/  ← .gitignore:2 tmp/
```

Exit status when nothing matches:

```console
$ grit ignore check tracked.txt; echo $?
1
```

## JSON output

Schema: [v1](/schemas/ignore.check/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `any_ignored` | boolean | `true` when at least one queried path is ignored. |
| `entries` | array | Filtered results (ignored only unless `--all`). |
| `entries[].path` | string | Repository-relative path. |
| `entries[].ignored` | boolean | Whether the path is ignored. |
| `entries[].matched` | object or omitted | Verbose match metadata when present. |
| `entries[].matched.source` | object | Typed source (`per_directory`, `info_exclude`, or `core_excludes_file`). |
| `entries[].matched.line_number` | number | 1-based line in the source file. |
| `entries[].matched.pattern_text` | string | Pattern text as written in the file. |
| `entries[].matched.negative` | boolean | Whether the rule is negated (`!pattern`). |

Example:

```json
{
  "any_ignored": true,
  "entries": [
    {
      "path": "build/output.log",
      "ignored": true,
      "matched": {
        "source": {
          "kind": "per_directory",
          "path": ".gitignore"
        },
        "line_number": 1,
        "pattern_text": "*.log",
        "negative": false
      }
    }
  ]
}
```

## Markdown output

Schema: [v1](/schemas/ignore.check/v1.json)

Pass `--markdown` for agent-friendly output with the same fields summarized in prose and lists.

## See also

- [`grit clean`](clean.md) — remove untracked files (optionally including ignored paths with `--ignored`).
- [`grit status`](status.md) — working tree summary.
