# grit commit

> Stage every change and record a new commit.

## Synopsis

```text
grit commit [-a] <message>
grit commit [-a] -m <message>
grit commit --amend [-m <message>]
```

## Description

Stages every change in the working tree (modified, deleted and untracked files) and records a new commit on the current branch with the given message. This is the same as running [`grit add`](https://grit-scm.com/docs/add/index.md) followed by a commit.

The message can be given as an argument or with `-m`. A message with several lines is recorded as is; its first line is the subject that [`grit log`](https://grit-scm.com/docs/log/index.md) shows. A normal commit needs a message: without one, `grit commit` stages your changes and stops with an error.

With `--amend`, `grit commit` replaces the tip commit on the current branch instead of adding a new one. The new commit keeps the previous tip's parents and author; the committer and commit time come from your identity and `GIT_COMMITTER_DATE` as usual. If you omit the message while amending, the previous commit message is reused. Amending with no staged changes is allowed when the tree is unchanged (Git-compatible). You cannot amend on a branch that has no commits yet.

The author and committer come from the `user.name` and `user.email` settings (see [`grit config`](https://grit-scm.com/docs/config/index.md)). To set the recorded time, use the `GIT_AUTHOR_DATE` and `GIT_COMMITTER_DATE` environment variables. On amend, the author line is taken from the commit being replaced, not from a fresh author identity.

When `commit.gpgsign` is true (or `commit.gpgSign`), `grit commit` signs the new commit object the same way Git does: OpenPGP and X.509 via your configured `gpg`/`gpgsm` program, or SSH via `ssh-keygen -Y sign` when `gpg.format` is `ssh`. Signing uses `user.signingkey` and the same `gpg.*` settings Git reads. [`grit merge`](https://grit-scm.com/docs/merge/index.md) and [`grit pick`](https://grit-scm.com/docs/pick/index.md) honor the same policy when they create commits.

`grit commit` fails when there's nothing to commit (except when amending with an unchanged tree), when HEAD is detached rather than on a branch, and when the index still has **unmerged** paths (for example after a merge conflict). Resolve conflicts, stage the result with [`grit add`](https://grit-scm.com/docs/add/index.md), then commit again.

When a merge is in progress (`MERGE_HEAD` is present) and every conflict is resolved in the index, `grit commit` records a **merge commit** with two parents (your branch tip and the merged tip) and clears merge state the same way Git does after `git commit`.

## Options

| Option | Description |
| --- | --- |
| `<message>` | The commit message. |
| `-m`, `--message <message>` | The commit message, as an option. Use either this or the argument, not both. Optional with `--amend` (keeps the previous message). |
| `-a`, `--all` | Stage every change first. This is always what `grit commit` does; the option is accepted so that `git` habits like `-am` keep working. |
| `--amend` | Replace the tip commit on the current branch. |

## Examples

Commit everything:

```console
$ grit commit "Add the greeting"
[main 217c6f9] Add the greeting
2 changes committed
```

The same, written the way you'd write it for `git`:

```console
$ grit commit -am "Add the greeting"
```

A longer message with a body:

```console
$ grit commit -m "Add the greeting

Prints hi on startup so we know the binary runs."
```

Amend the latest commit after fixing staged files:

```console
$ grit commit --amend -m "Add the greeting (fixed typo)"
```

## JSON output

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `oid` | string | Full commit object id (40 hex chars for SHA-1) |
| `branch` | string | Short branch name (e.g. `main`) |
| `subject` | string | First line of the commit message |
| `changes` | number | Path changes vs the parent commit tree |
| `amended` | boolean | `true` when `--amend` was used |

Example:

```json
{
  "oid": "92501f188ae0815af09a0cef6e121eddda4113cf",
  "branch": "main",
  "subject": "second",
  "changes": 1,
  "amended": false
}
```

## See also

[grit add](https://grit-scm.com/docs/add/index.md), [grit status](https://grit-scm.com/docs/status/index.md), [grit log](https://grit-scm.com/docs/log/index.md), [grit config](https://grit-scm.com/docs/config/index.md)
