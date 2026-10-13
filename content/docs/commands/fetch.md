---
title: grit fetch
summary: Download new commits, branches and tags from a remote.
group: Remotes
order: 2
---

## Synopsis

```text
grit fetch [<remote>] [--prune] [--no-prune]
```

## Description

Downloads everything new from a remote and updates your remote-tracking branches, such as `origin/main`, along with any new tags. Your own branches and working tree are left alone; to bring the new commits into your branch, use [`grit merge`](../merge/) or [`grit pull`](../pull/).

`<remote>` may be a configured remote name or a path to a [git bundle](https://git-scm.com/docs/git-bundle) file (detected by the bundle signature, not the file extension). Bundle fetches verify that prerequisite commits are already in your repository before applying the pack.

Each updated ref is listed with its old and new commit. Removed remote-tracking refs (after `--prune`) show `deleted` on the human line and `new_oid: null` in JSON.

When `fetch.prune` or `remote.<name>.prune` is true, stale remote-tracking refs under `refs/remotes/<remote>/` are removed automatically. Per-remote `remote.<name>.prune` overrides the global `fetch.prune` value. `--prune` and `--no-prune` override config for this invocation.

Repositories created as shallow clones with Git (for example `git clone --depth 1`) are supported: `grit fetch` respects the existing shallow boundary, does not create tag refs to missing commits, and leaves a clean repository when there is nothing new to download.

On an interactive terminal (human output, not `--json` or `--markdown`), grit prints live progress on stderr while objects are received and indexed, using the same style as [`grit clone`](../clone/). When stderr is not a TTY, no progress lines are shown.

## Options

| Option | Description |
| --- | --- |
| `<remote>` | Configured remote name, or a path/URL to a repository or bundle file. Defaults to `origin`. |
| `--prune` | Remove remote-tracking refs that no longer exist on the remote. |
| `--no-prune` | Do not prune, even when `fetch.prune` or `remote.<name>.prune` is enabled. |

## Examples

```console
$ grit fetch
Receiving objects: 4.00 KiB, 8.00 KiB/s, done.
  refs/remotes/origin/main  a8e620a → b52cca6
Fetched 1 update from origin.

$ grit fetch
Already up to date with origin.

$ grit fetch --prune
  refs/remotes/origin/gone1  a1b2c3d → deleted
Fetched 1 update from origin.
```

Fetch from another remote:

```console
$ grit fetch upstream
```

## JSON output

Schema: [v1](/schemas/fetch/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `remote` | string | Remote that was fetched. |
| `updates` | array | Ref updates, each with `ref`, `old_oid`, and `new_oid`. |
| `updated` | number | Count of refs that changed. |

`old_oid` is `null` for a ref that is new, and `new_oid` is `null` for one that was removed.

Example:

```json
{
  "remote": "origin",
  "updates": [],
  "updated": 0
}
```

## Markdown output

Pass [`--markdown`](../global-options/) for agent-friendly output.

Schema: [v1](/schemas/fetch/v1.json)

Markdown uses headings, bullet lists, and fenced blocks; pipe characters inside table cells are escaped. See the JSON output section for the stable field contract.

## See also

[grit pull](../pull/), [grit merge](../merge/), [grit remote](../remote/)
