---
title: grit pull
summary: Fetch from the remote and bring the current branch up to date.
group: Remotes
order: 3
---

## Synopsis

```text
grit pull
grit pull [<remote>] [<refspec>…]
grit pull --ff-only
grit pull --rebase
grit pull --no-rebase
grit pull --autostash
grit pull --check [<remote>] [<refspec>…]
```

## Description

Runs [`grit fetch`](../fetch/) (same shared fetch flags: tags, prune, dry-run, quiet), then merges the fetched upstream into the **current** branch, as [`grit merge`](../merge/) would: a fast-forward when you have no new commits of your own, and a merge commit when both sides do. On a branch with no commits yet, it simply takes the remote's.

Positional arguments follow [`grit fetch`](../fetch/): an optional remote (configured name, URL, or path), then optional refspecs. With `grit pull origin feature`, grit fetches that branch and integrates the tip recorded in `FETCH_HEAD`, even when no remote-tracking ref exists yet — the same model as `git pull origin feature`.

The default remote is `branch.<name>.remote`, or `origin`. With no refspecs, the upstream is `branch.<name>.merge`, or `refs/remotes/<remote>/<branch>`. The remote's `remote.<name>.fetch` refspecs decide which local ref holds each fetched branch. With `branch.<name>.remote = .` the upstream is a local branch, and `grit pull` merges it without fetching.

On a branch with no commits yet, the branch is created at the upstream commit with an `initial pull` reflog entry.

Like `grit merge`, `grit pull` uses the same scoped local-change rules as [`grit switch`](../switch/), won't run with a detached HEAD, and stops without changing anything if there are conflicts (except merge-mode conflicts, which leave merge state).

The fetch step uses the same terminal progress as [`grit fetch`](../fetch/) and [`grit clone`](../clone/): live stderr progress on a TTY in human mode, silent when stderr is piped or when you pass `--json` or `--markdown`. When you need an explicit fetch refspec or fetch-only flags (`--depth`, `--deepen`, `--unshallow`, `--tags`, `--no-tags`, `--prune`, `--no-prune`, `--dry-run`, `-q`), run [`grit fetch`](../fetch/) first, then integrate with `grit merge` or the pull integration flags below.

Integration mode follows Git's `pull.ff`, `pull.rebase`, and `branch.<name>.rebase` unless overridden on the command line. With rebase, local commits replay onto the fetched upstream tip via the history engine (no sequencer files). Rebase conflicts abort atomically: nothing changes and exit is **1** (stop/continue rebase is planned once the shared conflict model lands). `pull.rebase=interactive` is rejected. For a standalone rebase without fetching, use [`grit rebase`](rebase.md).

`--autostash` stashes dirty index/worktree changes (including untracked files) before integrating, then re-applies them afterward. Config `merge.autoStash` / `rebase.autoStash` enables the same behavior by default for merge and rebase pulls respectively. If re-applying the stash conflicts, the pull integration is kept, the stash entry remains on `refs/stash`, and the result is `autostash_conflict` (exit **1**).

`--check` fetches (always for real — not a dry run) and reports whether the branch could fast-forward or is already up to date, without changing `HEAD`, the index, the worktree, or reflogs. It cannot be combined with `--autostash` or integration mode overrides (`--ff-only`, `--rebase`, `--no-rebase`).

## Options

