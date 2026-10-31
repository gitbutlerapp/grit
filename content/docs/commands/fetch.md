---
title: grit fetch
summary: Download new commits, branches and tags from a remote.
group: Remotes
order: 2
---

## Synopsis

```text
grit fetch [<remote>] [<refspec>…] [--all] [--depth N] [--deepen N] [--unshallow] [--tags] [--no-tags] [--prune] [--no-prune] [--dry-run] [-f] [-q]
```

## Description

Downloads objects and updates refs from a remote. Your working tree and current branch are unchanged; use [`grit merge`](../merge/) or [`grit pull`](../pull/) to integrate fetched commits.

`<remote>` defaults to the current branch's remote (`branch.<name>.remote`, the remote `grit branch -u` and `grit push -u` record), and to `origin` when that is not set, when the branch tracks a local branch (`branch.<name>.remote = .`), or when `HEAD` is detached. It may be a configured remote name, a URL, an scp-style `host:path`, a filesystem path, or a [git bundle](https://git-scm.com/docs/git-bundle) file. Anything else fails with `unknown remote`.

Each `<refspec>` selects what to fetch (`main`, `refs/heads/x:refs/heads/y`, `+refs/…` to force). When no refspecs are given, configured `remote.<name>.fetch` refspecs apply (usually all heads into `refs/remotes/<remote>/…`).

