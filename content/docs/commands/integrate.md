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
2. **merges it into the target branch on the server**, with a merge commit by you (`user.name` and `user.email`, or the signed-in user), when it merges without conflicts. A branch that conflicts is not merged: the error names the files; refresh, rebase and try again. A branch the target already has is left alone;
3. **refreshes this checkout** onto the new target, as [`grit pull`](../pull/) does. Commits now in the target drop out of your branches.

The branch can also be one somebody else checked in or pushed. Integrating needs write access.

## Options

| Option | Description |
| --- | --- |
| `<branch>` | The branch to merge into the target. |

## Examples

```console
$ grit integrate alice/elm-orchid
· alice/elm-orchid already at 7fb21e5 on the server
✓ Merged alice/elm-orchid into main on the server: d08f98a → 7f51f16 (1 commit)
Refreshed onto main at 7f51f16 (snapshot 2b12f15)
Rebased onto origin (FETCH_HEAD) (2b12f15)
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

```markdown
## Integrate `alice/elm-orchid` into `main`

- **status**: merged
- **previous**: `d08f98a93fcf6c0bd1f4d7e1c5a0b9e8f7d6c5b4`
- **head**: `7f51f16b2cb0e4d3c2b1a0f9e8d7c6b5a4f3e2d1`
- **commits**: 1
```

## Exit status

| Code | Meaning |
| --- | --- |
| `0` | The branch was merged, or the target already had it. |
| `1` | The branch conflicts with the target, could not be checked in, does not exist on the server, or this is not a snapshot checkout. |

## See also

- [`grit checkin`](../checkin/), [`grit pull`](../pull/), [`grit checkout`](../checkout/).
