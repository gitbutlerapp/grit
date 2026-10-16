# grit pull

> Fetch from the remote and bring the current branch up to date.

## Synopsis

```text
grit pull
```

## Description

Runs [`grit fetch`](https://grit-scm.com/docs/fetch/index.md), then merges the remote's copy of the current branch into it, as [`grit merge`](https://grit-scm.com/docs/merge/index.md) would: a fast-forward when you have no new commits of your own, and a merge commit when both sides do. On a branch with no commits yet, it simply takes the remote's.

The remote is the one in `branch.<name>.remote`, or `origin`. The remote branch is the one in `branch.<name>.merge`, or the branch with the same name as yours.

Like `grit merge`, `grit pull` won't run with uncommitted changes or a detached HEAD, and stops without changing anything if there are conflicts.

## Options

`grit pull` takes no options beyond the [global ones](https://grit-scm.com/docs/global-options/index.md).

## Examples

```console
$ grit pull
Fast-forwarded origin/main → b52cca6

$ grit pull
Merged origin/main into the current branch (acd1d4b)

$ grit pull
Already up to date.
```

## JSON output

The same shape as [`grit merge`](https://grit-scm.com/docs/merge/index.md#json-output). Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `result` | string | `up_to_date`, `fast_forward`, `merged`, or `set_upstream`. |
| `branch` | string | Remote branch that was integrated. |
| `oid` | string or null | Commit the current branch points at after the pull, when applicable. |
| `upstream` | string or null | For `set_upstream`, the remote-tracking branch that was adopted. |

Example:

```json
{
  "result": "fast_forward",
  "branch": "origin/main",
  "oid": "b52cca65378cf17309f0b1f917c6dc4fdd9256f2"
}
```

## See also

[grit fetch](https://grit-scm.com/docs/fetch/index.md), [grit merge](https://grit-scm.com/docs/merge/index.md), [grit push](https://grit-scm.com/docs/push/index.md)
