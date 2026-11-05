---
title: grit share
summary: Work on a branch together with other users of a grit hosting server, each with their own head of it.
group: Remotes
order: 11
---

## Synopsis

```text
grit share add <branch> [<user>|<email>...] [--link [--uses <n>|unlimited] [--expires <days>]]
grit share list [<branch>]
grit share accept <link>
grit share remove <branch> [<user>|<email>...]
grit share leave <branch>
grit share resolve <branch> [<option>] [--with <user>]
grit share revoke <branch> <link-id>
```

## Description

On a grit hosting server such as [grithub](https://grithub.dev), your branches are private to your space. `grit share` lets other people work on one of them with you. A shared branch is in every member's space, and **each member has their own head of it**: only you move yours. Nobody's push can overwrite anyone else's work, so there is nothing to force, and every head anyone pushes is kept in the share's log on the server.

The other members' heads are fetched beside yours as `<remote>/shared/<branch>/<user>` (from the server's `refs/shared/<branch>/<user>`, which nobody pushes to). [`grit status`](../status/) shows them under your branch, [`grit sync`](../sync/) fast-forwards your branch to a member's head that builds on yours, and [`grit push`](../push/) replaces your head after an amend or rebase without `--force` (it still checks that the server's head is the one you last fetched).

- **`grit share add <branch> <user>...`** shares a branch of your space with people, each of whom is emailed. Name them by user name or email address: an address that belongs to an account adds that account, and any other address is emailed an invitation. The invitation's link asks them to create an account, then shows them how to work on the branch with grit or plain git; it works once, for 30 days. A branch that isn't in your space yet, or differs there, is pushed first. Anyone on the branch can add people. Someone added by name gets their copy of the project (with the branch in it) the first time they fetch from their space: `grit clone https://<server>/<them>/<project>`, or `grit sync` in a clone they have.
- **`--link`** makes a share link instead (or as well): a URL anyone signed in can accept, in the browser or with `grit share accept`. A link works once and for 7 days unless you pass `--uses` (a number or `unlimited`) and `--expires` (1 to 30 days). The URL is shown only now; the server keeps only its hash.
- **`grit share list`** shows your shared branches: who is on each, where each head is, and how it compares with your branch here (`same as yours`, `has commits you don't`, `behind yours`, `diverged from yours`), and any open links.
- **`grit share accept <link>`** accepts a link: you are on the branch, and your copy of the project is made in your space. In a clone of the project it fetches the branch into it; anywhere else it clones your copy into a new directory.
- **`grit share remove <branch> <user>...`** removes people, and withdraws invitations to the email addresses you name. Only the person who shared the branch (or, for a copy of an organization's project, someone who can integrate into it) can remove people, and nobody can remove the person who shared it. With no users, it ends the share. Nobody loses work: each member's head stays a branch in their own space.
- **`grit share leave <branch>`** takes you off a branch; your head stays a branch in your space.
- **`grit share resolve <branch> [<option>]`** puts your head and another member's back together after they diverged (see [Resolving a divergence](#resolving-a-divergence)). Name the member with `--with` when several heads diverged from yours.
- **`grit share revoke <branch> <link-id>`** stops a link from working. Link ids are in `grit share list`.

### Your copy

When the project is a copy of an organization's project, everyone's copy is a copy of that organization's project. Otherwise each member's copy holds just the shared branches and the sharer's default branch, read-only, as the base they build on; the sharer's other branches stay private.

### When heads differ

`grit sync` compares your branch with every other member's head:

- one that builds on yours: your branch is fast-forwarded to it (to the furthest such head, when they are on one line), and your head is pushed;
- one that your branch builds on: nothing to do;
- one that has diverged from yours: reported. Rebase onto it with `grit rebase <remote>/shared/<branch>/<user>` (or have them rebase onto yours) and sync again.

### Divergence scenarios

When another member's head has diverged from yours, `grit sync`, `grit status`, `grit share list` and `grit push` say what happened, commit by commit: for each commit, what you did and what they did (`unchanged`, `moved`, `reworded`, `amended`, `dropped`, `squashed`, `split`, `new`), matched by change id. The situation gets one of these codes, which scripts and agents can rely on. Some can be put together without losing anything; the rest need someone to decide, and `grit push` and `grit sync` exit with status `3` for them.

| Scenario | What happened | Needs a decision |
| --- | --- | --- |
| `added_both` | You both added commits on top of the same base, and they don't conflict. | no |
| `rewrote_one` | One of you rewrote commits (amended, reordered, split, squashed or dropped); the other only added on top. | no |
| `amended_both_compatible` | You both amended the same commit, without overlapping (or one changed only its message). | no |
| `amended_one_reordered_other` | One reordered commits; the other amended one of them. | no |
| `amended_different` | You each amended different commits. | no |
| `rebased_one` | One rebased onto a newer target; the other only changed commits. | no |
| `rebased_both` | You both rebased, onto different target commits. | no |
| `dropped_one` | One dropped a commit the other didn't touch. | no |
| `dropped_both` | You both dropped the same commit. | no |
| `split_one` | One split a commit the other didn't touch. | no |
| `squashed_one_added_other` | One squashed commits; the other added on top. | no |
| `squashed_one_amended_other` | One squashed a commit into another; the other amended it. | no |
| `split_one_amended_other` | One split a commit; the other's amendment of it fits one piece. | no |
| `amended_both_conflicting` | You both amended the same commit, and the edits overlap. | yes |
| `added_both_conflict` | You both added commits, which conflict with each other. | yes |
| `dropped_one_amended_other` | One deleted a commit; the other amended it. | yes |
| `reordered_both` | You both reordered the commits, into different orders. | yes |
| `split_both` | You both split the same commit, differently. | yes |
| `split_one_amended_other_spanning` | One split a commit; the other's amendment of it spans the pieces. | yes |
| `squashed_both_differently` | You both squashed commits, differently. | yes |
| `reset_one` | One head drops most of the branch: perhaps an accidental reset. | yes |
| `merge_commit` | One head has a merge commit. | yes |
| `no_change_ids` | Commits made or rebased with plain `git` have no change ids, so an amend can't be told from a new commit. | yes |

Heads that haven't diverged are `same`, `ahead`, `behind`, `same_content` (the same changes as different commits) or `same_rewrite` (the same amend, drop or squash on both sides).

The comparison starts from the last head you both had, read from the reflogs of your branch and of their head here; without one, from where the two heads meet. Use `grit commit` and `grit rebase`, which keep change ids, so commits can be followed across amends.

### Resolving a divergence

Every divergence report lists its options, the recommended one first, each with the command that applies it and what it leaves out (`loses`, `null` when it keeps every change). What an option leaves out is never lost: every head anyone pushed stays in the share's log on the server.

| Option | What it does |
| --- | --- |
| `combine` | Every change from both heads, commit by commit: their commits kept as they are where possible, yours replayed on top, so they can fast-forward to the result. |
| `keep_both` | For a commit you both amended, differently: their version, then yours as a commit of its own on top. |
| `keep_amended` / `drop` | For a commit one of you dropped and the other amended: keep the amended version, or drop it. |
| `my_order` / `their_order` | When you both reordered the commits. |
| `my_split` / `their_split` | When you both split a commit. |
| `restore` | When one head dropped most of the branch: bring the dropped commits back. |
| `keep_mine` / `keep_theirs` | One head as it is. |
| `resolve` | By hand: rebase onto their head (`grit rebase <remote>/shared/<branch>/<user>`), settle the conflicts, `grit rebase --continue`, and `grit push`. |

`grit share resolve <branch>` with no option applies `combine` when nothing would be lost; otherwise it lists the options and exits with status `3`. With an option, it moves your branch to the result (when the branch is checked out, as `grit reset --keep` does, keeping local changes or refusing) and pushes your head.

**Without asking.** When another member's head and yours can be put together without losing anything (any scenario that doesn't need a decision), `grit sync` and `grit push` do it themselves: they combine the two, move your branch, and push the result, and the other member's next `grit sync` fast-forwards to it. Only when someone has to decide do they stop, report the options, and exit `3`.

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