| Option | Description |
| --- | --- |
| `<remote>` | Remote to pull from (same rules as [`grit fetch`](../fetch/); defaults to the branch remote or `origin`). |
| `<refspec>…` | Refspecs to fetch before integrating (same as fetch); integration uses `FETCH_HEAD` when refspecs are given. |
| `--tags` / `--no-tags` | Tag fetching policy (same as [`grit fetch`](../fetch/)). |
| `--all` | Fetch all configured remotes before integrating (same as [`grit fetch`](../fetch/); cannot combine with `<remote>` or refspecs). |
| `--depth N` | Shallow fetch: keep at most N commits from each tip (same as fetch). |
| `--deepen N` | Deepen a shallow clone by N commits (same as fetch). |
| `--unshallow` | Fetch full history and drop shallow boundaries (same as fetch). |
| `--prune` / `--no-prune` | Prune stale remote-tracking refs after fetch. |
| `--dry-run` | Fetch dry-run only: report ref updates without writing refs or objects; integration is skipped. |
| `-q`, `--quiet` | Suppress fetch progress (human mode). |
| `--ff-only` | Refuse to create a merge commit when the current branch has local commits; exit with `refused_not_ff` in `--json` output instead. Honors `pull.ff=only` when set. Conflicts with `--rebase`, `--no-rebase`, and `--check`. |
| `--rebase` | Rebase local commits onto the upstream tip, overriding `pull.rebase=false`. Conflicts with `--ff-only`, `--no-rebase`, and `--check`. |
| `--no-rebase` | After fetch, merge the upstream with a merge commit when a fast-forward is not possible (the default when `pull.rebase` is not true). Conflicts with `--ff-only`, `--rebase`, and `--check`. |
| `--autostash` | Stash before integrating and re-apply after (see `merge.autoStash` / `rebase.autoStash`). Conflicts with `--check`. |
| `--check` | Fetch and report whether a fast-forward or up-to-date result is possible; exit **0** when yes, **1** when a merge or rebase would be required. Conflicts with `--autostash` and mode overrides. |

All [global options](../global-options/) apply as well.

## Examples

```console
$ grit pull
Receiving objects: 2.00 KiB, 4.00 KiB/s, done.
Fast-forwarded origin/main → b52cca6

$ grit pull origin feature
Fetched 1 update from origin.
Fast-forwarded origin/feature → d4e5f6a

$ grit pull --check
Would fast-forward (0 ahead, 2 behind). Fast-forward possible.

$ grit pull --autostash
Merged origin/main into the current branch (acd1d4b)
```

## JSON output

Schema: [v2](/schemas/pull/v2.json) · [v1](/schemas/pull/v1.json)

Pass `--json` for stable, scripting-friendly output:

Exit codes (also apply to default human output): **0** on success (`up_to_date`, `fast_forward`, `merged`, `rebased`, or `set_upstream`) and on `--check` when `check.fast_forward_possible` is true; **1** when `--ff-only` refuses diverged history (`refused_not_ff`), integration stops with conflicts (`conflicts`; rebase conflicts leave the repository unchanged), `--check` when a merge or rebase would be required, or `autostash_conflict`.

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `result` | string | `up_to_date`, `fast_forward`, `merged`, `rebased`, `refused_not_ff`, `conflicts`, `set_upstream`, or `autostash_conflict`. |
| `branch` | string | Remote branch label that was integrated (or checked). |
| `oid` | string or null | Commit the current branch points at after the pull, when applicable. |
| `change_id` | string or null | Stored `change-id` on `oid` when present. |
| `upstream` | string or null | For `set_upstream`, the remote-tracking branch that was adopted. |
| `conflicts` | array of strings or null | Path names when `result` is `conflicts` (legacy list). |
| `conflict_details` | array or null | Structured conflict items when `result` is `conflicts`. |
| `ahead` | integer or null | Local-only commits when `result` is `refused_not_ff` or with `--check`. |
| `behind` | integer or null | Upstream-only commits when `result` is `refused_not_ff` or with `--check`. |
| `check` | object or null | Present with `--check`: `ahead`, `behind`, `fast_forward_possible`, and `would` (same result strings as `result`). |

Example:

```json
{
  "result": "fast_forward",
  "branch": "origin/main",
  "oid": "b52cca65378cf17309f0b1f917c6dc4fdd9256f2"
}
```

With `--check`:

```json
{
  "result": "fast_forward",
  "branch": "origin/main",
  "ahead": 0,
  "behind": 2,
  "check": {
    "ahead": 0,
    "behind": 2,
    "fast_forward_possible": true,
    "would": "fast_forward"
  }
}
```

## Markdown output

Pass [`--markdown`](../global-options/) for agent-friendly output.

Schema: [v2](/schemas/pull/v2.json) · [v1](/schemas/pull/v1.json)

Markdown uses headings, bullet lists, and fenced blocks; pipe characters inside table cells are escaped. See the JSON output section for the stable field contract.

## See also

[grit fetch](../fetch/), [grit merge](../merge/), [grit push](../push/)
