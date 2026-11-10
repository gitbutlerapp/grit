---
title: grit sync
summary: Push and pull every active branch between a clone and your space on a grit hosting server.
group: Remotes
order: 10
---

## Synopsis

```text
grit sync
```

## Description

On a grit hosting server such as [GitButler Code](https://code.gitbutler.com), your work on a project lives in your space, `<you>/<project>`, private to you. An organization's projects (`<domain>/<project>`) are read-only: you work in a copy of one in your space, which follows the organization's branches, and [integrate](../integrate/) from there.

`grit sync`, in a clone of a project on such a server:

1. **asks the server where your work goes**: the project in your space for the one `origin` names. For an organization's project, the server makes your copy when you have none yet. The clone is then set up to push there: a `space` remote fetches from it, and `remote.origin.pushurl` points at it, so a plain [`grit push`](../push/) (or `git push`) sends branches to your space while fetches still come from the organization. [`grit clone`](../clone/) of a GitButler Code project does this setup too, when you are signed in;
2. **fetches** `origin` and your space;
3. **fast-forwards the target branch** (the branch work is integrated into, usually `main`) to the project's. A target with commits of its own is left alone: it changes only by integrating;
4. **brings down your space's branches**: one you don't have here is made, tracking your space's; one that is behind is fast-forwarded (when it is checked out, the working tree follows, as `grit merge --ff-only` would);
5. **pushes every active branch** to your space: every branch but the target, the branches your space follows from upstream (read-only there), and branches already merged into the target, such as one you integrated.

A branch that has commits here and in your space that the other lacks is left alone and reported as `diverged`: switch to it, run [`grit pull --rebase`](../pull/), and sync again. Nothing is ever force-pushed.

### Shared branches

A branch you [share](../share/) with other people works differently, because each member has their own head of it and only you move yours:

- the other members' heads are fetched too, as `<remote>/shared/<branch>/<user>`, and reported under the branch with how each compares with yours;
- when another member's head builds on yours, your branch is fast-forwarded to it (to the furthest such head, when they are on one line) and then pushed as your head (`took bob's head, which builds on yours`);
- when your branch here and your head in your space have diverged (you amended or rebased here), your head is replaced: it is yours alone, and the server keeps the old one in the share's log;
- a member's head that has diverged from yours, but goes together with it without losing anything, is combined with yours, and the result pushed (`combined with bob's head, ...`). One that needs a decision is reported with its options (see [`grit share resolve`](../share/#resolving-a-divergence)), and sync exits `3`.

Sync works in a clone. In a [snapshot checkout](../checkout/), send branches with [`grit checkin`](../checkin/) and update with [`grit pull`](../pull/). You need to be signed in to the server ([`grit login`](../auth/)).

## Options

`grit sync` takes no options besides the [global options](../global-options/).

## Examples

A clone of an organization's project, the first time:

```console
$ grit clone https://code.gitbutler.com/example.com/widget
$ cd widget
$ grit switch -c feature
$ grit commit -am "Make widgets faster"
$ grit sync
Syncing alice/widget (a copy of example.com/widget)
· Pushes from this clone now go to alice/widget (remote `space`)
✓ main fast-forwarded to 7f51f16
✓ feature pushed 538e7ad
✓ docs pulled 1c3e9a0
✓ from-laptop created 9a0b1c2
✗ spike diverged 2b12f15
  spike and space/spike each have commits the other lacks: switch to it and run `grit pull --rebase`, then sync again
```

## JSON output

Schema: [v1](/schemas/sync/v1.json)

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `space` | string | Your space's project, `<you>/<project>`. |
| `upstream` | string or omitted | The project it follows, for a copy of an organization's project. |
| `remote` | string | The remote branches are pushed to and pulled from: `origin` when it is your space, else `space`. |
| `configured` | boolean | Whether this run set the clone up to push to your space. |
| `target` | object or omitted | The target branch: `name`, `action` (`up_to_date`, `fast_forwarded`, `created`, `ahead` or `diverged`) and `tip`. |
| `branches` | array | Each active branch: `name`, `action` (`pushed`, `pulled`, `created`, `up_to_date`, `diverged` or `refused`), `tip`, a `message` for a divergence or refusal, and for a [shared](../share/) branch `shared`: the other members' heads, each with `user`, `tip`, `relation` to your branch (`same`, `ahead`, `behind`, `diverged` or `unknown`), and for a head that diverged from yours `divergence`: `scenario` (a stable code such as `amended_both_conflicting`; see [Divergence scenarios](../share/#divergence-scenarios)), `summary`, `needs_decision`, `base`, `changes` (each commit's `change`, `subject`, and what `you` and `them` did to it), `conflicts` and `also`. For a branch the sync made again after a share of it was integrated, `integrated`: the `pushed_to_integrated` report, as [`grit push`](../push/) has it. |

```json
{
  "space": "alice/widget",
  "upstream": "example.com/widget",
  "remote": "space",
  "configured": false,
  "target": { "name": "main", "action": "up_to_date", "tip": "7f51f16b2cb0e4d3c2b1a0f9e8d7c6b5a4f3e2d1" },
  "branches": [
    { "name": "feature", "action": "pushed", "tip": "538e7ad0c3b2a1f0e9d8c7b6a5f4e3d2c1b0a9f8" }
  ]
}
```

## Markdown output

Pass [`--markdown`](../global-options/) for agent-friendly output.

Schema: [v1](/schemas/sync/v1.json)

```markdown
## Sync `alice/widget`

- **upstream**: `example.com/widget`
- **remote**: `space`
- **main**: fast-forwarded
- `feature`: pushed
- `spike`: diverged — spike and space/spike each have commits the other lacks: switch to it and run `grit pull --rebase`, then sync again
```

## Exit status

| Code | Meaning |
| --- | --- |
| `0` | Every active branch is in step with your space. |
| `1` | A branch diverged or a push was refused (the output names it), the target diverged from upstream, or sync could not run: not a clone of a hosted project, not signed in, or this is a snapshot checkout. |
| `3` | Everything else is in step, but another member's head of a [shared](../share/) branch diverged from yours in a way someone has to decide about, or a branch was pushed again after it was integrated (`pushed_to_integrated`) (`needs_decision`). |

## See also

- [`grit integrate`](../integrate/), [`grit push`](../push/), [`grit clone`](../clone/), [`grit login`](../auth/).
