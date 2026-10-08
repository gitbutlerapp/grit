# grit pick

> Copy a single commit onto the current branch.

## Synopsis

```text
grit pick <commit>
```

## Description

Takes the change `<commit>` made and applies it to the current branch as a new commit (a cherry-pick). The new commit keeps the original author and message.

`<commit>` can be a full or short commit id, a branch name (to pick the commit at its tip) or an expression like `feature~2`.

`grit pick` works on one commit at a time and keeps it simple. It refuses, and changes nothing, when:

- you have uncommitted changes, or the current branch has no commits, or HEAD is detached
- `<commit>` is a merge commit
- `<commit>` doesn't change anything, or its change is already on the current branch
- the change conflicts with the current branch; the conflicting files are listed
- an untracked file would be replaced by a path the pick would check out (same rule as [`grit switch`](https://grit-scm.com/docs/switch/index.md))

## Options

| Option | Description |
| --- | --- |
| `<commit>` | The commit to copy onto the current branch. |

## Examples

Pick the latest commit from another branch:

```console
$ grit pick feature
Picked cf18394 → 4be20a1 Say hi
```

Pick a commit by id:

```console
$ grit pick 9a1c2e0
```

When an untracked file would be overwritten:

```console
$ grit pick feature
error: untracked file 'notes.txt' would be overwritten — move or remove it first
```

## JSON output

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `source` | string | Full id of the commit that was picked. |
| `oid` | string | Full id of the new commit on the current branch. |
| `subject` | string | Subject line of the picked commit. |

Example:

```json
{
  "source": "6e513d5cd6ab8d618238e22d7ca55d942b55964a",
  "oid": "6e513d5cd6ab8d618238e22d7ca55d942b55964a",
  "subject": "two"
}
```

## See also

[grit merge](https://grit-scm.com/docs/merge/index.md), [grit log](https://grit-scm.com/docs/log/index.md)
