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
grit pull <branch>                 # in a snapshot checkout
grit pull origin <branch>          # the same
```

## Description

Runs [`grit fetch`](../fetch/) (same shared fetch flags: tags, prune, dry-run, quiet), then merges the fetched upstream into the **current** branch, as [`grit merge`](../merge/) would: a fast-forward when you have no new commits of your own, and a merge commit when both sides do. On a branch with no commits yet, it simply takes the remote's.

Positional arguments follow [`grit fetch`](../fetch/): an optional remote (configured name, URL, or path), then optional refspecs. With `grit pull origin feature`, grit fetches that branch and integrates the tip recorded in `FETCH_HEAD`, even when no remote-tracking ref exists yet — the same model as `git pull origin feature`.

The default remote is `branch.<name>.remote`, or `origin`; every mode (`--ff-only`, `--rebase`, `--autostash`, `--check`) fetches from that remote, so a branch tracking `backup/main` pulls from `backup`. With no refspecs, the upstream is `branch.<name>.merge`, or `refs/remotes/<remote>/<branch>`. The remote's `remote.<name>.fetch` refspecs decide which local ref holds each fetched branch. With `branch.<name>.remote = .` the upstream is a local branch, and `grit pull` merges it without fetching.

On a branch with no commits yet, the branch is created at the upstream commit with an `initial pull` reflog entry.

Like `grit merge`, `grit pull` uses the same scoped local-change rules as [`grit switch`](../switch/) and won't run with a detached HEAD (except to refresh a [snapshot checkout](#in-a-snapshot-checkout)). Conflicts follow the same model as `grit merge` and `grit rebase`: the pull stops with conflict markers and an operation in progress (a merge, or a rebase), and the result tells you how to finish it.

The fetch step uses the same terminal progress as [`grit fetch`](../fetch/) and [`grit clone`](../clone/): a `Pulling <remote> <url>` line, then live stderr progress with an overall bar on a TTY in human mode, silent when stderr is piped or when you pass `--json` or `--markdown`. When the fetch received objects, the result line is followed by the same `time`, `data` and `objects` summary rows as `grit fetch`. When you need an explicit fetch refspec or fetch-only flags (`--depth`, `--deepen`, `--unshallow`, `--tags`, `--no-tags`, `--prune`, `--no-prune`, `--dry-run`, `-q`), run [`grit fetch`](../fetch/) first, then integrate with `grit merge` or the pull integration flags below.

Integration mode follows Git's `pull.ff`, `pull.rebase`, and `branch.<name>.rebase` unless overridden on the command line. With rebase, local commits replay onto the fetched upstream tip via the history engine, exactly as [`grit rebase`](../rebase/) does. When a commit conflicts, the rebase stops (exit **1**) with conflict markers, unmerged paths and Git-compatible `.git/rebase-merge/` state; the branch still points at its old tip until the rebase finishes. Resolve, `grit add`, then `grit rebase --continue`, or run `grit rebase --skip` / `grit rebase --abort` (`git rebase --continue` / `--abort` work on the same state). The rebase only stops when it can do so without losing work: if staged or unstaged changes exist, or an untracked file sits where the stop would check out a path, the pull refuses with `kind: dirty_worktree` (or the overwrite error) and nothing changes. Use `--autostash` or commit first. `pull.rebase=interactive` is rejected. For a standalone rebase without fetching, use [`grit rebase`](rebase.md).

Integration runs the same hooks as `git pull`: `post-merge 0` after a fast-forward or merge commit (in rebase mode too, when the branch is only behind and simply fast-forwards), and `post-checkout <old tip> <upstream> 1` plus `post-rewrite rebase` after a rebase (see [`grit hooks`](../hooks/)).

`--autostash` stashes dirty index/worktree changes (including untracked files) before integrating, then re-applies them afterward. Config `merge.autoStash` / `rebase.autoStash` enables the same behavior by default for merge and rebase pulls respectively. If re-applying the stash conflicts, the pull integration is kept, the stash entry remains on `refs/stash`, and the result is `autostash_conflict` (exit **1**). When a rebase pull stops on conflicts, the stash is not applied over the conflict markers: it stays on `refs/stash` (reported as `autostash`) so you can `grit stash pop` once the rebase is finished or aborted.

### In a snapshot checkout

In a [snapshot checkout](../checkout/), `grit pull` with no refspecs **refreshes** the checkout (so does `grit pull origin`, or a pull naming whatever remote has the checkout's server URL now):

1. It asks the server for a snapshot of the newest commit of the target branch the checkout was made from (the server keeps following that branch even when its `HEAD` moves to another), made on top of the checkout's last snapshot.
2. It gives the current branch's commits made with plain `git` a change id, as [`grit checkin`](../checkin/#change-ids) does, so the rebase below keeps an id the server can match them by (`Gave 1 commit made with git a change id.`; not with `--check`).
3. It fetches that snapshot into `origin/<target>` (`refs/remotes/origin/<target>`) from the server the checkout came from, with an explicit refspec and no tags, whatever `origin` is configured as now. Only what changed in the checkout's paths comes down. The checkout records the new snapshot only once it has arrived and is the one the server described.
4. It brings the current branch onto it: a rebase unless `--ff-only` or `--no-rebase` says otherwise. Commits already merged into the target drop out. Commits of a branch this checkout checked in and [integrated](../integrate/) into the branch it follows are never replayed, since the snapshot already has their result (replaying them one by one could conflict with themselves): a branch with nothing above them moves to the new snapshot, keeping uncommitted and untracked changes as `git reset --keep` does (`landed`), and one with commits of its own above them rebases only those, as `git rebase --onto origin/<target> <integrated tip>` would. When the rebase hits a conflict, it **stops** in a resumable rebase, as [`grit rebase`](../rebase/) does: resolve the files, `grit add` them, then `grit rebase --continue` (or `grit rebase --abort` to put the branch back on the older snapshot). Until then, another `grit pull` refuses to start. When the index or worktree has changes of their own, the rebase can't stop there, so nothing changes; commit or stash them and pull again.
5. A local branch named after the target (a `main` you made with `grit branch main origin/main`) that has no commits of its own follows to the new snapshot, and so does any other branch whose commits have all been integrated. Every other branch with commits of its own that still builds on an older snapshot is named, with how to bring it up: `grit switch <branch>`, then `grit pull` (which leaves out its integrated commits, too).

With `HEAD` detached, the refresh still fetches the new snapshot into `origin/<target>` and leaves `HEAD` where it is. It works the same from any worktree of the checkout.

The first lines say how far the target moved and what changed in the checkout's paths (as `git diff --shortstat` counts it), or that it hasn't moved:

```console
$ grit pull
main moved 4f06b71 → 3182ed4; new snapshot 39dfeba at origin/main
  1 file changed, 1 insertion(+), 1 deletion(-) in this checkout's paths
