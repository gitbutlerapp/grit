---
title: grit fetch
summary: Download new commits, branches and tags from a remote.
group: Remotes
order: 2
---

## Synopsis

```text
grit fetch [<remote>] [<refspec>…] [--all] [--depth N] [--deepen N] [--unshallow] [--tags] [--no-tags] [--prune] [--no-prune] [--dry-run] [-q]
```

## Description

Downloads objects and updates refs from a remote. Your working tree and current branch are unchanged; use [`grit merge`](../merge/) or [`grit pull`](../pull/) to integrate fetched commits.

`<remote>` defaults to `origin`. It may be a configured remote name, a URL, an scp-style `host:path`, a filesystem path, or a [git bundle](https://git-scm.com/docs/git-bundle) file. Anything else fails with `unknown remote`.

Each `<refspec>` selects what to fetch (`main`, `refs/heads/x:refs/heads/y`, `+refs/…` to force). When no refspecs are given, configured `remote.<name>.fetch` refspecs apply (usually all heads into `refs/remotes/<remote>/…`).

When the source is a **literal path or URL** (not a configured remote), grit does not update `refs/remotes/origin/*` unless you pass refspecs with destinations. [`FETCH_HEAD`](https://git-scm.com/docs/git-fetch#_discussion) is updated for explicit refspecs.

Human output lists each local ref with its remote source and old → new oids. Pruned refs show `deleted`; rejected updates are listed and exit **1** (same as [`grit push`](../push/) rejections). When a stale remote-tracking ref blocks another update (for example `origin/gone/x` still exists locally but the remote now has `gone`), grit still applies every ref it can and reports the blocked ref as rejected; the `reason` field suggests `grit fetch --prune` or `grit remote prune <remote>` when pruning would remove the stale ref.

Tag policy follows Git: by default grit follows tags, fetching every tag (lightweight or annotated) that points into the fetched history. That includes tags on commits, trees and blobs that arrive with the fetch, tags on commits you already have that the fetched refs reach, and tags on trees or blobs you already have. `--tags` fetches every remote tag, and `--no-tags` disables automatic tags. Tag following costs milliseconds even on large repositories. `remote.<name>.tagOpt` (`--tags` / `--no-tags`) applies when the CLI does not override it.

`--dry-run` negotiates and reports updates without writing refs, objects, or `FETCH_HEAD`. Human output ends with **Would fetch N update(s) from …** when anything would change, or **Already up to date with …** when nothing would.

`--depth`, `--deepen`, and `--unshallow` match Git’s shallow fetch controls (mutually exclusive). `--depth` truncates history on the first shallow fetch; `--deepen` adds commits relative to an existing shallow clone; `--unshallow` fetches the rest and removes `.git/shallow`. `--unshallow` on a complete repository is a typed error.

`-q` / `--quiet` suppresses progress and human output (JSON and markdown are unchanged).

`--all` fetches every configured remote in config order. Remotes with `remote.<name>.skipFetchAll=true` are skipped. Each remote uses its own `remote.<name>.prune` and `remote.<name>.tagOpt` unless `--prune`, `--no-prune`, `--tags`, or `--no-tags` override on the command line. Failures on one remote do not stop the rest; the command exits **1** if any remote failed or any ref update was rejected. `--all` cannot be combined with `<remote>` or `<refspec>` arguments.

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
| `--all` | Fetch all configured remotes (see above). |
| `--depth N` | Shallow fetch: keep at most N commits from each tip. |
| `--deepen N` | Deepen a shallow clone by N commits (relative). |
| `--unshallow` | Fetch full history and drop shallow boundaries. |
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

$ grit fetch --dry-run --prune
  refs/remotes/origin/gone1  a1b2c3d → deleted
  refs/remotes/origin/feature  ← refs/heads/feature  new → b52cca6
Would fetch 2 updates from origin.
```

Fetch a pull request ref into a local branch:

```console
$ grit fetch origin pull/123/head:pr-123
  refs/heads/pr-123  ← refs/pull/123/head  … → …
Fetched 1 update from origin.
```

## JSON output

Pass `--json` for stable, scripting-friendly output.

### Single remote (default)

Schema: [v1](/schemas/fetch/v1.json)

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `remote` | string | Remote that was fetched. |
| `dry_run` | boolean | True when `--dry-run` was used. |
| `rejected` | boolean | True when any update was rejected (exit 1). |
| `updates` | array | Per-ref results. Each object has `ref`, `remote_ref`, `status` (`new`, `fast_forward`, `forced`, `pruned`, `rejected`, or `up_to_date`), `old_oid` and `new_oid` (full hex or `null`), and `reason` when `status` is `rejected`. |
| `updated` | number | Count of refs that actually changed. Always **0** when `dry_run` is true (nothing is written); count pending changes by filtering `updates` where `status` is not `up_to_date`. |
| `shallow` | object | When present: `added` and `removed` lists of full hex oids for new/removed `.git/shallow` boundaries. |

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

### `--all`

Schema: [all v1](/schemas/fetch.all/v1.json)

When `--all` is used, JSON has no top-level `remote` field. Each entry in `remotes` matches the single-remote shape (`remote`, `updates`, `updated`) plus an optional `error` string when that remote could not be fetched.

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `dry_run` | boolean | True when `--dry-run` was used. |
| `remotes` | array | One object per remote attempted (skipFetchAll remotes omitted). |
| `updated` | number | Total refs changed across all successful remotes. Always **0** when `dry_run` is true. |

Example:

```json
{
  "remote": "origin",
  "updates": [],
  "updated": 0,
  "dry_run": false,
  "rejected": false
}
```

## Markdown output

Pass [`--markdown`](../global-options/) for agent-friendly output.

Schema: [v1](/schemas/fetch/v1.json)

Markdown uses headings, bullet lists, and fenced blocks; pipe characters inside table cells are escaped. See the JSON output section for the stable field contract.




```text
Already up to date with `origin`.
```

## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Success ([`success`](../exit-codes/)). |
| `1` | Command failed ([`failure`](../exit-codes/)). |
| `2` | Invalid flags or arguments ([`usage`](../exit-codes/)). |

See [Exit codes](../exit-codes/) for shared rules and structured `--json` `kind` values.


## See also

[grit pull](../pull/), [grit merge](../merge/), [grit remote](../remote/)