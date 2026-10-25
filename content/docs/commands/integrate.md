---
title: grit integrate
summary: Merge a branch into the server's target branch from a snapshot checkout, then refresh.
group: Remotes
order: 9
---

## Synopsis

```text
grit integrate <branch>
```

## Description

In a [snapshot checkout](../checkout/), the target branch only changes on the server. `grit integrate <branch>`:

1. **checks the branch in** first when you have it here (see [`grit checkin`](../checkin/));
2. **merges it into the target branch on the server**, with a merge commit by you, when it merges without conflicts. The merge is by the author your own commits get (`GIT_AUTHOR_NAME` and `GIT_AUTHOR_EMAIL`, then `author.*` and `user.*` config, as [`grit commit`](../commit/) has it), or the signed-in user when none is set; the server cleans `<`, `>` and line breaks out of the name and email. A branch that conflicts is not merged: the error names the files; refresh, rebase and try again. A branch the target already has is left alone;
3. **refreshes this checkout** onto the new target, as [`grit pull`](../pull/#in-a-snapshot-checkout) does: the current branch is rebased onto the new snapshot, and commits now in the target drop out of it. A local branch named after the target with no commits of its own follows; other branches still on an older snapshot are named, with how to bring each up to date.

The branch can also be one somebody else checked in or pushed: a plain push to the target itself is refused (see [`grit serve`](../serve/#snapshot-checkouts)), so push to another branch and integrate that. Integrating needs write access. Where the server's repository keeps reflogs, the move of the target is logged as `integrate: Merge branch '<branch>' into <target>`, so `git reflog <target>` on the server shows it.

These are refused, and the target is left alone:

- **the target branch itself** (in any case): name the branch to merge into it;
- **a name that is not a valid branch name**, such as one with `..` in it;
- **a branch the server doesn't have**: check it in first;
- **a branch that shares no history with the target**, such as an orphan branch, or raw checkout commits pushed straight to a branch (they sit on a snapshot commit rather than on the real history): check them in with [`grit checkin`](../checkin/) instead;
- **a merge that would overwrite uncommitted changes in the server's worktree**, when the server's repository has the target checked out: the server's worktree moves with each integration (keeping uncommitted changes there, as `git reset --keep` does), so commit or stash those changes on the server first.

Integrations, and [slice](../slice/) changes, take turns on the server: several at once all merge, each onto the target the one before left. Each is merged again if the target moves while it is made, so none is lost or refused for coming second.

## Options

| Option | Description |
| --- | --- |
| `<branch>` | The branch to merge into the target. |

## Examples

```console
$ grit integrate alice/elm-orchid
· alice/elm-orchid already at 7fb21e5 on the server
✓ Merged alice/elm-orchid into main on the server: d08f98a → 7f51f16 (1 commit)
main moved d08f98a → 7f51f16; new snapshot 2b12f15 at origin/main
  2 files changed, 2 insertions(+), 2 deletions(-) in this checkout's paths
Rebased onto origin/main (2b12f15)
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
| `refresh` | object or omitted | The refresh done after (see [`grit pull`](../pull/)'s JSON). |

```json
{
  "name": "alice/elm-orchid",
  "target": "main",
  "status": "merged",
  "previous": "d08f98a93fcf6c0bd1f4d7e1c5a0b9e8f7d6c5b4",
  "head": "7f51f16b2cb0e4d3c2b1a0f9e8d7c6b5a4f3e2d1",
  "commits": 1
}
```

## Markdown output

Pass [`--markdown`](../global-options/) for agent-friendly output.

Schema: [v1](/schemas/integrate/v1.json)

The check-in done first, and the refresh done after, follow in their own sections, as [`grit checkin`](../checkin/) and [`grit pull`](../pull/) render them:

```markdown
## Integrate `alice/elm-orchid` into `main`

- **status**: merged
- **previous**: `d08f98a93fcf6c0bd1f4d7e1c5a0b9e8f7d6c5b4`
- **head**: `7f51f16b2cb0e4d3c2b1a0f9e8d7c6b5a4f3e2d1`
- **commits**: 1

## Pull

- Refresh: **`main`** moved `d08f98a` → `7f51f16` · new snapshot `2b12f15` at `refs/remotes/origin/main` · 2 files changed, 2 insertions(+), 2 deletions(-) in this checkout's paths
- Rebased onto **`origin/main`** · `2b12f15`
```

## Exit status

| Code | Meaning |
| --- | --- |
| `0` | The branch was merged, or the target already had it. |
| `1` | The branch conflicts with the target, shares no history with it, is the target, is not a valid branch name, could not be checked in, or does not exist on the server; uncommitted changes in the server's worktree block the merge; or this is not a snapshot checkout. |

## See also

- [`grit checkin`](../checkin/), [`grit pull`](../pull/), [`grit checkout`](../checkout/).