Rebased onto origin/main (2b87e20)
main follows to 39dfeba
side still builds on an older snapshot; run `grit switch side`, then `grit pull`
```

```console
$ grit pull
main moved e8d5aa5 → f3dbd6f; new snapshot 552fbc6 at origin/main
  1 file changed, 1 insertion(+), 1 deletion(-) in this checkout's paths
CONFLICT (content): src/a.rs
Rebasing onto origin/main stopped on a conflict; the rebase is in progress.
Resolve the conflicts, `grit add` the files, then run `grit rebase --continue`; or run `grit rebase --abort` to put the branch back where it was.
$ grit add src/a.rs && grit rebase --continue
Rebased 1 commit(s)
```

### A server branch in a snapshot checkout

In a snapshot checkout, `grit pull <branch>` (or `grit pull origin <branch>`, naming the checkout's server) brings your `<branch>` onto the server's branch of that name, as your checkout sees it. That is how you pick up a teammate's branch, or work on one with someone whose checkout has other paths. `<branch>` must not be the name of a configured remote (then it is an ordinary pull from that remote); naming the branch the checkout follows (`grit pull main`) is just a refresh.

1. The server **shows you its branch**: each of its commits that the target lacks is rewritten with its tree cut down to your checkout's paths, on top of your newest snapshot under it, at `refs/checkouts/<id>/in/heads/<branch>` (see [`grit serve`](../serve/#snapshot-checkouts)). Author, date and message are kept. A commit that only touched paths you don't have shows up as an empty commit, so the history still lines up with the server's. Your own commits that the server's are grafts of come back as they are, and files outside your paths that they added stay in the commits above them while they are still files there (one the server turned into a directory is dropped). Another checkout's commit is never shown to you as it is when it has files outside your paths, even on the same snapshot. On a server whose file system can't tell names apart by case, a name that differs from the server's branch only in case is that branch, and you get it under the server's spelling (`grit pull feature` creates `Feature`), so its check-ins aren't refused as a case conflict.
2. It **fetches** that into `origin/<branch>` (`refs/remotes/origin/<branch>`) from the server the checkout came from, with no tags, and checks that what arrived is what the server described.
3. It gives the branch's commits made with plain `git` a change id, as `grit checkin` does.
4. It **brings your branch onto it**:
   - a branch you don't have yet is **created** at the server's (`created`; switch to it with `grit switch <branch>`);
   - a branch that has every commit already is left alone (`up_to_date`: `feature already has them.`);
   - one with nothing of its own on top moves to it (`fast_forward`), keeping uncommitted changes when it is the current branch;
   - otherwise **your own commits are rebased onto it** (`rebased`): those above the newer of where your branch meets the server's and the newest snapshot it builds on. Copies of the server's commits that your branch holds (made when a refresh rebased it onto a newer snapshot, say) are left out, recognized by change id or by patch, so only your work moves. A rebase that conflicts **stops** for you to resolve, then `grit rebase --continue` (or `--abort`), as a refresh's does (`rebase_stopped`). When the branch is not the current one, you are switched to it first, as `git rebase <upstream> <branch>` does, and switched back when the rebase can't go ahead (local changes it would overwrite, say). A branch that another worktree has checked out, or is rebasing or bisecting, is refused. A rebase of yours goes through cleanly when the server's branch turned a directory into a file (or the other way round) and your commits never touched it.

Checking the branch in afterwards is a fast-forward on top of the server's: work you build on its commits is grafted onto the real commits they stand for (see [`grit checkin`](../checkin/#sharing-a-branch)). It always rebases, so `--ff-only`, `--no-rebase`, `--autostash` and `--dry-run` are refused; `--check` says what it would do to the branch without changing it (the server still shows the branch, and it is fetched).

```console
$ grit pull feature
The server's feature is at bcd6c48: 2 commits on this checkout's snapshots, at origin/feature (19a3116).
Rebased feature onto origin/feature (4c2d9e1), 1 commit of yours on top.
feature builds on the server's now; `grit checkin feature` sends it.
$ grit pull feature
The server's feature is at bcd6c48: 2 commits on this checkout's snapshots, at origin/feature (19a3116).
feature already has them.
$ grit pull theirs
The server's theirs is at 6f1c2d3: 1 commit on this checkout's snapshots, at origin/theirs (a2b3c4d).
Created theirs at a2b3c4d, the server's; `grit switch theirs` to work on it.
```

The server refuses a branch it doesn't have, one the target already has every commit of, and one that builds on a commit from before your checkout's first snapshot (its author can rebase it onto a newer target first). A local branch by that name that shares nothing with the server's is refused too; rename it to get the server's.

A merge commit a `--no-rebase` refresh makes checks in as a merge whose result keeps the target's changes outside your paths (see [`grit checkin`](../checkin/)). A later rebasing refresh drops the merge commits a `--no-rebase` refresh made and replays your own commits onto the new snapshot.

`--dry-run` asks the server where the target is now and says whether a refresh would move, without the server making a snapshot, and without fetching or changing anything here. `--check` refreshes for real (the server makes the snapshot and it is fetched into `origin/<target>`), then reports what bringing the branch onto it would do, without changing the branch, the index or the worktree.

`--check` fetches (always for real — not a dry run) and reports whether the branch could fast-forward or is already up to date, without changing `HEAD`, the index, the worktree, or reflogs. In rebase mode it also predicts whether the replay would stop on conflicts (`would` is `conflicts`). It cannot be combined with `--autostash` or integration mode overrides (`--ff-only`, `--rebase`, `--no-rebase`).

## Options

| Option | Description |
| --- | --- |
| `<remote>` | Remote to pull from (same rules as [`grit fetch`](../fetch/); defaults to the branch remote or `origin`). In a snapshot checkout, a name that is not a configured remote is a branch: `grit pull <branch>` brings it onto the [server's branch](#a-server-branch-in-a-snapshot-checkout). |
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
| `--rebase` | Rebase local commits onto the upstream tip, overriding `pull.rebase=false`. As with `git pull --rebase`, merge commits on the branch (such as an earlier merge pull) are dropped, and the branch's own commits are replayed one after another on top of the upstream tip, skipping any whose change is already upstream. Conflicts with `--ff-only`, `--no-rebase`, and `--check`. |
| `--no-rebase` | After fetch, merge the upstream with a merge commit when a fast-forward is not possible (the default when `pull.rebase` is not true). Conflicts with `--ff-only`, `--rebase`, and `--check`. |
| `--autostash` | Stash before integrating and re-apply after (see `merge.autoStash` / `rebase.autoStash`). Conflicts with `--check`. |
| `--check` | Fetch and report whether a fast-forward or up-to-date result is possible; exit **0** when yes, **1** when a merge or rebase would be required. Conflicts with `--autostash` and mode overrides. |

All [global options](../global-options/) apply as well.

## Examples

```console
$ grit pull
Fast-forwarded origin/main → 9119bc7

  time      ███·························              8ms
            █ copying 0ms  · finish 5ms
  data      318 B received
  objects   3 · 1 commit · 1 tree · 1 blob

