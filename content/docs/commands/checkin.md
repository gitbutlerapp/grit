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

A branch is refused when:

- **it is the target branch**, in any case (`Main` too): the branch the checkout was made from, or the branch the server's `HEAD` names now. Only [`grit integrate`](../integrate/) and [`grit slice`](../slice/) change the target (the server refuses plain pushes to it too); check your work in on another branch.
- **a commit changes a path outside your checkout's paths that the target holds differently.** A refresh never brings such a path into your checkout, so make that change from a checkout whose paths include it. The same goes for a directory replaced by a file when the target has files there your checkout can't see.
- **a merge commit's parents conflict outside your paths**, where your checkout can't resolve them.
- **the server's branch has commits yours doesn't.** Rebase onto the server's branch with `grit pull <branch>` and check in again, or pass `--force` to replace it. A branch this checkout set last may always be replaced (after an amend or rebase).
- **its name differs only in case from a branch the server has**, on a server whose file system can't tell them apart.

When several checkouts check in the same branch at once, they take turns: the first sets it, and the others are refused because the server's branch now has commits they don't. On a server whose repository keeps reflogs (Git's default when it has a worktree), every branch a check-in sets gets a `checkin: from checkout <id>` reflog entry.

Check-ins need write access to the repository.

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
| `branches` | array | One entry per branch (empty when there was nothing to check in): `name`, `status` (`created`, `updated`, `unchanged` or `refused`), `grafted` (the server commit the branch became), `previous` (where the server's branch was), `commits` (commits replayed), and for a refused branch `message` (why, in words) and `reason`. |
| `branches[].reason` | string | The kind of refusal: `target_branch`, `conflict` (a path the target holds differently), `merge_conflict`, `diverged` (the server's branch has commits yours doesn't), `case_conflict`, `not_on_snapshot`, `not_pushed`, `invalid_name` or `invalid_tip`. |
| `refused` | boolean | Whether any branch was refused; the exit status is then `1`. |

```json
{
  "url": "http://127.0.0.1:8080/demo.git",
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

A refused branch, and a check-in with nothing to send:

```json
{
  "url": "http://127.0.0.1:8080/demo.git",
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
  "branches": [],
  "refused": false
}
```

## Markdown output

Pass [`--markdown`](../global-options/) for agent-friendly output.

Schema: [v1](/schemas/checkin/v1.json)

```markdown
## Check-in to `http://127.0.0.1:8080/demo.git`

| Branch | Status | Server commit | Commits |
| --- | --- | --- | ---: |
| `alice/elm-orchid` | created | `7fb21e5a0c1e4f7d9b2e6a3c8d0f1e2a3b4c5d6e` | 1 |
```

## Exit status

| Code | Meaning |
| --- | --- |
| `0` | Every branch was checked in (or was already there), or there was nothing to check in. |
| `1` | A branch was refused, or this is not a snapshot checkout. |

## See also

- [`grit checkout`](../checkout/), [`grit integrate`](../integrate/), [`grit pull`](../pull/).
