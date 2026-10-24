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
grit commit -F <file|->
grit commit --author "Name <email>"
grit commit --no-verify -m <message>
grit commit --dry-run -m <message>
grit commit --allow-empty -m <message>
grit commit --amend [-m <message>]
grit commit --hunks <id,...> -m <message> [-- <path>…]
grit commit --patch-file <file|-> -m <message>
```

## Description

By default, `grit commit` **auto-stages** every change in the working tree (modified, deleted and untracked files) and records a new commit on the current branch.

Use **`--staged`** to commit the index as-is without auto-staging (Git’s `git commit` without `-a`). Use **`-a` / `--all`** to force auto-staging even when `commit.autoStage=false`.

Use **trailing paths** (`grit commit -m msg -- a b`) or **`--pathspec-from-file`** to commit only those paths from the worktree while leaving other staged changes in the index (Git’s `git commit --only --` paths). Partial path commits are refused while a merge is in progress.

Use **`--hunks`** with stable ids from [`grit diff --json`](../diff/) to stage and commit only selected unstaged hunks in one step (like `git apply --cached` on those hunks, then commit). Optional **`-- paths`** narrow the diff scan the same way as [`grit add --hunks`](../add/). Use **`--patch-file`** to apply a unified diff to the index and commit the result (`-` reads stdin). These modes do not auto-stage the rest of the worktree (same effective scope as **`--staged`**). They cannot be combined with `-a` / `--all`, **`--staged`**, or path-only commits. **`--lines`** follows the same rules as `grit add --lines` when that flag is available.

The message can be given as an argument, with `-m`, or with `-F` / `--message-file` (`-` reads stdin). Messages may start with `-` when quoted on the shell (for example `grit commit -m "-fix typo"`) or passed via `--message=-fix typo`. Message text is normalized with Git-style stripspace before hooks run (leading/trailing blank lines collapse; a trailing newline is added). A message with several lines is recorded as is after normalization; its first line is the subject that [`grit log`](../log/) shows. A normal commit needs a message: without one, `grit commit` stops with an error and does not change the index or working tree. You cannot combine `-m`, a positional message, and `--message-file`.

Use **`--dry-run`** to preview what would be committed: path list, parents, and author, without writing objects, updating refs, running hooks, or changing the index on disk. Dry-run returns the same errors as a real commit for nothing-to-commit and unmerged index.

With `--amend`, `grit commit` replaces the **tip** commit on the current branch instead of adding a new one. It does not reword or squash older commits; positional arguments are always the commit message, not a revision (for example `grit commit --amend HEAD~1` records the message `HEAD~1`, it does not amend that commit). The new commit keeps the previous tip's parents and author; the committer and commit time come from your identity and `GIT_COMMITTER_DATE` as usual. If you omit the message while amending, the previous commit message is reused. Amending with no staged changes is allowed when the tree is unchanged (Git-compatible). You cannot amend on a branch that has no commits yet.

The author and committer come from `user.name` and `user.email` (see [`grit config`](../config/)), or from `GIT_AUTHOR_NAME` / `GIT_AUTHOR_EMAIL` and `GIT_COMMITTER_NAME` / `GIT_COMMITTER_EMAIL`. Use `--author "Name <email>"` (or a bare email) to override the author for one commit; the committer line still comes from your committer identity. If none of those are set and no repo or global `user.email` is configured, `grit commit` fails with an identity error instead of inventing an address (the same sources Git uses before it refuses with “unable to auto-detect email address”). Set identity once with:

```console
$ grit config --global user.name "Your Name"
$ grit config --global user.email "you@example.com"
```

To set the recorded time, use the `GIT_AUTHOR_DATE` and `GIT_COMMITTER_DATE` environment variables, or pass `--date` for the author timestamp on this commit (Git date formats, RFC 3339, `@epoch`, and the same loose strings Git accepts). On amend, the author line is taken from the commit being replaced unless you pass `--author` or `--reset-author`.

Repeat `--trailer key=value` (or `key: value`) to append trailers the same way `git interpret-trailers` would. `-s` / `--signoff` adds a `Signed-off-by` line for the committer. `--reset-author` sets the author to the committer with a fresh author date (also when amending). `-C` / `--reuse-message <commit>` reuses that commit's message and author; it is a copy and gets a new change id. `-S` / `--gpg-sign` and `--no-gpg-sign` override `commit.gpgsign` for one commit.

When `commit.gpgsign` is true (or `commit.gpgSign`), `grit commit` signs the new commit object the same way Git does: OpenPGP and X.509 via your configured `gpg`/`gpgsm` program, or SSH via `ssh-keygen -Y sign` when `gpg.format` is `ssh`. Signing uses `user.signingkey` and the same `gpg.*` settings Git reads. [`grit merge`](../merge/) and [`grit pick`](../pick/) honor the same policy when they create commits.

`grit commit` fails when there's nothing to commit (except when amending with an unchanged tree, or when you pass `--allow-empty`), when no message is provided (for a normal commit), and when the index still has **unmerged** paths after staging (for example when a merge conflict is not fully resolved in the working tree). Auto-staging does **not** treat a conflicted file as resolved while Git-style conflict markers (`<<<<<<<`, `=======`, `>>>>>>>`) remain in the worktree copy — edit the file to remove them, then run `grit commit` again. Validation failures such as a missing message, empty message, or nothing-to-commit leave the index and working tree unchanged.

On a **detached HEAD** (for example after [`grit switch --detach`](../switch/)), `grit commit` records a new commit and moves `HEAD` to it without updating any branch — the same as `git commit` in that state. Use [`grit switch -c <name>`](../switch/) afterward to name the branch if you want to keep the work. A failed commit (missing message, nothing to commit) does not advance `HEAD` or stage your working tree on detached HEAD, the same as on a branch.

With `--allow-empty`, grit records a new commit whose tree matches the parent even when the working tree is clean (staging still runs first, the same as any other commit). Use this to mark a point in history (for example to trigger CI) without changing files. The resulting commit is valid for Git the same way as `git commit --allow-empty`.

When a merge is in progress (`MERGE_HEAD` is present) and every conflict is resolved in the index, `grit commit` records a **merge commit** with two parents (your branch tip and the merged tip) and clears merge state the same way Git does after `git commit`.

`pre-commit` and `commit-msg` hooks run before the commit object is written when they are installed (config `hook.*` entries or scripts under the hooks directory). Pass `--no-verify` to skip both; `post-commit` still runs afterward, matching Git. A `pre-commit` or `commit-msg` hook that exits non-zero or cannot be started aborts the commit, and the error names the hook and its exit status (or the OS error). In human mode the hook's output appears on stderr as it runs, followed by the error. With `--json` the failure is a structured error with `kind: "hook_failed"` and a `hook` object carrying `name`, `status`, and the hook's captured `stdout` and `stderr`; `--markdown` prints the error followed by the captured output in `text` blocks. `post-commit` never changes the result: the commit is already written, so a non-zero exit is ignored, and a hook that cannot be started prints a warning on stderr (`hook_not_run`). Dry-run skips all hooks. See [`grit hooks`](hooks.md).

For a `pre-commit` hook that prints a line on each stream and exits 1, `grit commit --json` prints (exit status 1):

```json
{
  "error": "hook pre-commit failed with exit status 1",
  "kind": "hook_failed",
  "hook": {
    "name": "pre-commit",
    "status": 1,
    "stdout": "stdout from hook\n",
    "stderr": "lint failed: bad stuff\n"
  }
}
```

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
| `--dry-run` | Preview the commit; no objects, refs, hooks, or on-disk index changes. |
| `-a`, `--all` | Stage every change first, then commit. Conflicts with `--staged` and path arguments. |
| `--staged` | Commit only what is already in the index (no auto-stage). Conflicts with `-a` and path arguments. |
| `--hunks` | Commit only listed hunk ids from `grit diff --json` (comma-separated, repeatable). Optional `-- paths` narrow the diff scan. Conflicts with `--patch-file`, `--lines`, `-a`, and `--staged`. |
| `--patch-file` | Apply a unified diff to the index, then commit (`-` for stdin). Conflicts with `--hunks`, `--lines`, `-a`, and `--staged`. |
| `--lines` | Commit only line ranges from the unstaged diff (same format as [`grit add --lines`](../add/)). Mix with `--hunks`. Optional `-- paths`. Conflicts with `--patch-file`, `-a`, and `--staged`. |
| `--pathspec-from-file`, `--pathspec-file-nul` | Same as on [`grit add`](../add/): read pathspecs from a file; combine with trailing `PATH` arguments. |
| `--amend` | Replace the tip commit on the current branch. |
| `--allow-empty` | Record a commit even when staged content matches the parent tree (empty tree change). |
| `--dry-run` | Compute the commit tree and file list without writing objects, updating refs, or running hooks. |
| `-F`, `--message-file <path>` | Read the commit message from a file; use `-` for stdin (same body rules as `-m`). |
| `--author <author>` | Override the author line (`Name <email>`); committer identity is unchanged unless you also set `GIT_COMMITTER_*`. |
| `--no-verify` | Skip `pre-commit` and `commit-msg` hooks; `post-commit` still runs on a real commit. |

## Examples

Commit everything:

```console
$ grit commit "Add the greeting"
[main 217c6f9] Add the greeting
2 changes committed
```

Preview before committing:

```console
$ grit commit --dry-run -m "Add the greeting"
Would commit 2 files
  M README
  A src/main.rs
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