$ grit pull origin feature
Fetched 1 update from origin.
Fast-forwarded origin/feature → d4e5f6a

$ grit pull --check
Would fast-forward (0 ahead, 2 behind). Fast-forward possible.

$ grit pull --autostash
Merged origin/main into the current branch (acd1d4b)
```

When a rebase pull conflicts:

```console
$ grit pull --rebase
CONFLICT (content): f

Rebase stopped on conflicts; fix them and run `grit rebase --continue`, or `grit rebase --abort` / `grit rebase --skip`.
$ grit status
Rebasing main onto 3f6a904
…
$ grit add f
$ grit rebase --continue
Rebased 1 commit(s)
  commit 1e64458 → e79b420 (local)
  refs/heads/main 1e64458 → e79b420
```

When the branch has no upstream yet:

```console
$ grit pull
error: no upstream configured for branch main
hint: pull a branch by name with `grit pull origin main`, or publish this one and track it with `grit push -u origin main`
```

With `--json` that is `{"error":"no upstream configured for branch main","hint":"…","kind":"branch_no_upstream"}`. A missing remote is `kind` `remote_not_found` and an authentication failure `auth_required` or `auth_rejected`, each with a `hint` (see [Exit codes](../exit-codes/)).

## JSON output

Schema: [v2](/schemas/pull/v2.json) · [v1](/schemas/pull/v1.json)

Pass `--json` for stable, scripting-friendly output:

Exit codes (also apply to default human output): **0** on success (`up_to_date`, `fast_forward`, `merged`, `rebased`, `set_upstream`, `detached`, `landed`, or `created`) and on `--check` when `check.fast_forward_possible` is true; **1** when `--ff-only` refuses diverged history (`refused_not_ff`), integration stops with conflicts (`conflicts`, with `operation` `merge` or `rebase` in progress), a snapshot checkout's refresh stops in a rebase (`rebase_stopped`), `--check` when a merge or rebase would be required, or `autostash_conflict`.

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `result` | string | `up_to_date`, `fast_forward`, `merged`, `rebased`, `refused_not_ff`, `conflicts`, `set_upstream`, `autostash_conflict`, `rebase_stopped` (a snapshot checkout's rebase stopped on a conflict and is in progress), `detached` (a snapshot checkout refreshed with `HEAD` detached: only `origin/<target>` moved), `landed` (in a snapshot checkout, every commit of the branch was integrated, so it moved to the new snapshot, keeping uncommitted changes), or `created` (`grit pull <branch>` in a snapshot checkout made the branch at the server's). |
| `branch` | string | Remote branch label that was integrated (or checked). |
| `oid` | string or null | Commit the current branch points at after the pull, when applicable (absent when a rebase stopped). |
| `change_id` | string or null | Stored `change-id` on `oid` when present. |
| `upstream` | string or null | For `set_upstream`, the remote-tracking branch that was adopted. |
| `conflicts` | array of strings or null | Path names when `result` is `conflicts` or `rebase_stopped` (legacy list). |
| `conflict_details` | array or null | Structured conflict items when `result` is `conflicts` or `rebase_stopped`: `path`, `kind` (`content`, `add_add`, `modify_delete`), and for merges a `summary`. |
| `operation` | string or null | When `result` is `conflicts` or `rebase_stopped`: the operation left in progress, `merge` or `rebase`. |
| `commit` | string or null | When `operation` is `rebase`: the local commit that could not be replayed. |
| `next_steps` | array of strings or null | When `result` is `conflicts`: ordered `grit` commands to finish (for a rebase, ending in `grit rebase --continue`). |
| `alternatives` | array of strings or null | When `result` is `conflicts`: other ways out (`grit merge --abort`, or `grit rebase --abort` / `grit rebase --skip`). |
| `autostash` | string or null | Autostash commit kept on `refs/stash` because a rebase stopped or re-applying it conflicted. |
| `ahead` | integer or null | Local-only commits when `result` is `refused_not_ff` or with `--check`. |
| `behind` | integer or null | Upstream-only commits when `result` is `refused_not_ff` or with `--check`. |
| `check` | object or null | Present with `--check`: `ahead`, `behind`, `fast_forward_possible`, and `would` (same result strings as `result`). |
| `refresh` | object or omitted | In a snapshot checkout, the refresh (fields below). |
| `refresh.target` | string | The server's target branch the checkout follows. |
| `refresh.upstream_ref` | string | The local ref holding the newest snapshot, `refs/remotes/origin/<target>`. |
| `refresh.base` | string | The real commit the newest snapshot stands for. With `--dry-run`, where the target is now. |
| `refresh.snapshot` | string | The newest snapshot commit, now at `upstream_ref`. With `--dry-run`, the checkout's snapshot as it is. |
| `refresh.previous_base` | string or null | The real commit the checkout's previous snapshot stood for. |
| `refresh.previous_snapshot` | string or null | The checkout's previous snapshot. |
| `refresh.moved` | boolean | Whether the target moved since the checkout's last snapshot. |
| `refresh.changes` | object or null | What changed in the checkout's paths between the two snapshots: `files`, `insertions`, `deletions`; null when the target didn't move. |
| `refresh.dry_run` | boolean | True with `--dry-run`: nothing was fetched or changed, and the server made no snapshot. |
| `refresh.followed` | array | Local branches with no commits of their own that moved along (the one named after the target, and any whose commits were all integrated): `name`, `from`, `to`. |
| `refresh.behind` | array | Other branches with commits of their own still on an older snapshot: `name`, `tip`. |
| `refresh.gave_change_ids` | integer | Commits made with plain `git` on the current branch that were given a change id before the rebase (the branch was rewritten); `0` when none needed one. |
| `server_branch` | object or omitted | In a snapshot checkout, `grit pull <branch>`: the server's branch and what your branch did (fields below). `branch` is then `origin/<branch>` and `oid` where your branch is now. |
| `server_branch.name` | string | The branch, as the server spells it (which can differ in case from the name you gave, on a server that ignores case). |
| `server_branch.upstream_ref` | string | The local ref holding the server's branch as the checkout sees it, `refs/remotes/origin/<branch>`. |
| `server_branch.tip` | string | Where the server's branch is, on its real history. |
| `server_branch.projected` | string | What the checkout sees of it, now at `upstream_ref`. |
| `server_branch.commits` | integer | The server branch's commits the target lacks, all shown to the checkout. |
| `server_branch.made` | integer | How many of them were made afresh for this checkout. |
| `server_branch.reused` | integer | How many were commits that were there already: checkout commits on the same snapshots that the server's are grafts of (your own among them), and what you were shown before. |
| `server_branch.previous` | string or null | Where your branch was; null when it was `created`. |
| `server_branch.own` | integer | Your commits replayed onto the server's (`rebased`), without the copies of the server's that were left out. |
| `server_branch.switched` | boolean | Whether `HEAD` was moved onto the branch to rebase it. |
| `server_branch.was_on` | string or null | The branch `HEAD` was on before. |
| `server_branch.gave_change_ids` | integer | Commits made with plain `git` on the branch that were given a change id first. |

Example:

```json
{
  "result": "fast_forward",
  "branch": "origin/main",
  "oid": "b52cca65378cf17309f0b1f917c6dc4fdd9256f2"
}
```

A rebase pull that stopped on conflicts:

```json
{
  "result": "conflicts",
  "branch": "origin/main",
  "conflicts": ["f"],
  "conflict_details": [{ "path": "f", "kind": "content" }],
  "operation": "rebase",
  "commit": "1e644589243fb885b773ec4d697b779000495741",
  "next_steps": ["grit diff", "grit add .", "grit rebase --continue"],
  "alternatives": ["grit rebase --abort", "grit rebase --skip"]
}
```

In a snapshot checkout:

```json
{
  "result": "rebased",
  "branch": "origin/main",
  "oid": "2b87e20c6d1f0a9e8b7c6d5e4f3a2b1c0d9e8f7a",
  "refresh": {
    "target": "main",
    "upstream_ref": "refs/remotes/origin/main",
    "base": "3182ed489b694ae64834b5f6648079c4f30c5e64",
    "snapshot": "39dfeba2cfa4b1d0e9f8a7b6c5d4e3f2a1b0c9d8",
    "previous_base": "4f06b71eca1af2057ee312b06eecac8f40c15b00",
    "previous_snapshot": "5c892b5783dbdeb57f7228eb6a13ffb2fa17d832",
    "moved": true,
    "changes": { "files": 1, "insertions": 1, "deletions": 1 },
    "dry_run": false,
    "followed": [
      {
        "name": "main",
        "from": "5c892b5783dbdeb57f7228eb6a13ffb2fa17d832",
        "to": "39dfeba2cfa4b1d0e9f8a7b6c5d4e3f2a1b0c9d8"
      }
    ],
    "behind": [
      { "name": "side", "tip": "8e2d26b0a1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e6" }
    ],
    "gave_change_ids": 0
  }
}
```

`grit pull feature` in a snapshot checkout, from another branch:

```json
{
  "result": "rebased",
  "branch": "origin/feature",
  "oid": "4c2d9e1b7a6f5e4d3c2b1a0f9e8d7c6b5a4f3e2d",
  "server_branch": {
    "name": "feature",
    "upstream_ref": "refs/remotes/origin/feature",
    "tip": "bcd6c48a1f2e3d4c5b6a79881726354a5b6c7d8e",
    "projected": "19a3116f0e9d8c7b6a5f4e3d2c1b0a9f8e7d6c5b",
    "commits": 2,
    "made": 1,
    "reused": 1,
    "previous": "8e2d26b0a1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e6",
    "own": 1,
    "switched": true,
    "was_on": "alice/elm-orchid",
    "gave_change_ids": 1
  }
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



```text
## Pull

- Fast-forwarded **`origin/main`** → `d0f5bdf`
```

In a snapshot checkout, the refresh comes first, and branches it moved or left behind follow the result:

```text
## Pull

- Refresh: **`main`** moved `4f06b71` → `3182ed4` · new snapshot `39dfeba` at `refs/remotes/origin/main` · 1 file changed, 1 insertion(+), 1 deletion(-) in this checkout's paths
- Rebased onto **`origin/main`** · `2b87e20`
- `main` follows to `39dfeba`
- `side` still builds on an older snapshot → `grit switch side`, then `grit pull`
```

`grit pull <branch>` in a snapshot checkout names the server's branch first:

```text
## Pull

- Change ids: gave 1 commit made with git a change id
- Server branch: **`feature`** at `bcd6c48` · 2 commits on this checkout's snapshots · at `refs/remotes/origin/feature` (`19a3116`)
- Rebased **`feature`** onto **`origin/feature`** · `4c2d9e1` · 1 commit of yours on top → `grit checkin feature` sends it
- Now on `feature` (was `alice/elm-orchid`)
```

## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Success (`up_to_date`, `fast_forward`, `merged`, `rebased`, `set_upstream`, `detached`, `landed`, `created`) or `--check` when fast-forward is possible. |
| `1` | Conflicts, refused merge/rebase, a snapshot checkout's rebase stopped in progress (`rebase_stopped`), a refresh refused while a rebase or merge is in progress, `--check` when integration would be required, `autostash_conflict`, or a server branch `grit pull <branch>` can't take (the server has none, or it is already on the target or older than the checkout's first snapshot). |
| `2` | Invalid flags ([`usage`](../exit-codes/)). |
| `128` | `--ff-only` when history diverged ([`refused_not_ff`](../exit-codes/)). |
| `128` | The fetch could not reach or read the remote ([`remote_unreadable`](../exit-codes/)). |

See [Exit codes](../exit-codes/).


## See also

[grit fetch](../fetch/), [grit merge](../merge/), [grit push](../push/)