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

With no branches named, every local branch with commits on top of the snapshot is checked in. The branch's commits are first pushed to `refs/checkouts/<id>/out/heads/<branch>` on the server (a large push uses the [resumable upload](../push/#large-pushes)), then grafted. Grafts are deterministic: checking the same commits in again changes nothing.

A branch is refused when:

- **it is the target branch.** Only [`grit integrate`](../integrate/) and [`grit slice`](../slice/) change the target; check your work in on another branch.
- **a commit changes a path the target holds differently** from what the commit started from. Refresh with [`grit pull`](../pull/) and check in again.
- **the server's branch has commits yours doesn't.** Run `grit pull` to rebase, or pass `--force` to replace it. A branch this checkout set last may always be replaced (after an amend or rebase).

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
```

## JSON output

Schema: [v1](/schemas/checkin/v1.json)

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `url` | string | The server repository. |
| `branches` | array | One entry per branch: `name`, `status` (`created`, `updated`, `unchanged` or `refused`), `grafted` (the server commit the branch became), `previous` (where the server's branch was), `commits` (commits replayed) and `message` (why it was refused). |
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
| `0` | Every branch was checked in (or was already there). |
| `1` | A branch was refused, there was nothing to check in, or this is not a snapshot checkout. |

## See also

- [`grit checkout`](../checkout/), [`grit integrate`](../integrate/), [`grit pull`](../pull/).