## JSON output

Schema: [v1](/schemas/commit/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `oid` | string or omitted | Full commit object id; omitted when `dry_run` is true |
| `branch` | string | Short branch name (e.g. `main`) |
| `subject` | string | First line of the commit message |
| `changes` | number | Path changes vs the parent commit tree |
| `amended` | boolean | `true` when `--amend` was used |
| `dry_run` | boolean | `true` when `--dry-run` was used |
| `scope` | string | `auto_stage`, `staged`, `paths`, or `patch` — how the index was prepared |
| `dry_run` | boolean | `true` when `--dry-run` was passed |
| `files` | array | Path-level changes (`path`, `status`, optional `old_path` for renames) |
| `change_id` | string or null | Stored `change-id` header on the new commit, or null when absent or dry-run (see [Change ids](../library/change-ids/)) |
| `parents` | array of strings | Parent commit OIDs that would be stored |
| `author` | object | `name`, `email`, and `date` (RFC 3339, same shape as [`grit log`](../log/) / [`grit show`](../show/)) |
| `hooks_run` | array of strings | Hook names that ran (`pre-commit`, `commit-msg`); empty on dry-run |

Example:

```json
{
  "oid": "92501f188ae0815af09a0cef6e121eddda4113cf",
  "branch": "main",
  "subject": "second",
  "changes": 1,
  "amended": false,
  "dry_run": false,
  "scope": "auto_stage",
  "files": [{ "path": "b.txt", "status": "M" }],
  "change_id": "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
  "parents": ["abc123…"],
  "author": {
    "name": "Your Name",
    "email": "you@example.com",
    "date": "2023-11-14T22:13:20Z"
  },
  "hooks_run": ["pre-commit", "post-commit"]
}
```

Human output includes a `hooks:` line only when at least one hook ran.

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

With `--dry-run`, output lists the paths that would be committed instead of a new OID.


## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Success ([`success`](../exit-codes/)). |
| `1` | Command failed ([`failure`](../exit-codes/)). |
| `2` | Invalid flags or arguments ([`usage`](../exit-codes/)). |

See [Exit codes](../exit-codes/) for shared rules and structured `--json` `kind` values.


## See also

[grit add](../add/), [grit status](../status/), [grit log](../log/), [grit config](../config/)