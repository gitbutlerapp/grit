---
title: grit integrate
summary: Merge a branch into the server's target branch, from a snapshot checkout or a clone of a hosted project.
group: Remotes
order: 9
---

## Synopsis

```text
grit integrate <branch> [--without <user>...]
```

## Description

In a [snapshot checkout](../checkout/), the target branch only changes on the server. `grit integrate <branch>`:

1. **checks the branch in** first when you have it here, after giving its commits made with plain `git` a change id (see [`grit checkin`](../checkin/#change-ids));
2. **merges it into the target branch on the server**, with a merge commit by you, when it merges without conflicts. The merge is by the author your own commits get (`GIT_AUTHOR_NAME` and `GIT_AUTHOR_EMAIL`, then `author.*` and `user.*` config, as [`grit commit`](../commit/) has it), or the signed-in user when none is set; the server cleans `<`, `>` and line breaks out of the name and email. A branch that conflicts is not merged: the error names the files and tells you to run [`grit pull`](../pull/) on the checkout, then integrate again. A branch the target already has is left alone;
3. **refreshes this checkout** onto the new target, as [`grit pull`](../pull/#in-a-snapshot-checkout) does. The branch's commits are in the new snapshot now, but not as its ancestors (a snapshot is one commit standing for the target's tree), so they are never replayed: replaying them one by one could conflict with themselves, as three commits editing one line would against a snapshot that has the last edit. Instead:
   - **when you are on the branch**, it moves to the new snapshot, keeping uncommitted and untracked changes as `git reset --keep` does (`refresh.result` is `landed`);
   - **when you are on a branch built on it**, only that branch's own commits are rebased onto the new snapshot, as `git rebase --onto origin/<target> <integrated tip>` would;
   - **other branches**: one whose commits were all integrated follows to the new snapshot, like a local branch named after the target with no commits of its own; ones with work of their own still on an older snapshot are named, with how to bring each up (`grit switch <branch>`, then `grit pull`, which leaves the integrated commits out too). With `HEAD` detached, only `origin/<target>` moves.

   The checkout remembers the integrated tip (in its marker) until no branch, and no worktree's detached `HEAD`, builds on it any more, so later refreshes leave those commits out as well, in any of the checkout's worktrees;
4. **moves you to a new branch** when you were on the branch it merged: the server reserves a fresh work branch name for the checkout (from your `user.email`, numbered past names it has given out, as [`grit checkout`](../checkout/) does), and the merged branch is renamed to it. The merged branch is gone here; it stays on the server. New commits then check in as a new branch instead of adding to one the target already has.

The server merges into its current target, the branch its `HEAD` names, while a checkout follows the branch it was made from. When the two differ (the server's `HEAD` was moved to another branch since), the merge is not in this checkout's snapshots: the refresh in step 3 treats the branch like any other, nothing is remembered, step 4 is skipped, and the branch keeps its commits here, so refreshes rebase them onto newer snapshots as usual. The output says which branch the checkout follows (`follows`).

The branch can also be one somebody else checked in or pushed: a plain push to the target itself is refused (see [`grit serve`](../serve/#snapshot-checkouts)), so push to another branch and integrate that. Integrating needs write access. Where the server's repository keeps reflogs, the move of the target is logged as `integrate: Merge branch '<branch>' into <target>`, so `git reflog <target>` on the server shows it.

These are refused, and the target is left alone:

- **the target branch itself** (in any case): name the branch to merge into it;
- **a name that is not a valid branch name**, such as one with `..` in it;
- **a branch the server doesn't have**: check it in first;
- **a branch that shares no history with the target**, such as an orphan branch, or raw checkout commits pushed straight to a branch (they sit on a snapshot commit rather than on the real history): check them in with [`grit checkin`](../checkin/) instead;
- **a merge that would overwrite uncommitted changes in the server's worktree**, when the server's repository has the target checked out: the server's worktree moves with each integration (keeping uncommitted changes there, as `git reset --keep` does), so commit or stash those changes on the server first.

Integrations, and [slice](../slice/) changes, take turns on the server: several at once all merge, each onto the target the one before left. Each is merged again if the target moves while it is made, so none is lost or refused for coming second.

### In a clone of a hosted project

In a clone (not a snapshot checkout) of a project on a grit hosting server such as [GitButler Code](https://code.gitbutler.com), `grit integrate <branch>`:

1. **pushes the branch to your space** (`<you>/<project>`), as [`grit sync`](../sync/) would, setting the clone up for it first when it isn't yet;
2. **asks the server to merge it into the target**. For your copy of an organization's project, that is the organization's target branch (usually `main`): the server opens a pull request from the branch, or uses the open one, and integrates it at once. Only members of the organization may; anyone else is told to propose the branch instead, with the link. For a project of your own, it is the project's own target branch. A branch that conflicts is refused with the conflicting files, and nothing changes;
3. **fetches `origin` and fast-forwards the target branch here** to the new tip (`local`).

The branch you are on stays as it is; once merged, [`grit sync`](../sync/) stops pushing it.

**A [shared](../share/) branch** merges your head of it. Every other member's head has to be in yours first: when one has changes yours doesn't (matched by change id), nothing is merged, and the refusal names each such member and their commits. Take them with [`grit sync`](../sync/) and integrate again, or pass `--without <user>` to merge without them; their changes stay in their copy, as a branch nobody shares. Once merged, the share is over: the branch is deleted from every member's copy (except the heads of members left out), every member is told, and `unshared` lists them. The server remembers what was merged for 14 days, so a later push of the branch's name that brings those commits is reported as `pushed_to_integrated` (see [`grit share`](../share/#after-integrating)).

## Options

| Option | Description |
| --- | --- |
| `<branch>` | The branch to merge into the target. |
| `--without <user>` | For a shared branch: merge without waiting for this member's changes, which stay in their copy. Repeatable. |

## Examples

Integrating the branch you are on, with an untracked file you haven't committed:

```console
$ grit integrate alice/elm-orchid
✓ alice/elm-orchid created on the server at 538e7ad (2 commits)
✓ Merged alice/elm-orchid into main on the server: d08f98a → 7f51f16 (2 commits)
main moved d08f98a → 7f51f16; new snapshot 2b12f15 at origin/main
  1 file changed, 1 insertion(+), 1 deletion(-) in this checkout's paths
Moved to origin/main (2b12f15), which has this branch's commits; uncommitted changes are kept.
alice/elm-orchid is in main now, so it's gone here. You're on alice/maple-elm, a new branch.
$ grit status --short
?? src/draft.rs
```

The merge is reported even when the refresh after it fails, here because of an uncommitted change to a file the target changed too. The exit status is then `1`:

```console
$ grit integrate alice/maple-elm
✓ alice/maple-elm created on the server at 1c3e9a0 (1 commit)
✓ Merged alice/maple-elm into main on the server: 7f51f16 → 9a0b1c2 (1 commit)
✗ Refreshing this checkout failed: local changes would be overwritten: src/deep/b.rs
  The merge stands on the server; fix that, then run `grit integrate alice/maple-elm` again to finish.
```

Running `grit integrate` again then finds the target already has the branch (`already_integrated`), refreshes, and moves you to a new branch. That also finishes an integration whose refresh worked but whose move to a new branch failed (the server could not reserve a name, say): the branch is at the new snapshot with nothing of its own, so it is renamed. A refresh whose rebase stops on a conflict reports the conflict as [`grit pull`](../pull/) does, and also exits `1`.

When the server's `HEAD` names another branch than the one this checkout follows:

```console
$ grit integrate alice/elm-orchid
✓ alice/elm-orchid created on the server at 538e7ad (2 commits)
✓ Merged alice/elm-orchid into develop on the server: d08f98a → 7f51f16 (2 commits)
· This checkout follows main, not develop, so alice/elm-orchid keeps its commits here.
main has not moved since this checkout's last snapshot (still at d08f98a).
Already up to date.
```

## JSON output

Schema: [v1](/schemas/integrate/v1.json)

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `name` | string | The branch merged. |
| `target` | string | The server's target branch. |
| `status` | string | `merged` or `already_integrated`. |
| `previous` | string | Where the target was. |
| `head` | string | Where it is now. |
| `commits` | integer | The branch's commits the merge brought in. |
| `checkin` | object or omitted | The check-in done first (see [`grit checkin`](../checkin/)'s JSON). |
| `refresh` | object or omitted | The refresh done after (see [`grit pull`](../pull/)'s JSON); `refresh.result` is `landed` when the branch you were on moved to the new snapshot. Omitted when the refresh failed. |
| `moved_to` | string or omitted | The new work branch you are on, when you were on the branch merged (it is gone here and stays on the server). |
| `follows` | string or omitted | The branch this checkout follows, when the server merged into another one (`target`, the branch its `HEAD` names now): the merge is not in this checkout's snapshots, so the branch keeps its commits here and you stay on it. |
| `refresh_error` | string or omitted | Why refreshing this checkout after the merge, or moving you to a new branch, failed. The merge stands; the exit status is `1`. |
| `project` | string or omitted | In a clone: the project merged into, `<owner>/<project>` (the organization's, for your copy of one). |
| `pull` | integer or omitted | In a clone: the pull request the merge went through, for a copy of an organization's project. |
| `url` | string or omitted | In a clone: the pull request's web page. |
| `local` | object or omitted | In a clone: the target branch here after the merge, as [`grit sync`](../sync/)'s `target` reports it (`name`, `action`, `tip`). |
| `unshared` | array of strings or omitted | For a [shared](../share/) branch: the other members. Integrating ended the share and deleted the branch from their copies. |

```json
{
  "name": "alice/elm-orchid",
  "target": "main",
  "status": "merged",
  "previous": "d08f98a93fcf6c0bd1f4d7e1c5a0b9e8f7d6c5b4",
  "head": "7f51f16b2cb0e4d3c2b1a0f9e8d7c6b5a4f3e2d1",
  "commits": 2,
  "refresh": {
    "result": "landed",
    "branch": "origin/main",
    "oid": "2b12f15c0d9e8f7a6b5c4d3e2f1a0b9c8d7e6f5a",
    "refresh": {
      "target": "main",
      "upstream_ref": "refs/remotes/origin/main",
      "base": "7f51f16b2cb0e4d3c2b1a0f9e8d7c6b5a4f3e2d1",
      "snapshot": "2b12f15c0d9e8f7a6b5c4d3e2f1a0b9c8d7e6f5a",
      "previous_base": "d08f98a93fcf6c0bd1f4d7e1c5a0b9e8f7d6c5b4",
      "previous_snapshot": "5c892b5783dbdeb57f7228eb6a13ffb2fa17d832",
      "moved": true,
      "changes": { "files": 1, "insertions": 1, "deletions": 1 },
      "dry_run": false,
      "followed": [],
      "behind": [],
      "gave_change_ids": 0
    }
  },
  "moved_to": "alice/maple-elm"
}
```

When the refresh fails, `refresh` is omitted and `refresh_error` says why:

```json
{
  "name": "alice/maple-elm",
  "target": "main",
  "status": "merged",
  "previous": "7f51f16b2cb0e4d3c2b1a0f9e8d7c6b5a4f3e2d1",
  "head": "9a0b1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e6f7a8b",
  "commits": 1,
  "refresh_error": "local changes would be overwritten: src/deep/b.rs"
}
```

## Markdown output

Pass [`--markdown`](../global-options/) for agent-friendly output.

Schema: [v1](/schemas/integrate/v1.json)

The check-in done first, and the refresh done after, have their own sections, as [`grit checkin`](../checkin/) and [`grit pull`](../pull/) render them; a move to a new branch, a merge into another branch than the one this checkout follows, and a failed refresh are listed with the merge:

```markdown
## Check-in to `http://127.0.0.1:8080/demo.git`

| Branch | Status | Server commit | Commits | Rule |
| --- | --- | --- | ---: | --- |
| `alice/elm-orchid` | created | `538e7ad0c3b2a1f0e9d8c7b6a5f4e3d2c1b0a9f8` | 2 |  |

## Integrate `alice/elm-orchid` into `main`

- **status**: merged
- **previous**: `d08f98a93fcf6c0bd1f4d7e1c5a0b9e8f7d6c5b4`
- **head**: `7f51f16b2cb0e4d3c2b1a0f9e8d7c6b5a4f3e2d1`
- **commits**: 2
- **moved to**: `alice/maple-elm`, a new branch (`alice/elm-orchid` is in `main` now, so it's gone here)

## Pull

- Refresh: **`main`** moved `d08f98a` → `7f51f16` · new snapshot `2b12f15` at `refs/remotes/origin/main` · 1 file changed, 1 insertion(+), 1 deletion(-) in this checkout's paths
- Moved to **`origin/main`** · `2b12f15`, which has this branch's commits; uncommitted changes kept
```

A merge into another branch than the one this checkout follows:

```markdown
- **follows**: `main` (the merge went into `develop`, so `alice/elm-orchid` keeps its commits here)
```

A failed refresh:

```markdown
- **refresh failed**: local changes would be overwritten: src/deep/b.rs → the merge stands on the server; fix that, then `grit integrate alice/maple-elm` again to finish
```

## Exit status

| Code | Meaning |
| --- | --- |
| `0` | The branch was merged, or the target already had it, and the checkout was refreshed. |
| `1` | The branch was merged (or the target had it), but the refresh after failed (`refresh_error`) or stopped (`refresh.result` is `rebase_stopped`, `conflicts`, `refused_not_ff` or `autostash_conflict`): the output says so and what to do. Or nothing was merged: the branch conflicts with the target, shares no history with it, is the target, is not a valid branch name, could not be checked in, or does not exist on the server; uncommitted changes in the server's worktree block the merge; in a clone, you aren't a member of the organization, the branch couldn't be pushed to your space, or it is a shared branch another member's head has changes of that yours lacks (pass `--without <user>` to merge anyway); or this is neither a snapshot checkout nor a clone of a hosted project. |

## See also

- [`grit checkin`](../checkin/), [`grit pull`](../pull/), [`grit checkout`](../checkout/).
