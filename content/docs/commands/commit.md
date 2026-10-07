---
title: grit commit
summary: Stage every change and record a new commit.
group: Making changes
order: 2
---

## Synopsis

```
grit commit [-a] <message>
grit commit [-a] -m <message>
```

## Description

Stages every change in the working tree (modified, deleted and untracked files) and records a new commit on the current branch with the given message. This is the same as running [`grit add`](../add/) followed by a commit.

The message can be given as an argument or with `-m`. A message with several lines is recorded as is; its first line is the subject that [`grit log`](../log/) shows. A commit needs a message: without one, `grit commit` stages your changes and stops with an error.

The author and committer come from the `user.name` and `user.email` settings (see [`grit config`](../config/)). To set the recorded time, use the `GIT_AUTHOR_DATE` and `GIT_COMMITTER_DATE` environment variables.

`grit commit` fails when there's nothing to commit, and when HEAD is detached rather than on a branch.

## Options

| Option | Description |
| --- | --- |
| `<message>` | The commit message. |
| `-m`, `--message <message>` | The commit message, as an option. Use either this or the argument, not both. |
| `-a`, `--all` | Stage every change first. This is always what `grit commit` does; the option is accepted so that `git` habits like `-am` keep working. |

## Examples

Commit everything:

```
$ grit commit "Add the greeting"
[main 217c6f9] Add the greeting
2 changes committed
```

The same, written the way you'd write it for `git`:

```
$ grit commit -am "Add the greeting"
```

A longer message with a body:

```
$ grit commit -m "Add the greeting

Prints hi on startup so we know the binary runs."
```

## JSON output

```
{
  "oid": "217c6f958c3162926c9c9379e6d21b38f110e1ad",
  "branch": "main",
  "subject": "Add the greeting",
  "changes": 2
}
```

`oid` is the new commit's id and `changes` is the number of paths it changed.

## See also

[grit add](../add/), [grit status](../status/), [grit log](../log/), [grit config](../config/)
