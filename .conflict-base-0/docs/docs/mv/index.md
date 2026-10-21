# grit mv

> Rename or move tracked paths.

## Synopsis

```text
grit mv [-f|--force] <source>... <destination>
```

## Description

Renames or moves tracked paths in the working tree and index, preserving object ids and cached stat data. The last argument is the destination; earlier arguments are sources.

When the destination is an existing directory, each source is moved into that directory under its original base name. Moving a directory renames the whole tree of tracked files.

Grit refuses untracked sources, overwriting an existing destination without `--force`, and moving a directory into one of its own subdirectories.

## Options

| Option | Description |
| --- | --- |
| `<source>...` | One or more tracked paths to move. |
| `<destination>` | Final path or existing directory (last argument). |
| `-f`, `--force` | Replace an existing file at the destination. |

## Examples

Rename a file:

```console
$ grit mv draft.txt final.txt
Renamed draft.txt -> final.txt.
```

Move into a directory:

```console
$ grit mv report.txt docs/
Renamed report.txt -> docs/.
```

## JSON output

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `from` | string | Source path (space-separated when multiple sources were moved into a directory). |
| `to` | string | Destination path after the move. |

Example:

```json
{
  "from": "draft.txt",
  "to": "final.txt"
}
```

## Markdown output

Pass [`--markdown`](https://grit-scm.com/docs/global-options/index.md) for agent-friendly output:

```text
- **from**: `draft.txt`
- **to**: `final.txt`
```

## See also

[grit add](https://grit-scm.com/docs/add/index.md), [grit rm](https://grit-scm.com/docs/rm/index.md), [grit status](https://grit-scm.com/docs/status/index.md)
