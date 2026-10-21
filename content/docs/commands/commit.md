---
title: grit commit
summary: Stage every change and record a new commit.
group: Making changes
order: 2
---

## Synopsis

```text
grit commit [-a] <message>
grit commit [-a] -m <message>
grit commit -F <file|->
grit commit --author "Name <email>"
grit commit --no-verify -m <message>
grit commit --allow-empty -m <message>
grit commit --amend [-m <message>]
```

## Description

Stages every change in the working tree (modified, deleted and untracked files) and records a new commit on the current branch with the given message. This is the same as running [`grit add`](../add/) followed by a commit.

The message can be given as an argument, with `-m`, or with `-F` / `--message-file` (`-` reads stdin). Message text is normalized with Git-style stripspace before hooks run (leading/trailing blank lines collapse; a trailing newline is added). A message with several lines is recorded as is after normalization; its first line is the subject that [`grit log`](../log/) shows. A normal commit needs a message: without one, `grit commit` stages your changes and stops with an error. You cannot combine `-m`, a positional message, and `--message-file`.

With `--amend`, `grit commit` replaces the **tip** commit on the current branch instead of adding a new one. It does not reword or squash older commits; positional arguments are always the commit message, not a revision (for example `grit commit --amend HEAD~1` records the message `HEAD~1`, it does not amend that commit). The new commit keeps the previous tip's parents and author; the committer and commit time come from your identity and `GIT_COMMITTER_DATE` as usual. If you omit the message while amending, the previous commit message is reused. Amending with no staged changes is allowed when the tree is unchanged (Git-compatible). You cannot amend on a branch that has no commits yet.

The author and committer come from `user.name` and `user.email` (see [`grit config`](../config/)), or from `GIT_AUTHOR_NAME` / `GIT_AUTHOR_EMAIL` and `GIT_COMMITTER_NAME` / `GIT_COMMITTER_EMAIL`. Use `--author "Name <email>"` (or a bare email) to override the author for one commit; the committer line still comes from your committer identity. If none of those are set and no repo or global `user.email` is configured, `grit commit` fails with an identity error instead of inventing an address (the same sources Git uses before it refuses with “unable to auto-detect email address”). Set identity once with:

```console
$ grit config --global user.name "Your Name"
$ grit config --global user.email "you@example.com"
```

To set the recorded time, use the `GIT_AUTHOR_DATE` and `GIT_COMMITTER_DATE` environment variables. On amend, the author line is taken from the commit being replaced, not from a fresh author identity.

When `commit.gpgsign` is true (or `commit.gpgSign`), `grit commit` signs the new commit object the same way Git does: OpenPGP and X.509 via your configured `gpg`/`gpgsm` program, or SSH via `ssh-keygen -Y sign` when `gpg.format` is `ssh`. Signing uses `user.signingkey` and the same `gpg.*` settings Git reads. [`grit merge`](../merge/) and [`grit pick`](../pick/) honor the same policy when they create commits.

`grit commit` fails when there's nothing to commit (except when amending with an unchanged tree, or when you pass `--allow-empty`), when HEAD is detached rather than on a branch, and when the index still has **unmerged** paths after staging (for example when a merge conflict is not fully resolved in the working tree). Fix conflicts in your files, then run `grit commit` again — staging runs automatically, the same as for other changes. Refusal for a detached HEAD does not stage your working tree (unlike a missing message, which intentionally stages first so you can add a message and run `grit commit` again).

With `--allow-empty`, grit records a new commit whose tree matches the parent even when the working tree is clean (staging still runs first, the same as any other commit). Use this to mark a point in history (for example to trigger CI) without changing files. The resulting commit is valid for Git the same way as `git commit --allow-empty`.

When a merge is in progress (`MERGE_HEAD` is present) and every conflict is resolved in the index, `grit commit` records a **merge commit** with two parents (your branch tip and the merged tip) and clears merge state the same way Git does after `git commit`.

`pre-commit` and `commit-msg` hooks run before the commit object is written when they are installed (config `hook.*` entries or scripts under the hooks directory). Pass `--no-verify` to skip both; `post-commit` still runs afterward, matching Git. See [`grit hooks`](hooks.md).

### Change ids

Every commit `grit commit` creates gets a `change-id` header (reverse-hex by default) that stays with the same logical change across amends. Commits created without the header still have a deterministic id derived from the commit object id (Jujutsu-compatible); amending such a commit stores that derived value. Set `grit.changeIds=false` to omit the header (Git byte-identical commits for tests or importers that reject extra headers). See [Change ids](../library/change-ids/) for policy, testing knobs, and library APIs.

## Options

| Option | Description |
| --- | --- |
| `<message>` | The commit message. |
| `-m`, `--message <message>` | The commit message, as an option. Use either this, the argument, or `--message-file`, not more than one. Optional with `--amend` (keeps the previous message). |
| `-F`, `--message-file <file\|->` | Read the commit message from a file, or from stdin when the path is `-`. |
| `--author <ident>` | Author for this commit (`Name <email>` or bare email). Committer unchanged. |
| `--no-verify` | Skip `pre-commit` and `commit-msg` hooks. |
| `-a`, `--all` | Stage every change first. This is always what `grit commit` does; the option is accepted so that `git` habits like `-am` keep working. |
| `--amend` | Replace the tip commit on the current branch. |
| `--allow-empty` | Record a commit even when staged content matches the parent tree (empty tree change). |

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

Record an empty commit on a clean tree:

```console
$ grit commit --allow-empty -m "Trigger CI"
[main a1b2c3d] Trigger CI
0 changes committed
```

## JSON output

Schema: [v1](/schemas/commit/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `oid` | string | Full commit object id (40 hex chars for SHA-1) |
| `branch` | string | Short branch name (e.g. `main`) |
| `subject` | string | First line of the commit message |
| `changes` | number | Path changes vs the parent commit tree |
| `amended` | boolean | `true` when `--amend` was used |
| `parents` | string[] | Parent commit OIDs (empty for the root commit) |
| `author` | object | `{ "name", "email", "date" }` recorded on the commit (`date` is `<unix> <tz>`) |
| `hooks_run` | string[] | Hook basenames that were installed and ran successfully (`pre-commit`, `commit-msg`, `post-commit`) |

Example:

```json
{
  "oid": "92501f188ae0815af09a0cef6e121eddda4113cf",
  "branch": "main",
  "subject": "second",
  "changes": 1,
  "amended": false,
  "parents": ["abc123…"],
  "author": {
    "name": "Your Name",
    "email": "you@example.com",
    "date": "1700000000 +0000"
  },
  "hooks_run": ["pre-commit", "post-commit"]
}
```

Human output includes a `hooks:` line only when at least one hook ran.

## Markdown output

Schema: [v1](/schemas/commit/v1.json)
Pass [`--markdown`](../global-options/) for a `Commit` heading, branch and commit summary, and change count:

```text
- **`main`** · `9244cbd` — third
- **3** changes committed
```

With `--amend`, the first bullet notes the amended branch tip:

```text
- Amended **`main`** at `2fb4e89` — third (amended)
- **3** changes committed
```

## See also

[grit add](../add/), [grit status](../status/), [grit log](../log/), [grit config](../config/)
