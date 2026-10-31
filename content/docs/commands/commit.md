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
grit commit -q -m <message>
grit commit --allow-empty -m <message>
grit commit --amend [-m <message>]
grit commit --hunks <id,...> -m <message> [-- <path>…]
grit commit --patch-file <file|-> -m <message>
```

## Description

By default, `grit commit` **auto-stages** every change in the working tree (modified, deleted and untracked files) and records a new commit on the current branch. When auto-staging newly records a nested repository as a gitlink, grit prints the same **warning** as [`grit add`](add.md) on stderr (human and `--markdown`); `--json` includes `embedded_gitlinks` for paths introduced in that staging pass only (not when the gitlink was already in the index). Paths it cannot access because a directory is unreadable are left as-is in the index (with a `warning:` on stderr, same as [`grit status`](status.md)); other staged changes still commit. With `core.ignorecase=true` on a case-folding volume, auto-staging keeps indexed path spellings (same as [`grit add`](add.md)) and does not rewrite either spelling of a case-colliding path when the shared on-disk file already matches one of the indexed blobs (including directory-name collisions such as `Docs/` vs `docs/`); use [`grit mv`](mv.md) to change a path's case on purpose.

On a [GitButler workspace](../status/#gitbutler-workspaces), `grit commit` refuses (exit **1**, `kind: "gitbutler_workspace"`) and changes nothing, because GitButler rewrites the `gitbutler/workspace` branch and would lose the commit. Commit with `but commit`, or switch to a branch first.

Use **`--staged`** to commit the index as-is without auto-staging (Git’s `git commit` without `-a`). Use **`-a` / `--all`** to force auto-staging even when `commit.autoStage=false`.

Use **trailing paths** (`grit commit -m msg -- a b`) or **`--pathspec-from-file`** to commit only those paths from the worktree while leaving other staged changes in the index (Git’s `git commit --only --` paths). Partial path commits are refused while a merge, cherry-pick, or revert is in progress.

Use **`--hunks`** with stable ids from [`grit diff --json`](../diff/) to stage and commit only selected unstaged hunks in one step (like `git apply --cached` on those hunks, then commit). Optional **`-- paths`** narrow the diff scan the same way as [`grit add --hunks`](../add/). Use **`--patch-file`** to apply a unified diff to the index and commit the result (`-` reads stdin). These modes do not auto-stage the rest of the worktree (same effective scope as **`--staged`**). They cannot be combined with `-a` / `--all`, **`--staged`**, or path-only commits. **`--lines`** follows the same rules as `grit add --lines` when that flag is available.

The message can be given as an argument, with `-m`, or with `-F` / `--message-file` (`-` reads stdin). Messages may start with `-` when quoted on the shell (for example `grit commit -m "-fix typo"`) or passed via `--message=-fix typo`. A positional message that starts with `-` must contain whitespace (`grit commit "-fix typo"`): a single hyphen-leading word such as `-q`, `--bogus` or `--opt=value` is treated as an unknown flag and rejected with exit status `2` before anything is staged or committed, so a misspelled or unsupported flag never becomes the commit message. Pass such a message with `-m` (`grit commit -m --bogus`). Message text is normalized with Git-style stripspace before hooks run (leading/trailing blank lines collapse; a trailing newline is added). A message with several lines is recorded as is after normalization; its first line is the subject that [`grit log`](../log/) shows. A normal commit needs a message: without one, `grit commit` stops with an error and does not change the index or working tree. You cannot combine `-m`, a positional message, and `--message-file`.

Use **`--dry-run`** to preview what would be committed: path list, parents, and author, without writing objects, updating refs, running hooks, or changing the index on disk. The preview includes executable-bit-only changes the same way the commit does. Dry-run returns the same errors as a real commit for nothing-to-commit and unmerged index.

A commit either completes or changes nothing. Pressing Ctrl+C, or sending `SIGTERM` or `SIGHUP` on Unix, while the index is being updated or files are being checked out stops the commit with `error: operation cancelled`; `HEAD`, the index and the work tree are left as they were before the command started, no `.git/index.lock` is left behind, and the same commit can simply be run again. This works the same in a Windows console, where Ctrl+Break counts as Ctrl+C. A second interrupt terminates grit at once, without that cleanup; only `SIGKILL` skips rollback. Auto-staging may leave unreachable loose blobs in the object database if you interrupt mid-write; they are harmless. In the rare case that a path cannot be put back during checkout rollback, the error has kind `checkout_incomplete` and lists those paths in `paths`.

With `--amend`, `grit commit` replaces the **tip** commit on the current branch instead of adding a new one. It does not reword or squash older commits; positional arguments are always the commit message, not a revision (for example `grit commit --amend HEAD~1` records the message `HEAD~1`, it does not amend that commit). The new commit keeps the previous tip's parents and author; the committer and commit time come from your identity and `GIT_COMMITTER_DATE` as usual. If you omit the message while amending, the previous commit message is reused. Amending with no staged changes is allowed when the tree is unchanged (Git-compatible). You cannot amend on a branch that has no commits yet.

The author and committer come from `user.name` and `user.email` (see [`grit config`](../config/)), or from `GIT_AUTHOR_NAME` / `GIT_AUTHOR_EMAIL` and `GIT_COMMITTER_NAME` / `GIT_COMMITTER_EMAIL`. Use `--author "Name <email>"` (or a bare email) to override the author for one commit; the committer line still comes from your committer identity. If none of those are set and no repo or global `user.email` is configured, `grit commit` fails with an identity error instead of inventing an address (the same sources Git uses before it refuses with “unable to auto-detect email address”). Set identity once with:

```console
$ grit config --global user.name "Your Name"
$ grit config --global user.email "you@example.com"
```

To set the recorded time, use the `GIT_AUTHOR_DATE` and `GIT_COMMITTER_DATE` environment variables, or pass `--date` for the author timestamp on this commit (Git date formats, RFC 3339, `@epoch`, and the same loose strings Git accepts). On amend, the author line is taken from the commit being replaced unless you pass `--author` or `--reset-author`.

Repeat `--trailer key=value` (or `key: value`) to append trailers the same way `git interpret-trailers` would. `-s` / `--signoff` adds a `Signed-off-by` line for the committer. `--reset-author` sets the author to the committer with a fresh author date (also when amending). `-C` / `--reuse-message <commit>` reuses that commit's message and author; it is a copy and gets a new change id. `-S` / `--gpg-sign` and `--no-gpg-sign` override `commit.gpgsign` for one commit.

When `commit.gpgsign` is true (or `commit.gpgSign`), `grit commit` signs the new commit object the same way Git does: OpenPGP and X.509 via your configured `gpg`/`gpgsm` program, or SSH via `ssh-keygen -Y sign` when `gpg.format` is `ssh`. Signing uses `user.signingkey` and the same `gpg.*` settings Git reads. [`grit merge`](../merge/) and [`grit pick`](../pick/) honor the same policy when they create commits.

`grit commit` fails when there's nothing to commit (except when amending with an unchanged tree, or when you pass `--allow-empty`), when no message is provided (for a normal commit), and when the index still has **unmerged** paths after staging (for example when a merge conflict is not fully resolved in the working tree). With **`commit.autoStage=false`** or **`--staged`**, an empty index while the work tree still has changes is **`nothing staged to commit`** (not “working tree clean”): human output names how many files are unstaged or untracked and suggests `grit add` or `grit commit -- <path>`. With **`--json`**, `kind` is `nothing_staged` (dirty work tree, nothing staged) or `nothing_to_commit` (genuinely clean). Auto-staging does **not** treat a conflicted file as resolved while Git-style conflict markers (`<<<<<<<`, `=======`, `>>>>>>>`) remain in the worktree copy — edit the file to remove them, then run `grit commit` again. The same check applies when a file's conflict stages were dropped without editing it (for example by `git restore --staged` after a merge, pick, revert, rebase or `stash pop`): the commit fails with `kind` `unmerged_paths`, names the file, and leaves the index, `HEAD` and any operation state untouched, so `--abort` still works. Marker text that is already in the committed version of the file (for example an example in documentation) is not refused, but any new conflict block next to it is. Validation failures such as a missing message or empty message leave the index and working tree unchanged. When auto-staging finds changes that cancel out against `HEAD` (for example a new file that was staged and then deleted, or a staged edit that was reverted in the working tree), `grit commit` still records the staged index before reporting nothing to commit, so the index matches `HEAD` and `git status` is clean afterward; `--dry-run` never writes the index.

On a **detached HEAD** (for example after [`grit switch --detach`](../switch/)), `grit commit` records a new commit and moves `HEAD` to it without updating any branch — the same as `git commit` in that state. Use [`grit switch -c <name>`](../switch/) afterward to name the branch if you want to keep the work. With `--json`, a detached-HEAD commit reports `"branch": null` and `"detached": true`. A failed commit (missing message, nothing to commit) does not advance `HEAD` or stage your working tree on detached HEAD, the same as on a branch.

Auto-staging records the modes Git would. With `core.symlinks=false` (common on Windows, where Git and Grit check symlinks out as plain files holding the link target), a symlink stays a symlink (`120000`): an untouched link is not part of the commit, and editing the file stages the new link target. With `core.filemode=false`, a tracked file keeps the executable bit the index already has.

Symlinks to files, directories and missing paths are staged as link-target bytes, without clean filters or line-ending conversion. `--dry-run` uses the same comparison in serial and parallel scans: an unchanged tree returns `nothing_to_commit` under `--json`, and retargeted links appear in the preview just as they will in the commit. Preview leaves the index, refs and stored objects unchanged.

With `--allow-empty`, grit records a new commit whose tree matches the parent even when the working tree is clean (staging still runs first, the same as any other commit). Use this to mark a point in history (for example to trigger CI) without changing files. The resulting commit is valid for Git the same way as `git commit --allow-empty`.

When a merge is in progress (`MERGE_HEAD` is present) and every conflict is resolved in the index, `grit commit` records a **merge commit** with two parents (your branch tip and the merged tip) and clears merge state the same way Git does after `git commit`.

When a cherry-pick or revert is paused (`CHERRY_PICK_HEAD` or `REVERT_HEAD`) and conflicts are resolved, `grit commit` finishes the current step the same way `git commit` does during `git cherry-pick`: it keeps the picked commit’s **author** and **author date** (revert keeps the committer as author), uses `MERGE_MSG` when you omit `-m`, clears replay state, and writes a reflog entry `commit (cherry-pick): …` for picks. Auto-staging applies as usual; you do not need `grit pick --continue` after a resolving commit unless more steps remain in a multi-commit sequence.

During a **rebase conflict stop** (grit-owned `.git/rebase-merge/` with `grit-plan`), a resolving `grit commit` records the replayed pick on the detached replay parent: it keeps that commit’s **author**, **change id**, and `MERGE_MSG` when you omit `-m`, and leaves the rebase session in place so `grit rebase --continue` replays only the remaining commits.

`pre-commit`, `prepare-commit-msg` and `commit-msg` hooks run, in that order, before the commit object is written when they are installed (config `hook.*` entries or scripts under the hooks directory). `prepare-commit-msg` gets `.git/COMMIT_EDITMSG` and where the message came from: `message` for `-m`, `-F` or the argument, `commit <oid>` when `--amend` or a reused message keeps a commit's message, `merge` when finishing a merge with its `MERGE_MSG`, and `squash` after `grit merge --squash`. Both message hooks may edit the file, and the commit records what they leave. Pass `--no-verify` to skip `pre-commit` and `commit-msg`; `prepare-commit-msg` and `post-commit` still run, matching Git. A `pre-commit`, `prepare-commit-msg` or `commit-msg` hook that exits non-zero or cannot be started aborts the commit, and the error names the hook and its exit status (or the OS error). The index and working tree stay as they were before the attempt (including a deliberate staged/unstaged split on the same file and untracked paths not yet in the index). In human mode the hook's output appears on stderr as it runs, followed by the error. With `--json` the failure is a structured error with `kind: "hook_failed"` and a `hook` object carrying `name`, `status`, and the hook's captured `stdout` and `stderr`; `--markdown` prints the error followed by the captured output in `text` blocks. `post-commit` never changes the result: the commit is already written, so a non-zero exit is ignored, and a hook that cannot be started prints a warning on stderr (`hook_not_run`). With `--amend`, grit then runs `post-rewrite amend` with `<old commit> <new commit>` on stdin, as Git does (library callers can skip it with `CommitRequest::post_rewrite`); like `post-commit`, its outcome never changes the result. Dry-run skips all hooks. See [`grit hooks`](hooks.md).

After a successful commit (not `--dry-run`), grit checks `gc.auto` and `gc.autoPackLimit` like `git commit` does and, once loose objects or packs pile up, starts background maintenance with one stderr line in human mode. See [Automatic maintenance](gc.md#automatic-maintenance).

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

On an interactive terminal (human output, not `--json`, `--markdown` or `-q`), a live **Staging** line on stderr counts the files auto-staged before the commit; nothing is drawn when stderr is not a TTY.

## Options

| Option | Description |
| --- | --- |
| `<message>` | The commit message. A single hyphen-leading word is rejected as an unknown flag; use `-m` for it. |
| `-q`, `--quiet` | Print nothing on success in human mode. `--json` and `--markdown` output is unchanged; errors still go to stderr. |
| `-m`, `--message <message>` | The commit message, as an option (repeatable; each `-m` starts a new paragraph separated by a blank line, like Git). Do not combine with `-F` / `--message-file` or a positional message. Optional with `--amend` (keeps the previous message when omitted). |
| `-F`, `--message-file <file\|->` | Read the commit message from a file, or from stdin when the path is `-`. |
| `--author <ident>` | Author for this commit (`Name <email>` or bare email). Committer unchanged. |
| `--no-verify` | Skip `pre-commit` and `commit-msg` hooks (`prepare-commit-msg` still runs). |
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
| `--no-verify` | Skip `pre-commit` and `commit-msg` hooks; `prepare-commit-msg` and `post-commit` still run on a real commit. |

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

Schema: [v2](/schemas/commit/v2.json) · [v1](/schemas/commit/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `oid` | string or omitted | Full commit object id; omitted when `dry_run` is true |
| `branch` | string or null | Short name of the branch the commit advanced (e.g. `main`); `null` on a detached `HEAD` |
| `detached` | boolean | `true` when the commit was made on a detached `HEAD` (only `HEAD` moved) |
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
| `hooks_run` | array of strings | Hook names that ran, in order (`pre-commit`, `prepare-commit-msg`, `commit-msg`, `post-commit`, and `post-rewrite` after `--amend`); empty on dry-run |
| `embedded_gitlinks` | array | Nested repositories newly recorded as gitlinks during auto-staging for this commit. Omitted when empty. |

Example:

```json
{
  "oid": "92501f188ae0815af09a0cef6e121eddda4113cf",
  "branch": "main",
  "detached": false,
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

Schema: [v2](/schemas/commit/v2.json) · [v1](/schemas/commit/v1.json)
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

An unknown flag is a usage error even when it lands where the positional message goes; nothing is staged or committed:

```console
$ grit commit --bogus
error: unexpected argument '--bogus' found

  tip: to use '--bogus' as text, pass it with '-m --bogus'

Usage: grit commit [OPTIONS] [MESSAGE] [-- <PATH>...]

For more information, try '--help'.
```

See [Exit codes](../exit-codes/) for shared rules and structured `--json` `kind` values.

When another process holds `.git/index.lock`, `grit commit` exits `1` with `kind` `lock_held` under `--json`. `lock.path` names the lock file, and `lock.holder` gives its owning process and whether it still runs when `core.lockfilePid` recorded one; the message says whether the lock looks live or stale. Every scope checks the lock before it creates the commit, so on this error no commit was recorded and `HEAD`, the branch, the reflog and the index are unchanged. It is safe to retry once the other process finishes. Path commits (`-- paths`), `--hunks` and `--patch-file` take the lock before writing anything at all; auto-staging may leave an unreferenced blob behind for a later `git gc` to prune. Ordinary commands never remove a foreign lock; interrupted rewrite recovery may remove a stale grit index lock when its pid sidecar shows the holder exited.

```console
$ grit commit -m "fix" -- f1.txt
error: another process holds /work/repo/.git/index.lock; if none is running, remove the stale lock and retry
$ grit commit -m "fix" --json -- f1.txt
{"error":"another process holds /work/repo/.git/index.lock; if none is running, remove the stale lock and retry","kind":"lock_held","lock":{"path":"/work/repo/.git/index.lock"}}
```

Several `grit commit` processes can run in one checkout at once, for example parallel jobs each committing their own `-- paths`. The index lock serializes them: each one either records a commit with its own message and only its own paths, or fails with `lock_held` and changes nothing. The message always comes from `-m`, `-F` or the positional argument; `.git/COMMIT_EDITMSG` is read back only after a `prepare-commit-msg` or `commit-msg` hook ran, so one process never records another's message. A commit is built on the `HEAD` it read first: if another tool moves the branch before the commit lands, `grit commit` fails with `ref 'refs/heads/<branch>' did not match expected state` (exit `1`) instead of recording a commit that would undo the other change.


## See also

[grit add](../add/), [grit status](../status/), [grit log](../log/), [grit config](../config/)