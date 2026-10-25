---
title: Exit codes
summary: Process exit status for grit commands and structured JSON error kinds.
---

Grit uses a small set of exit codes. Command-specific yes/no checks (for example `grit diff --exit-code`) reuse **failure** (1) when the answer is “no”. See [Scripting with grit](../scripting/) and the [Agent guide](../agents/) for `--json` behavior.

## Exit status

| Slug | Code | Meaning |
| --- | ---: | --- |
| `success` | 0 | Command completed successfully (including some checks that answer “yes”). |
| `failure` | 1 | Command failed, or a yes/no check answered “no” (`diff --exit-code`, `merge --check`, `pick --check`, `revert --check`, `pull --check`, `merge-base --is-ancestor`, `fsck`, `ignore check`, `tag --verify`, conflicts after `merge` / `pick` / `revert` / `rebase`, rejected `push`, and similar). With `--json`, stdout is usually `{"error":"…"}` or a full outcome object when the failure is part of the result (for example rejected push). |
| `usage` | 2 | Invalid flags or arguments. With `--json`, stdout is still `{"error":"…"}` (clap usage errors and subcommand usage such as `grit branch`). Hunk selection mistakes (`add` / `commit` / `stash` / `restore` with unknown or stale `--hunks` ids) also exit 2. `grit diff --exit-code` / `--quiet` uses 2 when diff itself errors. |
| `remote_unreadable` | 128 | Clone or fetch could not read the remote (transport / HTTP failure). |
| `missing_prerequisite` | 128 | `grit bundle unbundle` when the repository lacks prerequisite commits. |
| `refused_not_ff` | 128 | `grit merge --ff-only` when a fast-forward is not possible. |
| `invalid_commit_filter` | 129 | Invalid `--author`, `--grep`, or pickaxe pattern on `grit log`, `grit revs`, or related commands (`kind`: `invalid_pattern`). |
| `custom` | *varies* | Passthrough from `grit hooks run` (hook exit code) or `grit rebase --exec` when a command fails. |

## Structured error kinds (`kind` in JSON)

These values appear in the `kind` field of structured `--json` errors ([error schema](/schemas/error/v1.json)). Plain failures omit `kind`.

When grit knows what to do next, the object also has a **`hint`** naming the command to run, and human output prints the same text on a `hint:` line after the `error:` line (Markdown output adds a `**Hint:**` paragraph). Failures about specific files list them in **`paths`** (or `conflicts` / `would_overwrite.paths`).

```console
$ grit push
error: no remote to push to
hint: add one with `grit remote add origin <url>`, or push to a URL with `grit push <url> <branch>`
$ grit push --json
{"error":"no remote to push to","hint":"add one with `grit remote add origin <url>`, or push to a URL with `grit push <url> <branch>`","kind":"no_remote"}
```

| Slug | When |
| --- | --- |
| `conflict` | Merge, pick, rebase, or similar would introduce conflicts (`conflicts`, optional `commit`). |
| `hash_collision` | Refusing to store content that triggers SHA-1 collision detection (`oid`). |
| `would_overwrite` | Checkout / switch would overwrite local or untracked files (`would_overwrite.reason`, `would_overwrite.paths`). The hint suggests `grit commit` or `grit stash` for local changes. |
| `operation_in_progress` | `grit switch` refused while a merge, rebase, pick or revert is paused (grit's or one `git` started); finish or abort it first. |
| `invalid_pattern` | Invalid filter pattern (exit 129). |
| `usage` | Subcommand usage mistake (exit 2), e.g. `grit branch` flag combinations. |
| `incompatible_flags` | Flags that cannot be combined (often `--json` with human-only options). |
| `index_locked` | Another process holds `.git/index.lock` (exit 1). The operation stopped before changing refs, the reflog or the index, so retrying later is safe; the lock file is left alone. |
| `branch_already_exists` | Create branch when the name already exists. |
| `branch_not_found` | Branch name not found. `grit switch` hints at `grit branch` and `grit switch -c <name>`. |
| `branch_invalid_name` | Invalid branch name. |
| `branch_not_fully_merged` | Delete refused: not fully merged. |
| `branch_checked_out` | Branch checked out in this or another worktree. |
| `branch_no_commits` | No commits in repository. |
| `branch_invalid_start_point` | Invalid `--start` / start point. |
| `branch_upstream_not_tracking` | Upstream is not a tracking branch. |
| `branch_no_upstream` | No upstream configured. `grit pull` hints at `grit pull origin <branch>` and `grit push -u origin <branch>`. |
| `branch_error` | Other branch operation failure. |
| `hook_failed` | A hook such as `pre-commit` exited non-zero (`hook.name`, `hook.status`, and the hook's captured `hook.stdout` / `hook.stderr`). |
| `revision_not_found` | A branch, tag, commit id or revision expression names nothing (for example `grit merge nonexistent`). |
| `no_remote` | `grit push` with no remote named, no push remote for the branch, and no `origin`. Hint: `grit remote add origin <url>`. |
| `remote_not_found` | A remote name that is not configured (`grit push origin main`, `grit pull`, `grit fetch nosuch`). Hint: `grit remote add <name> <url>`. |
| `remote_no_url` | The remote is configured but has no URL. Hint: `grit remote set-url <name> <url>`. |
| `auth_required` | An HTTPS remote wants credentials and none were available. The `error` names the repository URL; the hint is `grit auth` for github.com, otherwise a `credential.helper`. |
| `auth_rejected` | The remote refused the credentials that were sent. |
| `repository_not_found` | There is no repository at the HTTP URL (`404` on discovery). |
| `unmerged_paths` | `grit merge --continue` while conflicts are unresolved, or resolved but not staged with `commit.autoStage=false` (`paths`). The hint names the files and `grit merge --continue` / `grit merge --abort` (or `grit add`). |
| `dirty_worktree` | Staged or unstaged changes block a merge, pick, revert, switch or `rebase --exec` (`paths`). Hint: `grit commit` or `grit stash`. |
