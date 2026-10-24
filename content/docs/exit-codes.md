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

| Slug | When |
| --- | --- |
| `conflict` | Merge, pick, rebase, or similar would introduce conflicts (`conflicts`, optional `commit`). |
| `hash_collision` | Refusing to store content that triggers SHA-1 collision detection (`oid`). |
| `would_overwrite` | Checkout / switch would overwrite local or untracked files (`would_overwrite.reason`, `would_overwrite.paths`). |
| `invalid_pattern` | Invalid filter pattern (exit 129). |
| `usage` | Subcommand usage mistake (exit 2), e.g. `grit branch` flag combinations. |
| `incompatible_flags` | Flags that cannot be combined (often `--json` with human-only options). |
| `index_locked` | Another process holds `.git/index.lock` (exit 1). The operation stopped before changing anything, so retrying later is safe; the lock file is left alone. |
| `branch_already_exists` | Create branch when the name already exists. |
| `branch_not_found` | Branch name not found. |
| `branch_invalid_name` | Invalid branch name. |
| `branch_not_fully_merged` | Delete refused: not fully merged. |
| `branch_checked_out` | Branch checked out in this or another worktree. |
| `branch_no_commits` | No commits in repository. |
| `branch_invalid_start_point` | Invalid `--start` / start point. |
| `branch_upstream_not_tracking` | Upstream is not a tracking branch. |
| `branch_no_upstream` | No upstream configured. |
| `branch_error` | Other branch operation failure. |
