---
title: grit fetch
summary: Download new commits, branches and tags from a remote.
group: Remotes
order: 2
---

## Synopsis

```text
grit fetch [<remote>] [<refspec>…] [--tags] [--no-tags] [--prune] [--no-prune] [--dry-run] [-q]
```

## Description

Downloads objects and updates refs from a remote. Your working tree and current branch are unchanged; use [`grit merge`](../merge/) or [`grit pull`](../pull/) to integrate fetched commits.

`<remote>` defaults to `origin`. It may be a configured remote name, a URL, a filesystem path, or a [git bundle](https://git-scm.com/docs/git-bundle) file.

Each `<refspec>` selects what to fetch (`main`, `refs/heads/x:refs/heads/y`, `+refs/…` to force). When no refspecs are given, configured `remote.<name>.fetch` refspecs apply (usually all heads into `refs/remotes/<remote>/…`).

When the source is a **literal path or URL** (not a configured remote), grit does not update `refs/remotes/origin/*` unless you pass refspecs with destinations. [`FETCH_HEAD`](https://git-scm.com/docs/git-fetch#_discussion) is updated for explicit refspecs.

Human output lists each local ref with its remote source and old → new oids. Pruned refs show `deleted`; rejected updates are listed and exit **1** (same as [`grit push`](../push/) rejections).

Tag policy follows Git: default is follow-tags (reachable annotated tags), `--tags` fetches every remote tag, `--no-tags` disables automatic tags. `remote.<name>.tagOpt` (`--tags` / `--no-tags`) applies when the CLI does not override it.

`--dry-run` negotiates and reports updates without writing refs, objects, or `FETCH_HEAD`.

`-q` / `--quiet` suppresses progress and human output (JSON and markdown are unchanged).

## Options

| Option | Description |
| --- | --- |
| `<remote>` | Remote name, URL, path, or bundle. Defaults to `origin`. |
| `<refspec>…` | What to fetch; short names expand like Git (`main` → `refs/heads/main`). |
| `--tags` | Fetch all tags from the remote. |
| `--no-tags` | Do not fetch tags automatically. |
| `--prune` | Remove stale remote-tracking refs. |
| `--no-prune` | Do not prune, even when config enables it. |
| `--dry-run` | Report updates without writing. |
| `-q`, `--quiet` | Suppress progress and human stdout/stderr. |

## Examples

```console
$ grit fetch origin main
  refs/remotes/origin/main  ← refs/heads/main  a8e620a → b52cca6
Fetched 1 update from origin.

$ grit fetch
Already up to date with origin.

$ grit fetch --prune
  refs/remotes/origin/gone1  a1b2c3d → deleted
Fetched 1 update from origin.
```

Fetch a pull request ref into a local branch:

```console
$ grit fetch origin pull/123/head:pr-123
  refs/heads/pr-123  ← refs/pull/123/head  … → …
Fetched 1 update from origin.
```

## JSON output

Schema: [v1](/schemas/fetch/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `remote` | string | Remote that was fetched. |
| `dry_run` | boolean | True when `--dry-run` was used. |
| `rejected` | boolean | True when any update was rejected (exit 1). |
| `updates` | array | Per-ref results. Each object has `ref`, `remote_ref`, `status` (`new`, `fast_forward`, `forced`, `pruned`, `rejected`, or `up_to_date`), `old_oid` and `new_oid` (full hex or `null`), and `reason` when `status` is `rejected`. |
| `updated` | number | Count of refs that actually changed. |

Example:

```json
{
  "remote": "origin",
  "dry_run": false,
  "rejected": false,
  "updates": [
    {
      "ref": "refs/remotes/origin/main",
      "remote_ref": "refs/heads/main",
      "status": "fast_forward",
      "old_oid": "a8e620a111111111111111111111111111111111",
      "new_oid": "b52cca6222222222222222222222222222222222"
    }
  ],
  "updated": 1
}
```

Dry run:

```json
{
  "remote": "origin",
  "dry_run": true,
  "rejected": false,
  "updates": [
    {
      "ref": "refs/remotes/origin/main",
      "remote_ref": "refs/heads/main",
      "status": "fast_forward",
      "old_oid": "a8e620a111111111111111111111111111111111",
      "new_oid": "b52cca6222222222222222222222222222222222"
    }
  ],
  "updated": 0
}
```

## Markdown output

Pass [`--markdown`](../global-options/) for agent-friendly output.

Schema: [v1](/schemas/fetch/v1.json)

Markdown uses headings, bullet lists, and fenced blocks; pipe characters inside table cells are escaped. See the JSON output section for the stable field contract.

## See also

[grit pull](../pull/), [grit merge](../merge/), [grit remote](../remote/)
