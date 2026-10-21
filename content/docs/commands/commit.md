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
grit commit --staged -m <message>
grit commit -m <message> -- [<path>…]
grit commit --allow-empty -m <message>
grit commit --amend [-m <message>]
```

## Description

By default, `grit commit` **auto-stages** every change in the working tree (modified, deleted and untracked files) and records a new commit on the current branch. That matches `commit.autoStage=true` (the default). Set `commit.autoStage=false` in config to make the default commit only what is already staged (same as passing `--staged`).

Use **`--staged`** to commit the index as-is without auto-staging (Git’s `git commit` without `-a`). Use **`-a` / `--all`** to force auto-staging even when `commit.autoStage=false`.

Use **trailing paths** (`grit commit -m msg -- a b`) or **`--pathspec-from-file`** to commit only those paths from the worktree while leaving other staged changes in the index (Git’s `git commit --only --` paths). Partial path commits are refused while a merge is in progress.

The message can be given as an argument or with `-m`. A message with several lines is recorded as is; its first line is the subject that [`grit log`](../log/) shows. A normal commit needs a message: without one, `grit commit` stages your changes and stops with an error.

With `--amend`, `grit commit` replaces the **tip** commit on the current branch instead of adding a new one. It does not reword or squash older commits; positional arguments are always the commit message, not a revision (for example `grit commit --amend HEAD~1` records the message `HEAD~1`, it does not amend that commit). The new commit keeps the previous tip's parents and author; the committer and commit time come from your identity and `GIT_COMMITTER_DATE` as usual. If you omit the message while amending, the previous commit message is reused. Amending with no staged changes is allowed when the tree is unchanged (Git-compatible). You cannot amend on a branch that has no commits yet.

The author and committer come from `user.name` and `user.email` (see [`grit config`](../config/)), or from `GIT_AUTHOR_NAME` / `GIT_AUTHOR_EMAIL` and `GIT_COMMITTER_NAME` / `GIT_COMMITTER_EMAIL`. If none of those are set and no repo or global `user.email` is configured, `grit commit` fails with an identity error instead of inventing an address (the same sources Git uses before it refuses with “unable to auto-detect email address”). Set identity once with:

```console
$ grit config --global user.name "Your Name"
$ grit config --global user.email "you@example.com"
```

To set the recorded time, use the `GIT_AUTHOR_DATE` and `GIT_COMMITTER_DATE` environment variables. On amend, the author line is taken from the commit being replaced, not from a fresh author identity.

When `commit.gpgsign` is true (or `commit.gpgSign`), `grit commit` signs the new commit object the same way Git does: OpenPGP and X.509 via your configured `gpg`/`gpgsm` program, or SSH via `ssh-keygen -Y sign` when `gpg.format` is `ssh`. Signing uses `user.signingkey` and the same `gpg.*` settings Git reads. [`grit merge`](../merge/) and [`grit pick`](../pick/) honor the same policy when they create commits.

`grit commit` fails when there's nothing to commit (except when amending with an unchanged tree, or when you pass `--allow-empty`), when HEAD is detached rather than on a branch, and when the index still has **unmerged** paths after staging (for example when a merge conflict is not fully resolved in the working tree). Fix conflicts in your files, then run `grit commit` again — staging runs automatically, the same as for other changes. Refusal for a detached HEAD does not stage your working tree (unlike a missing message, which intentionally stages first so you can add a message and run `grit commit` again).

With `--allow-empty`, grit records a new commit whose tree matches the parent even when the working tree is clean (staging still runs first, the same as any other commit). Use this to mark a point in history (for example to trigger CI) without changing files. The resulting commit is valid for Git the same way as `git commit --allow-empty`.

When a merge is in progress (`MERGE_HEAD` is present) and every conflict is resolved in the index, `grit commit` records a **merge commit** with two parents (your branch tip and the merged tip) and clears merge state the same way Git does after `git commit`.

### Change ids

Every commit `grit commit` creates gets a `change-id` header (reverse-hex by default) that stays with the same logical change across amends. Commits created without the header still have a deterministic id derived from the commit object id (Jujutsu-compatible); amending such a commit stores that derived value. Set `grit.changeIds=false` to omit the header (Git byte-identical commits for tests or importers that reject extra headers). See [Change ids](../change-ids/) for interop, display, revision syntax, and library APIs.

## Options

| Option | Description |
| --- | --- |
| `<message>` | The commit message. |
| `-m`, `--message <message>` | The commit message, as an option. Use either this or the argument, not both. Optional with `--amend` (keeps the previous message). |
| `-a`, `--all` | Stage every change first, then commit. Conflicts with `--staged` and path arguments. |
| `--staged` | Commit only what is already in the index (no auto-stage). Conflicts with `-a` and path arguments. |
| `--pathspec-from-file`, `--pathspec-file-nul` | Same as on [`grit add`](../add/): read pathspecs from a file; combine with trailing `PATH` arguments. |
| `<path>…` | After `--`, limit the commit to these paths (partial commit). |
| `--amend` | Replace the tip commit on the current branch. |
| `--allow-empty` | Record a commit even when staged content matches the parent tree (empty tree change). |
| `--dry-run` | Compute the commit tree and file list without writing objects, updating refs, or running hooks. |
| `-F`, `--message-file <path>` | Read the commit message from a file; use `-` for stdin (same body rules as `-m`). |
| `--author <author>` | Override the author line (`Name <email>`); committer identity is unchanged unless you also set `GIT_COMMITTER_*`. |
| `--no-verify` | Skip `pre-commit` and `commit-msg` hooks; `post-commit` still runs on a real commit. |

### Config

| Key | Default | Meaning |
| --- | --- | --- |
| `commit.autoStage` | `true` | When true, `grit commit` with no scope flags auto-stages the whole worktree before committing. When false, the default is `--staged`. |

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
| `change_id` | string or null | Stored `change-id` header on the new commit, or `null` when absent |
| `scope` | string | `auto_stage`, `staged`, or `paths` — how the index was prepared |
| `dry_run` | boolean | `true` when `--dry-run` was passed |
| `files` | array | Path-level changes (`path`, `status`, optional `old_path` for renames) |
| `parents` | array of strings | Parent commit OIDs that would be stored |
| `author` | object | `name`, `email`, and `date` recorded on the commit |
| `hooks_run` | array of strings | Hook names that ran (`pre-commit`, `commit-msg`); empty on dry-run |

Example:

```json
{
  "oid": "92501f188ae0815af09a0cef6e121eddda4113cf",
  "branch": "main",
  "subject": "second",
  "changes": 1,
  "amended": false,
  "change_id": "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
  "scope": "auto_stage"
}
```

Commit only staged paths when another file is staged but should not be in this commit:

```console
$ grit add baz
$ grit commit -m "file only" -- file
```

Commit the index without auto-staging (with `commit.autoStage=true`, or always with `--staged`):

```console
$ grit add a.txt
$ grit commit --staged -m "index only"
```

Preview a commit without writing (message required):

```console
$ grit commit --dry-run -m "preview"
```

Read the message from stdin (agents):

```console
$ printf '%s\n' "Subject line" "" "Body." | grit commit --staged -F -
```

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
