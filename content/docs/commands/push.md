---
title: grit push
summary: Publish the current branch, or your tags, to a remote.
group: Remotes
order: 4
---

## Synopsis

```text
grit push
grit push --tags
```

## Description

Sends the current branch to the remote, creating the branch there if it doesn't exist. There are no arguments and no upstream to set: the branch goes to `origin`, under the same name. To push a branch somewhere else, set `branch.<name>.remote` and `branch.<name>.merge` with [`grit config`](../config/).

`grit push` never overwrites commits on the remote. If the remote branch has commits you don't have, the push is rejected and `grit` exits with an error; run [`grit pull`](../pull/) and push again.

With `--tags`, pushes every local tag to `origin` instead of the current branch.

For GitHub over HTTPS, if a push fails because you aren't signed in, `grit` offers to run [`grit auth`](../auth/) and then tries again.

## Options

| Option | Description |
| --- | --- |
| `-t`, `--tags` | Push all tags instead of the current branch. |

## Examples

```console
$ grit push
  pushed main → origin refs/heads/main

$ grit push
  origin refs/heads/main already up to date

$ grit push --tags
  pushed --tags → origin refs/tags/v0.1
```

When someone else pushed first:

```console
$ grit push
  rejected origin refs/heads/main: not a fast-forward — run `grit pull` first
```

## JSON output

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `remote` | string | Remote that was pushed to. |
| `branch` | string | Branch that was pushed, or `--tags`. |
| `results` | array | Per-ref outcomes with `ref` and `status` (`ok`, `up_to_date`, or a rejection). |
| `rejected` | boolean | `true` if any ref was rejected (exit status is also `1`). |

Example:

```json
{
  "remote": "origin",
  "branch": "main",
  "results": [
    { "ref": "refs/heads/main", "status": "ok" }
  ],
  "rejected": false
}
```

## See also

[grit pull](../pull/), [grit tag](../tag/), [grit auth](../auth/)
