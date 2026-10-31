---
title: grit mv
summary: Rename or move tracked paths.
group: Making changes
order: 4
---

## Synopsis

```text
grit mv [-f|--force] [--dry-run] <source>... <destination>
```

## Description

Renames or moves tracked paths in the working tree and index, preserving object ids and cached stat data. The last argument is the destination; earlier arguments are sources.

When the destination is an existing directory, each source is moved into that directory under its original base name. Moving a directory renames the whole tree of tracked files. Multiple sources are validated together before anything moves; the index is updated once.

Grit refuses untracked sources, overwriting an existing destination without `--force`, and moving a directory into one of its own subdirectories.

On case-insensitive volumes with `core.ignorecase=true`, case-only renames (for example `README.md` to `readme.md`) succeed: the worktree spelling updates and the index tracks the destination path, matching `git mv`.

When a move leaves a source directory empty, grit leaves that directory on disk (same as `git mv`).

## Options

| Option | Description |
| --- | --- |
| `<source>...` | One or more tracked paths to move. |
| `<destination>` | Final path or existing directory (last argument). |
| `-f`, `--force` | Replace an existing file at the destination. |
| `--dry-run` | List planned renames without changing the index or work tree. |

## Examples

Rename a file:

```console
$ grit mv draft.txt final.txt
Renamed draft.txt -> final.txt.
```

Move into a directory:

```console
$ grit mv report.txt docs/
Renamed report.txt -> docs/report.txt.
```

## JSON output

Live `--json` output follows schema [v2](/schemas/mv/v2.json). The frozen [v1](/schemas/mv/v1.json) schema documents the legacy `{from, to}` shape.

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `moves` | array of `{from, to}` | Each rename performed or previewed. |
| `dry_run` | boolean | `true` when `--dry-run` was passed. |

Example:

```json
{
  "dry_run": false,
  "moves": [
    {
      "from": "draft.txt",
      "to": "final.txt"
    }
  ]
}
```

## Markdown output

Schema: [v2](/schemas/mv/v2.json)
Pass [`--markdown`](../global-options/) for agent-friendly output:

```text
- **moves**:
  - `draft.txt` → `final.txt`
```


## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Success ([`success`](../exit-codes/)). |
| `1` | Command failed ([`failure`](../exit-codes/)). |
| `2` | Invalid flags or arguments ([`usage`](../exit-codes/)). |

See [Exit codes](../exit-codes/) for shared rules and structured `--json` `kind` values.


## See also

[grit add](../add/), [grit rm](../rm/), [grit status](../status/)