Schema: [share.add v1](/schemas/share.add/v1.json) · [share.list v1](/schemas/share.list/v1.json) · [share.accept v1](/schemas/share.accept/v1.json) · [share.remove v1](/schemas/share.remove/v1.json) · [share.leave v1](/schemas/share.leave/v1.json) · [share.revoke v1](/schemas/share.revoke/v1.json) · [share.resolve v1](/schemas/share.resolve/v1.json)

`grit share add`, `list`, `remove`, `leave` and `revoke` have the same shape:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `action` | string | `add`, `list`, `remove`, `leave` or `revoke`. |
| `project` | string | Your project the shares are in, `<you>/<project>`. |
| `shares` | array | Each shared branch: `branch`, `origin`, `sharer`, `state` (`active`, `integrated`, `ended`), `mode` (`full`), `base_branch`, `members` (`user`, `sharer`, `you`, `head`, `project`, `relation` to your branch: `same`, `ahead`, `behind`, `diverged`, `unknown`, and for a diverged head `divergence`, as in [`grit sync`](../sync/#json-output)), `invited` (email addresses that haven't accepted yet), `links` (`id`, `uses_left`, `expires_at`) and `url`. |
| `link` | object or omitted | The link `add --link` made: `id`, `url`, `uses_left` and `expires_at`. |
| `added` | array or omitted | People added, who were emailed: user names, and the email addresses invited. |
| `removed` | array or omitted | People removed (or you, when leaving), and the email addresses whose invitations were withdrawn. |
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

`grit share resolve`:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `branch` | string | The shared branch. |
| `with` | string | The member whose head yours was put together with. |
| `scenario` | string | What the two heads added up to, before. |
| `summary` | string | What happened, in a sentence. |
| `option` | string or omitted | The option applied; omitted when a decision is needed and none was chosen. |
| `previous` | string | Your head before. |
| `head` | string or omitted | Your head afterwards, when it moved. |
| `pushed` | boolean | Whether your head was pushed. |
| `next` | string or omitted | What to run next, for `resolve`, which is done by hand. |
| `options` | array or omitted | When a decision is needed and none was chosen: each option's `id`, `label`, `loses` and `command`. |

<!-- schema: share.resolve -->

```json
{
  "branch": "pairing",
  "with": "carol",
  "scenario": "amended_both_conflicting",
  "summary": "You and carol both amended \"Add the history RPC\", and the edits overlap in src/rpc.rs.",
  "option": "keep_both",
  "previous": "538e7ad0c3b2a1f0e9d8c7b6a5f4e3d2c1b0a9f8",
  "head": "9a0b1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e6f7a8b",
  "pushed": true
}
```

## Markdown output

Pass [`--markdown`](../global-options/) for agent-friendly output.

Schema: [share.add v1](/schemas/share.add/v1.json) · [share.list v1](/schemas/share.list/v1.json) · [share.accept v1](/schemas/share.accept/v1.json) · [share.remove v1](/schemas/share.remove/v1.json) · [share.leave v1](/schemas/share.leave/v1.json) · [share.revoke v1](/schemas/share.revoke/v1.json) · [share.resolve v1](/schemas/share.resolve/v1.json)

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
| `1` | The server refused (the message says why: not on the branch, only the sharer can remove people, a used-up or expired link), the option chosen conflicts, or the command could not run: not a clone of a hosted project, not signed in. |
| `3` | `grit share resolve` without an option, for a divergence that needs a decision: the options are listed (`needs_decision`). |
| `2` | Invalid arguments, such as `--uses` without `--link`. |

## See also

- [`grit sync`](../sync/), [`grit push`](../push/), [`grit status`](../status/), [`grit clone`](../clone/), [`grit login`](../auth/).