When the source is a **literal path or URL** (not a configured remote), grit does not update `refs/remotes/origin/*` unless you pass refspecs with destinations. [`FETCH_HEAD`](https://git-scm.com/docs/git-fetch#_discussion) is updated for explicit refspecs.

Human output lists each local ref with its remote source and old → new oids. Pruned refs show `deleted`; rejected updates are listed and exit **1** (same as [`grit push`](../push/) rejections). When a stale remote-tracking ref blocks another update (for example `origin/gone/x` still exists locally but the remote now has `gone`), grit still applies every ref it can and reports the blocked ref as rejected; the `reason` field suggests `grit fetch --prune` or `grit remote prune <remote>` when pruning would remove the stale ref.

When the fetch received objects, the ref list is followed by the same transfer summary [`grit clone`](../clone/) prints: a `time` bar splitting the run into connecting, receiving, indexing, resolving deltas and finishing; the `data` received off the wire with its average and peak rate; and the `objects` received by type. Local `file://` and plain-path remotes use the same summary shape, with **Enumerating** (reachability walk) and **Copying** (link or pack the send set) instead of network **Receiving**. Nothing extra is printed when the fetch was already up to date. With `--all`, one summary after the last remote covers the whole run.

On a terminal, stderr first shows `Fetching <remote> <url>`, then one live progress line per phase with a throughput graph and, under it, an overall bar with a time-left estimate. Finished phases stay on screen as `✓` lines. When stderr is not a TTY, or with `--json`, `--markdown` or `-q`, no progress is drawn.

By default grit follows tags, fetching every tag (lightweight or annotated) that points into the fetched history. That includes tags on commits, trees and blobs that arrive with the fetch, and tags on trees or blobs you already have. A tag on a commit you already have is followed only when the fetched refs reach that commit; Git also follows it when the commit is unreachable from them. In a partial clone, tag following never downloads missing objects. `--tags` fetches every remote tag, and `--no-tags` disables automatic tags. Tag following costs milliseconds even on large repositories. `remote.<name>.tagOpt` (`--tags` / `--no-tags`) applies when the CLI does not override it.

Default tag following never updates a tag you already have locally, even when the remote retargets it — a plain `grit fetch` stays quiet and exits **0**, matching Git. To refresh moved release tags, use `grit fetch --tags` (reports a rejection until you accept the move) or `grit fetch --tags --force`. A refspec that names tags (`refs/tags/rel:refs/tags/rel2`, `refs/tags/*:refs/tags/*`) is honoured like any other refspec; a leading `+` or `--force` on the fetch forces the update (status `forced`).

`--dry-run` negotiates and reports updates without writing refs, objects, or `FETCH_HEAD`. Human output ends with **Would fetch N update(s) from …** when anything would change, or **Already up to date with …** when nothing would.

`--depth`, `--deepen`, and `--unshallow` match Git’s shallow fetch controls (mutually exclusive). `--depth` truncates history on the first shallow fetch; `--deepen` adds commits relative to an existing shallow clone; `--unshallow` fetches the rest and removes `.git/shallow`. `--unshallow` on a complete repository is a typed error. With default tag following, tags on newly fetched history (including annotated tags) arrive in that same operation; a second fetch is unnecessary. Following tags does not extend the requested branch depth. A shallow source can only supply its available history, so boundaries remain when the source is incomplete.

In a partial clone, a fetch takes part in the server's `promisor-remote` exchange (protocol v2) the way [`grit clone`](../clone/#large-object-promisors) does: it accepts the advertised large-object promisors that `promisor.acceptFromServer` allows, tells the server so, and applies `promisor.storeFields`. A remote whose `partialclonefilter` is `auto` (a clone made with `--blobs auto`) picks its filter on every fetch from the accepted remotes' advertised `partialCloneFilter`. A fetch that is neither filtered nor from a promisor remote declines every promisor remote (`not_partial`), because it needs every object from the server; that includes an unfiltered fetch from a second, non-promisor remote of a partial clone, whose pack would otherwise be marked promisor although no promisor remote can give its objects back. The human output adds a `promisor` line per advertised remote, for example `promisor  lop accepted · blob:limit=5k`, and `promisor  stored partialCloneFilter for lop` for each stored field.

`-q` / `--quiet` suppresses progress and human output (JSON and markdown are unchanged).

Local fetches reuse the repository's ref store across tracking refs and tags. An up-to-date fetch skips object-copy preparation while still handling pruning, `FETCH_HEAD`, and the remote's default branch. See the [many-ref local fetch measurements](../../benchmarks/#local-fetch-with-many-refs).

When `core.logAllRefUpdates` is enabled (grit's default for new repositories), each fetch that moves a remote-tracking branch writes a reflog entry on that ref (`fetch: fast-forward`, `fetch: forced-update`, or `fetch: storing head` for a newly stored tracking ref). [`grit pull`](../pull/) uses the same mechanism with a `pull:` prefix. The initial clone fetch does not create branch tracking reflogs (Git matches this); only `refs/remotes/<remote>/HEAD` gets `clone: from <url>`. Pruning a stale tracking ref removes its reflog file. Tags are not logged unless a reflog already exists or logging is set to `always`.

`--all` fetches every configured remote in config order. Remotes with `remote.<name>.skipFetchAll=true` are skipped. Each remote uses its own `remote.<name>.prune` and `remote.<name>.tagOpt` unless `--prune`, `--no-prune`, `--tags`, or `--no-tags` override on the command line. Failures on one remote do not stop the rest; the command exits **1** if any remote failed or any ref update was rejected. `--all` cannot be combined with `<remote>` or `<refspec>` arguments.

When a fetch is interrupted (`SIGINT`, `SIGTERM`, or `SIGHUP`, including Ctrl+C from a terminal), grit cancels the in-progress transfer, removes any partial pack temp file it was writing, and exits **130** without leaving `tmp_pack_*` or pack install staging directories behind. Stale temp files from a hard kill (`SIGKILL`) are removed later by [`grit gc`](gc.md) once they are older than `gc.pruneExpire`, the same way `git gc` prunes `tmp_pack_*`.

## Options

| Option | Description |
| --- | --- |
| `<remote>` | Remote name, URL, path, or bundle. Defaults to the current branch's remote, else `origin`. |
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
| `-f`, `--force` | Force non-fast-forward branch updates and tag moves when combined with `--tags` or a forced refspec. Does not move existing tags on a default fetch. |
| `-q`, `--quiet` | Suppress progress and human stdout/stderr. |

## Examples

```console
$ grit fetch origin main
  refs/remotes/origin/main  ← refs/heads/main  a8e620a → b52cca6
Fetched 1 update from origin.

$ grit fetch
Already up to date with origin.

$ grit fetch origin '+refs/heads/*:refs/remotes/origin/*'
  refs/remotes/origin/ag/bstr-migration  ← refs/heads/ag/bstr-migration  new → 4b88e08
  refs/remotes/origin/ag/changelog  ← refs/heads/ag/changelog  new → f3146f8
  …
  refs/remotes/origin/ag/snafu  ← refs/heads/ag/snafu  new → 7bf7ceb
Fetched 15 updates from origin.

  time      ···································█·▓▒·  1.4s
            · connect 1.3s  █ receiving 0ms  ▓ indexing 13ms  ▒ resolving 22ms  · finish 139ms
  data      201 KiB received
  objects   249 · 35 commits · 82 trees · 132 blobs

$ grit fetch --prune
  refs/remotes/origin/gone1  a1b2c3d → deleted
Fetched 1 update from origin.

$ grit fetch --dry-run --prune
  refs/remotes/origin/gone1  a1b2c3d → deleted
  refs/remotes/origin/feature  ← refs/heads/feature  new → b52cca6
Would fetch 2 updates from origin.
```

On a terminal, with progress on stderr above the summary (here filling in a shallow clone, which moves no refs):

```console
$ grit fetch --unshallow
Fetching origin https://github.com/BurntSushi/ripgrep.git
✓ Receiving  ▇▇▄▄▄▄▅▅▅▆▆▆▆▆▆▆▆█▇▇█          4.66 MiB          381 KiB/s   12.5s
✓ Indexing   ▆▆▆▆▆▆▆▆▆▆▆██████████    13,689 objects    82.2k objects/s   166ms
✓ Resolving  ▃▃▃▃▃▃▃▃▃▃▃██████████      9,877 deltas     49.1k deltas/s   201ms
Already up to date with origin.

  time      ·······████████████████████████████·▓▒··  18.1s
            · connect 3.0s  █ receiving 12.5s  ▓ indexing 166ms  ▒ resolving 201ms  · finish 2.1s
  data      4.66 MiB received · 381 KiB/s avg · 583 KiB/s peak
  objects   13,845 · 2,297 commits · 6,306 trees · 4,973 blobs · 269 tags
```

The rates are left out when the pack arrived in a single read, too fast to time. A fetch pack can be *thin*: grit completes it with the delta bases it already has, so `objects` can be a little higher than the `Indexing` count.

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
| `promisor_remotes` | array | When the server advertised promisor remotes: `name`, `url` (credentials removed), `accepted`, `filter` (advertised `partialCloneFilter` or `null`) and `rejected` (`policy_none`, `unknown_remote`, `url_mismatch`, `field_missing`, `field_mismatch`, `not_partial`, or `null` when accepted). Tokens are never shown. |
| `stored_fields` | array | When `promisor.storeFields` changed config: `remote` and `field` (`partialCloneFilter` or `token`), never the value. |
| `promisor_warnings` | array | Promisor config or advertised values that were ignored: `code` is `unknown_accept_value` (`promisor.acceptFromServer` is not a known policy, so nothing is accepted), `unknown_field` (a `promisor.checkFields` / `promisor.storeFields` entry is not a known field), `invalid_advertised_value` (an advertised field was not stored because its value is invalid) or `unusable_auto_filter` (`--blobs auto` cannot use an accepted remote's filter, such as `tree:0`); `key`, `remote`, `field` and `value` say which, when they apply. A rejected advertised value is never shown. Shown as `promisor  warning: …` lines in the human output and `promisor warning` bullets in `--markdown`. |

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
| `128` | Could not reach or read the remote: missing repository, DNS or connection failure, HTTP error status ([`remote_unreadable`](../exit-codes/)). |

See [Exit codes](../exit-codes/) for shared rules and structured `--json` `kind` values.

An HTTPS remote that wants credentials fails with `kind` `auth_required` (none available) or `auth_rejected` (refused). Credentials come from your credential helper, then an askpass program (`GIT_ASKPASS`, `core.askPass`, `SSH_ASKPASS`), as in Git; save a user name and token for any host with `grit auth <url> --username <user> --token <token>`. See [Credentials](../agents/#credentials-https).


## See also

[grit pull](../pull/), [grit merge](../merge/), [grit remote](../remote/)