---
title: grit checkin
summary: Send a snapshot checkout's branches back to the server, rebuilt on its real history.
group: Remotes
order: 8
---

## Synopsis

```text
grit checkin [<branch>...] [-f|--force]
```

## Description

In a [snapshot checkout](../checkout/), your commits sit on a snapshot commit that holds only the checkout's paths. `grit checkin` sends branches back to the server it came from, which **grafts** them onto its real history: it replays each commit's changes onto the full tree of the real commit the snapshot stands for, keeping the commit's author, date and message. On the server, the branch is then an ordinary branch whose parent is the real commit, with the whole tree — files outside your checkout's paths stay as the target branch has them.

With no branches named, every local branch with commits on top of the snapshot is checked in; when there is none, `grit checkin` says there is nothing to check in and succeeds. The branch's commits are first pushed to `refs/checkouts/<id>/out/heads/<branch>` on the server the checkout was made from (a large push uses the [resumable upload](../push/#large-pushes)), then grafted. That push always goes to the URL the checkout was made from, even if `origin` now points elsewhere. A plain `git push` or `grit push` in a checkout pushes to the same place, so it never puts raw snapshot commits on the server's real branches; run `grit checkin` to graft what you pushed. Grafts are deterministic: checking the same commits in again changes nothing.

A grafted commit has a new tree and new parents, so a signature (`gpgsig`) or merged tag (`mergetag`) it carried no longer holds and is dropped; every other header is kept. A merge commit, such as the one [`grit pull --no-rebase`](../pull/) makes, is grafted onto a three-way merge of its parents' grafts: what the target changed outside your paths since your older snapshot is kept, and inside your paths the merge's own result wins. A file that replaces a directory (or the other way round) checks in like any other change.

Check-ins need write access to the repository.

### Change ids

Commits made with `grit commit` carry a `change-id` header, and `grit rebase` keeps it, so the server can tell a rebased copy of one of its commits from new work. Commits made with plain `git` (by an editor, an agent or a script) have none, so before anything is pushed, `grit checkin` gives every commit on the branches it checks in, above the snapshot, that lacks one the id derived from its own commit id: the id `grit log` already shows for it. The branch is rewritten with it (commits on top move to the new parents and keep their ids), and the output says so (`· gave 1 commit made with git a change id`). Only the header is added (a signature, which no longer holds, is dropped), so trees, the index and the worktree stay as they are. Nothing is rewritten under the snapshot, in remote-tracking refs, in commits that were already integrated, or when `grit.changeIds` is `false`, and nothing at all while a rebase, merge, cherry-pick or revert (one paused between its commits too) is in progress in any of the checkout's worktrees; the check-in then sends the commits as they are. The server gives a commit that reaches it without an id the same derived id when it grafts it. Plain `git rebase` drops the header, so when you share a branch, rebase it with `grit rebase`, or bring it up to date with [`grit pull <branch>`](../pull/#a-server-branch-in-a-snapshot-checkout).

### Refused branches

A branch is refused when:

- **it is the target branch**, in any case (`Main` too): the branch the checkout was made from, or the branch the server's `HEAD` names now. Only [`grit integrate`](../integrate/) and [`grit slice`](../slice/) change the target (the server refuses plain pushes to it too); check your work in on another branch.
- **a commit changes a path outside your checkout's paths that the target holds differently.** A refresh never brings such a path into your checkout, so make that change from a checkout whose paths include it. The same goes for a directory replaced by a file when the target has files there your checkout can't see.
- **a merge commit's parents conflict outside your paths**, where your checkout can't resolve them.
- **the server's branch has commits yours doesn't.** Rebase onto the server's branch with `grit pull <branch>` and check in again, or pass `--force` to replace it.
- **its name differs only in case from a branch the server has**, on a server whose file system can't tell them apart.
- **a commit builds on none of your checkout's snapshots**, such as one made on the server's own history fetched with plain Git (`not_on_snapshot`). Take a branch of the server's with `grit pull <branch>` and build on that, or put your commits on the snapshot: `grit switch -c <new branch> origin/<target>`, then `grit pick <commits>`.

