# grit rm

> Remove tracked paths from the working tree and index.

## Synopsis

```text
grit rm [--cached] [-f|--force] <path>...
```

## Description

Removes tracked files or directories from the index and, by default, from the working tree. With `--cached`, the file stays on disk as an untracked file but is removed from the index.

Grit refuses to remove paths that have staged or unstaged local modifications unless you pass `--force`, matching Git's safety rule for `git rm`.

Directory pathspecs remove every tracked file under that directory; trailing slashes on a directory path imply recursive removal.

## Options

| Option | Description |
| --- | --- |
| `<path>...` | Files or directories to remove (required). |
| `--cached` | Remove from the index only; keep the working tree copy. |
| `-f`, `--force` | Remove even when paths have local modifications. |

## Examples

Remove a tracked file:

```console
$ grit rm old.txt
Removed old.txt.
```

Keep the file on disk but stop tracking it:

```console
$ grit rm --cached config.local
Removed config.local.
```

## JSON output

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `removed` | array of strings | Repository-relative paths removed from the index (and work tree unless `--cached`). |

Example:

```json
{
  "removed": ["old.txt"]
}
```

## Markdown output

Pass [`--markdown`](https://grit-scm.com/docs/global-options/index.md) for agent-friendly output:

```text
- **removed**:
  - `old.txt`
```

## See also

[grit add](https://grit-scm.com/docs/add/index.md), [grit mv](https://grit-scm.com/docs/mv/index.md), [grit status](https://grit-scm.com/docs/status/index.md)
