---
title: grit push
summary: Publish the current branch or tags, or delete a remote ref.
group: Remotes
order: 4
---

## Synopsis

```text
grit push
grit push --tags
grit push --delete <ref>
grit push --remote <name>
```

## Description

Sends the current branch to the remote, creating the branch there if it doesn't exist. There are no positional arguments: the branch goes to its configured remote (usually `origin`), under the same name. To push a branch somewhere else, set `branch.<name>.remote` and `branch.<name>.merge` with [`grit config`](../config/).

`grit push` never overwrites commits on the remote. If the remote branch has commits you don't have, the push is rejected and `grit` exits with an error; run [`grit pull`](../pull/) and push again.

With `--tags`, pushes every local tag to the remote instead of the current branch.

With `--delete`, deletes a remote branch or tag (short names map to `refs/heads/<name>` when that branch exists locally, otherwise `refs/tags/<name>`). Repeat `--delete` to remove several refs in one push.

Use `--remote` to target a remote other than the current branch's configured remote.

For GitHub over HTTPS, if a push fails because you aren't signed in, `grit` offers to run [`grit auth`](../auth/) and then tries again.

## Options

| Option | Description |
| --- | --- |
| `-t`, `--tags` | Push all tags instead of the current branch. |
| `--delete` | Delete a remote ref (branch or tag). May be repeated. |
| `--remote` | Remote name (default: current branch remote or `origin`). |

## Examples

```console
$ grit push
  pushed main → origin refs/heads/main

$ grit push --tags
  pushed --tags → origin refs/tags/v0.1

$ grit push --delete v0.1
  pushed --delete → origin refs/tags/v0.1

$ grit push --delete old-topic --remote origin
  pushed --delete → origin refs/heads/old-topic
```

When someone else pushed first:

```console
$ grit push
  rejected origin refs/heads/main: not a fast-forward — run `grit pull` first
```

## JSON output

Schema: [v1](/schemas/push/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `remote` | string | Remote that was pushed to. |
| `branch` | string | Branch that was pushed, `--tags`, or `--delete`. |
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

## Markdown output

Pass [`--markdown`](../global-options/) for agent-friendly output.

Schema: [v1](/schemas/push/v1.json)

Markdown uses headings, bullet lists, and fenced blocks; pipe characters inside table cells are escaped. See the JSON output section for the stable field contract.

## See also

[grit pull](../pull/), [grit tag](../tag/), [grit auth](../auth/)