### When a branch moves

A branch the server already has moves when one of these holds, checked in this order, and the output names the rule (`rule` in `--json`):

- **fast-forward** (`fast_forward`): your branch has the server's branch in its history;
- **this checkout set it last** (`own_last_set`), so you are replacing your own work after an amend or rebase;
- **it keeps every commit the server's branch had** (`keeps_every_commit`), matched by change id: every commit of the server's branch that the target doesn't have is in yours, as a copy with the same id, perhaps rebased and with more on top. Say a teammate rebased the shared branch onto a newer `main` with `grit rebase` and force-pushed it, and you then add a commit to your copy: your check-in has every commit theirs had, so it goes through without `--force`. A copy only counts when it also has what the server's commit changed outside your checkout's paths, which you can't see: if the teammate amended the commit to change `docs/` as well and your checkout is of `src`, your copy would put `docs/` back, so the branch is refused as diverged instead;
- **`--force`** (`forced`), when none of the others holds.

### Sharing a branch

Checkouts of different paths can work on one branch. The server keeps a **graft log** of which real commit each checkout commit stands for: every check-in adds the commits it grafts, and so does [`grit pull <branch>`](../pull/#a-server-branch-in-a-snapshot-checkout), which shows you the server's branch cut down to your paths. A commit you build on one of those grafts onto the real commit it stands for, so files outside your paths keep the other checkout's changes. Say Alice has `builtin/` and `Documentation/`, and Bob has `Documentation/` and `t/`:

1. Alice commits to `builtin/` and `Documentation/` on `blame-docs` and runs `grit checkin blame-docs`.
2. Bob runs `grit pull blame-docs`. He sees her `Documentation/` change; her `builtin/` change is on the server but not in his files.
3. Bob commits to `Documentation/` and `t/` on top and runs `grit checkin blame-docs`: a fast-forward, his commit on top of her real one, so `builtin/` keeps her change.
4. Alice runs `grit pull blame-docs` and gets his `Documentation/` change on top of her own commit, which comes back as it is. His `t/` change stays out of her files.

When someone else checks in to a branch you have too, your next check-in of it is refused as diverged; `grit pull <branch>` rebases your commits onto theirs, and the check-in after that is a fast-forward. A branch you pulled and haven't added to is left as it is (`unchanged`) when the server's has moved on since, so a plain `grit checkin` of your other work still succeeds (`· theirs has nothing new: the server's is ahead, at e32980a`); only `--force` would take the server's back to yours. The log also means checking a branch in again replays only its new commits. A check-in whose grafts the server can't record (say its disk is full) still sets the branch; the server logs the failure, and later grafts of those commits walk the history again.

When several checkouts check in the same branch at once, they take turns: the first sets it, and the others are refused because the server's branch now has commits they don't. On a server whose repository keeps reflogs (Git's default when it has a worktree), every branch a check-in sets gets a `checkin: from checkout <id>` reflog entry.

## Options

| Option | Description |
| --- | --- |
| `<branch>...` | Branches to check in. Defaults to every branch with commits on top of the snapshot. |
| `-f`, `--force` | Replace a server branch even where that drops commits it has. |

## Examples

```console
$ echo a2 > src/a.rs && grit add src/a.rs && grit commit -m "Change a"
$ grit checkin
✓ alice/elm-orchid created on the server at 7fb21e5 (1 commit)
$ grit checkin
· alice/elm-orchid already at 7fb21e5 on the server
```

A commit made with plain `git` is given a change id first; a teammate's rebase of the shared branch is replaced without `--force` when your branch still has all its commits:

```console
$ echo a3 > src/a.rs && git commit -qam "Change a again"
$ grit checkin
· gave 1 commit made with git a change id
✓ alice/elm-orchid updated on the server 7fb21e5 → 2c41d0e (1 commit; fast-forward)
$ grit checkin shared
✓ shared updated on the server 9a4b911 → c41d0e2 (2 commits; rebased, keeps every commit the server's branch had)
```

```console
$ grit checkout http://127.0.0.1:8080/demo.git -p src --into fresh && cd fresh
$ grit checkin
· nothing to check in: no branch has commits on top of the snapshot
```

## JSON output

Schema: [v1](/schemas/checkin/v1.json)

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `url` | string | The server repository. |
| `gave_change_ids` | integer | Commits made with plain `git` that were given a change id before the check-in (their branches were rewritten); `0` when none needed one. |
| `branches` | array | One entry per branch (empty when there was nothing to check in): `name`, `status` (`created`, `updated`, `unchanged` or `refused`; `unchanged` when the server's branch was at the graft already or ahead of it), `grafted` (the server commit the branch became), `previous` (where the server's branch was), `commits` (commits replayed), `rule` for an updated branch, and for a refused branch `message` (why, in words) and `reason`. |
| `branches[].rule` | string or omitted | What let an `updated` branch move: `fast_forward`, `own_last_set` (this checkout set it last), `keeps_every_commit` (a rebase that keeps every commit the server's branch had, by change id) or `forced`. |
| `branches[].reason` | string | The kind of refusal: `target_branch`, `conflict` (a path the target holds differently), `merge_conflict`, `diverged` (the server's branch has commits yours doesn't), `case_conflict`, `not_on_snapshot`, `not_pushed`, `invalid_name` or `invalid_tip`. |
| `refused` | boolean | Whether any branch was refused; the exit status is then `1`. |

```json
{
  "url": "http://127.0.0.1:8080/demo.git",
  "gave_change_ids": 0,
  "branches": [
    {
      "name": "alice/elm-orchid",
      "status": "created",
      "grafted": "7fb21e5a0c1e4f7d9b2e6a3c8d0f1e2a3b4c5d6e",
      "commits": 1
    }
  ],
  "refused": false
}
```

An update allowed by the change-id rule, after a commit made with `git` was given an id:

```json
{
  "url": "http://127.0.0.1:8080/demo.git",
  "gave_change_ids": 1,
  "branches": [
    {
      "name": "shared",
      "status": "updated",
      "grafted": "c41d0e2b9a8f7e6d5c4b3a2f1e0d9c8b7a6f5e4d",
      "previous": "9a4b9113f0c1e4f7d9b2e6a3c8d0f1e2a3b4c5d6",
      "commits": 2,
      "rule": "keeps_every_commit"
    }
  ],
  "refused": false
}
```

A refused branch, and a check-in with nothing to send:

```json
{
  "url": "http://127.0.0.1:8080/demo.git",
  "gave_change_ids": 0,
  "branches": [
    {
      "name": "shared",
      "status": "refused",
      "previous": "9a4b9113f0c1e4f7d9b2e6a3c8d0f1e2a3b4c5d6",
      "commits": 0,
      "message": "the server's shared is at 9a4b9113f0c1, which has commits this checkout doesn't; rebase onto it with `grit pull shared` and check in again, or check in with --force to replace it",
      "reason": "diverged"
    }
  ],
  "refused": true
}
```

```json
{
  "url": "http://127.0.0.1:8080/demo.git",
  "gave_change_ids": 0,
  "branches": [],
  "refused": false
}
```

## Markdown output

Pass [`--markdown`](../global-options/) for agent-friendly output.

Schema: [v1](/schemas/checkin/v1.json)

```markdown
## Check-in to `http://127.0.0.1:8080/demo.git`

- Change ids: gave 1 commit made with git a change id

| Branch | Status | Server commit | Commits | Rule |
| --- | --- | --- | ---: | --- |
| `alice/elm-orchid` | created | `7fb21e5a0c1e4f7d9b2e6a3c8d0f1e2a3b4c5d6e` | 1 |  |
| `shared` | updated | `c41d0e2b9a8f7e6d5c4b3a2f1e0d9c8b7a6f5e4d` | 2 | rebased, keeps every commit the server's branch had |
```

## Exit status

| Code | Meaning |
| --- | --- |
| `0` | Every branch was checked in (or was already there), or there was nothing to check in. |
| `1` | A branch was refused, or this is not a snapshot checkout. |

## See also

- [`grit checkout`](../checkout/), [`grit integrate`](../integrate/), [`grit pull`](../pull/).
