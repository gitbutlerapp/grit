---
title: grit share
summary: Work on a branch together with other users of a grit hosting server, each with their own head of it.
group: Remotes
order: 11
---

## Synopsis

```text
grit share add <branch> [<user>...] [--link [--uses <n>|unlimited] [--expires <days>]]
grit share list [<branch>]
grit share accept <link>
grit share remove <branch> [<user>...]
grit share leave <branch>
grit share revoke <branch> <link-id>
```

## Description

On a grit hosting server such as [grithub](https://grithub.dev), your branches are private to your space. `grit share` lets other people work on one of them with you. A shared branch is in every member's space, and **each member has their own head of it**: only you move yours. Nobody's push can overwrite anyone else's work, so there is nothing to force, and every head anyone pushes is kept in the share's log on the server.

The other members' heads are fetched beside yours as `<remote>/shared/<branch>/<user>` (from the server's `refs/shared/<branch>/<user>`, which nobody pushes to). [`grit status`](../status/) shows them under your branch, [`grit sync`](../sync/) fast-forwards your branch to a member's head that builds on yours, and [`grit push`](../push/) replaces your head after an amend or rebase without `--force` (it still checks that the server's head is the one you last fetched).

- **`grit share add <branch> <user>...`** shares a branch of your space with people, each of whom is emailed. A branch that isn't in your space yet, or differs there, is pushed first. Anyone on the branch can add people. Someone added by name gets their copy of the project (with the branch in it) the first time they fetch from their space: `grit clone https://<server>/<them>/<project>`, or `grit sync` in a clone they have.
- **`--link`** makes a share link instead (or as well): a URL anyone signed in can accept, in the browser or with `grit share accept`. A link works once and for 7 days unless you pass `--uses` (a number or `unlimited`) and `--expires` (1 to 30 days). The URL is shown only now; the server keeps only its hash.
- **`grit share list`** shows your shared branches: who is on each, where each head is, and how it compares with your branch here (`same as yours`, `has commits you don't`, `behind yours`, `diverged from yours`), and any open links.
- **`grit share accept <link>`** accepts a link: you are on the branch, and your copy of the project is made in your space. In a clone of the project it fetches the branch into it; anywhere else it clones your copy into a new directory.
- **`grit share remove <branch> <user>...`** removes people. Only the person who shared the branch (or, for a copy of an organization's project, someone who can integrate into it) can remove people, and nobody can remove the person who shared it. With no users, it ends the share. Nobody loses work: each member's head stays a branch in their own space.
- **`grit share leave <branch>`** takes you off a branch; your head stays a branch in your space.
- **`grit share revoke <branch> <link-id>`** stops a link from working. Link ids are in `grit share list`.

### Your copy

When the project is a copy of an organization's project, everyone's copy is a copy of that organization's project. Otherwise each member's copy holds just the shared branches and the sharer's default branch, read-only, as the base they build on; the sharer's other branches stay private.

### When heads differ

`grit sync` compares your branch with every other member's head:

- one that builds on yours: your branch is fast-forwarded to it (to the furthest such head, when they are on one line), and your head is pushed;
- one that your branch builds on: nothing to do;
- one that has diverged from yours: reported. Rebase onto it with `grit rebase <remote>/shared/<branch>/<user>` (or have them rebase onto yours) and sync again.

## Options

| Option | Description |
| --- | --- |
| `--link` | `add`: make a share link anyone signed in can accept. |
| `--uses <n>` | `add --link`: how many times it can be accepted, or `unlimited`. Default 1. |
| `--expires <days>` | `add --link`: how many days it works, 1 to 30. Default 7. |

## Examples

Share a branch with bob, and make a link for anyone else:

```console
$ grit share add feature bob --link
· pushed feature to alice/widget first
✓ shared with bob, who is emailed
✓ share link (1 use, until 2026-11-11T12:00:00Z):
  https://grithub.dev/share/0f3a9c
feature, shared with bob
  you    538e7ad
  bob    -        hasn't fetched it yet
  link 12  1 use left, until 2026-11-11T12:00:00Z
```

Accept a link outside any clone:

```console
$ grit share accept https://grithub.dev/share/0f3a9c
✓ Joined feature, shared by alice
  your copy  bob/widget
  cloned to  /home/bob/widget
Switch to it with `grit switch feature`; push your head with `grit push`, and `grit sync` brings the others' in.
```

See where everyone is:

```console
$ grit share list
feature, shared with bob, carol, dave
  you    538e7ad
  bob    1c3e9a0  has commits you don't   grit sync
  carol  2b12f15  diverged from yours
  dave   -        hasn't fetched it yet
  link 12  1 use left, until 2026-11-11T12:00:00Z
```

## JSON output

Schema: [share.add v1](/schemas/share.add/v1.json) · [share.list v1](/schemas/share.list/v1.json) · [share.accept v1](/schemas/share.accept/v1.json) · [share.remove v1](/schemas/share.remove/v1.json) · [share.leave v1](/schemas/share.leave/v1.json) · [share.revoke v1](/schemas/share.revoke/v1.json)

`grit share add`, `list`, `remove`, `leave` and `revoke` have the same shape:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `action` | string | `add`, `list`, `remove`, `leave` or `revoke`. |
| `project` | string | Your project the shares are in, `<you>/<project>`. |
| `shares` | array | Each shared branch: `branch`, `origin`, `sharer`, `state` (`active`, `integrated`, `ended`), `mode` (`full`), `base_branch`, `members` (`user`, `sharer`, `you`, `head`, `project`, and `relation` to your branch: `same`, `ahead`, `behind`, `diverged`, `unknown`), `links` (`id`, `uses_left`, `expires_at`) and `url`. |
| `link` | object or omitted | The link `add --link` made: `id`, `url`, `uses_left` and `expires_at`. |
| `added` | array or omitted | People added, who were emailed. |
| `removed` | array or omitted | People removed (or you, when leaving). |
| `pushed` | boolean or omitted | Whether the branch was pushed to your space first. |

<!-- schema: share.list -->

```json
{
  "action": "list",
  "project": "alice/widget",
  "shares": [
    {
      "branch": "feature",
      "origin": "alice/widget",
      "sharer": "alice",
      "state": "active",
      "mode": "full",
      "base_branch": "main",
      "members": [
        { "user": "alice", "sharer": true, "you": true, "head": "538e7ad0c3b2a1f0e9d8c7b6a5f4e3d2c1b0a9f8", "project": "alice/widget" },
        { "user": "bob", "sharer": false, "you": false, "head": "1c3e9a0b2d4f6a8c0e2b4d6f8a0c2e4b6d8f0a2c", "project": "bob/widget", "relation": "ahead" }
      ],
      "url": "https://grithub.dev/alice/widget"
    }
  ]
}
```

`grit share accept`:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `status` | string | `accepted` or `already_member`. |
| `branch` | string | The shared branch. |
| `sharer` | string | Who shared it. |
| `origin` | string | The project it was shared from. |
| `project` | string | Your copy, `<you>/<project>`. |
| `url` | string | Your copy's clone URL. |
| `path` | string or omitted | The clone it was fetched into, or cloned as. |
| `cloned` | boolean | Whether this made a new clone. |

<!-- schema: share.accept -->

```json
{
  "status": "accepted",
  "branch": "feature",
  "sharer": "alice",
  "origin": "alice/widget",
  "project": "bob/widget",
  "url": "https://grithub.dev/bob/widget.git",
  "path": "/home/bob/widget",
  "cloned": true
}
```

## Markdown output

Pass [`--markdown`](../global-options/) for agent-friendly output.

Schema: [share.add v1](/schemas/share.add/v1.json) · [share.list v1](/schemas/share.list/v1.json) · [share.accept v1](/schemas/share.accept/v1.json) · [share.remove v1](/schemas/share.remove/v1.json) · [share.leave v1](/schemas/share.leave/v1.json) · [share.revoke v1](/schemas/share.revoke/v1.json)

```markdown
## Shared branches in `alice/widget`

### `feature`

Shared by `alice` from `alice/widget`; state active, base `main`.

| Member | Head | Compared with yours |
| --- | --- | --- |
| `alice` (you) | `538e7ad` | - |
| `bob` | `1c3e9a0` | has commits you don't |
| `carol` | `2b12f15` | diverged from yours |
| `dave` | not fetched yet | - |
```

## Exit status

| Code | Meaning |
| --- | --- |
| `0` | Done. |
| `1` | The server refused (the message says why: not on the branch, only the sharer can remove people, a used-up or expired link), or the command could not run: not a clone of a hosted project, not signed in. |
| `2` | Invalid arguments, such as `--uses` without `--link`. |

## See also

- [`grit sync`](../sync/), [`grit push`](../push/), [`grit status`](../status/), [`grit clone`](../clone/), [`grit login`](../auth/).
