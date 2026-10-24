---
title: grit remote
summary: List remotes, add one, show one, set URLs, remove one, or list refs on a remote.
group: Remotes
order: 1
---

## Synopsis

```text
grit remote [-v]
grit remote add <name> <url> [--fetch]
grit remote set-url <name> <url> [--push] [--dry-run]
grit remote remove <name> [--dry-run]
grit remote prune <name> [--dry-run]
grit remote set-head <name> (--auto | <branch> | --delete) [--dry-run]
grit remote show <name> [--no-query] [-n N]
grit remote refs <remote-or-url> [--heads] [--tags] [--sort name|version] [--reverse] [-n N] [<prefix>…]
```

## Description

A remote is another copy of the repository that you fetch from and push to, usually on a server. With no arguments, `grit remote` lists your remotes and their URLs, sorted by name. Like `git remote`, the list includes a remote that has config but no URL, shown as `(no url)`.

`grit remote add` adds a remote. It writes `remote.<name>.url` and the fetch refspec `+refs/heads/*:refs/remotes/<name>/*` to the repository's shared config, also when run from a linked worktree. It fails if a remote with that name is already configured, or if the name could not be used in a ref such as `refs/remotes/<name>/main` (for example names with spaces, `..` or `:`). With `--fetch` (long only; `-f` means force elsewhere in grit), it runs [`grit fetch`](../fetch/) for that remote immediately after adding it. [`grit fetch`](../fetch/) stores the remote's branches as remote-tracking branches named `<name>/<branch>`, such as `origin/main`. [`grit clone`](../clone/) adds a remote called `origin` for you.

`grit remote prune` deletes stale remote-tracking refs for one remote without fetching: it lists refs on the remote, compares them to local tracking refs under that remote's fetch refspecs (same rules as `git fetch --prune`), and removes refs whose upstream branch is gone. `refs/remotes/<name>/HEAD` is never pruned. `--dry-run` lists refs that would be removed without deleting them.

`grit remote set-head` updates or deletes the `refs/remotes/<name>/HEAD` symref. `--auto` queries the remote default branch and points HEAD at `refs/remotes/<name>/<branch>` when that tracking ref exists locally. A branch argument sets HEAD to that branch. `--delete` removes the symref. `--dry-run` reports the change without updating refs.

`-v` / `--verbose` lists fetch and push URLs on separate lines (`name url (fetch)` and `name url (push)`), like `git remote -v`. With `--json`, each remote also includes `fetch_url` and `push_url` after `url.*.insteadOf` / `pushInsteadOf` rewrites, plus `refspecs`.

`grit remote set-url` changes the fetch URL (`remote.<name>.url`) or, with `--push`, replaces every `remote.<name>.pushurl` with a single new push URL. It fails when the remote is not configured. `--dry-run` reports the change without writing config.

`grit remote remove` deletes a configured remote: it removes the `remote.<name>` section from the shared config, deletes every remote-tracking ref this remote's fetch refspecs map to (including `refs/remotes/<name>/HEAD`), and clears `branch.<b>.remote`, `branch.<b>.merge`, and matching `branch.<b>.pushRemote` / `remote.pushDefault` keys. Tracking refs that another remote's fetch refspec also maps to are kept. `--dry-run` reports what would change without writing config or refs. There is no `rm` alias.

`grit remote show` summarizes one configured remote: fetch and push URLs, the remote `HEAD` branch, each remote branch classified as tracked, new, or stale (when the remote is queried), and local branches whose `branch.<name>.remote` points at this remote. By default it contacts the remote (same transports as [`grit fetch`](../fetch/)). With `--no-query`, branch rows come from local remote-tracking refs only and every branch is reported as tracked.

`grit remote refs` lists references on a configured remote name or a literal URL, scp-style `host:path` or path (same transports as [`grit clone`](../clone/)), matching `git ls-remote` ordering. An argument that is none of these fails with `unknown remote`. SSH URLs use the same `GIT_SSH_COMMAND`, `GIT_SSH`, and `core.sshCommand` resolution as [`grit clone`](../clone/) and [`grit fetch`](../fetch/).

## Options

| Option | Description |
| --- | --- |
| `-v`, `--verbose` | With no subcommand, show fetch and push URLs (like `git remote -v`). |
| `add <name> <url>` | Add a remote called `<name>` at `<url>`. Fails if a remote with that name exists or the name is not valid in a ref. |
| `--fetch` | With `add`, fetch from the new remote after writing config. |
| `prune <name>` | Delete stale remote-tracking refs for `<name>`. `--dry-run` previews without deleting. |
| `set-head <name> …` | Set or delete `refs/remotes/<name>/HEAD` (`--auto`, a branch name, or `--delete`). |
| `--auto` | With `set-head`, follow the remote `HEAD` symref. |
| `--delete` | With `set-head`, remove the local HEAD symref. |
| `set-url <name> <url>` | Set the fetch URL for `<name>`. Use `--push` for `pushurl`. |
| `--push` | With `set-url`, update push URL(s) instead of fetch. |
| `remove <name>` | Remove remote `<name>` and its tracking refs. `--dry-run` previews without writing. |
| `show <name>` | Summarize `<name>` (URLs, HEAD branch, remote branches, pull config). |
| `--no-query` | With `show`, use local remote-tracking refs only; do not contact the remote. |
| `refs <remote-or-url>` | List refs on the remote (or URL). |
| `--heads` | With `refs`, only `refs/heads/`. |
| `--tags` | With `refs`, only `refs/tags/`. |
| `--sort` | With `refs`, `name` (default) or `version` (`git ls-remote --sort=version:refname`). |
| `--reverse` | With `refs`, reverse `--sort` order. |
| `-n`, `--limit` | With `show` or `refs`, return at most `N` branch or ref lines after sorting. |
| `<prefix>…` | With `refs`, optional ref prefixes (same rules as `git ls-remote`). |

## Examples

```console
$ grit remote add origin https://github.com/ada/project.git
Added remote origin → https://github.com/ada/project.git

$ grit remote
origin	https://github.com/ada/project.git

$ grit remote -v
origin	https://github.com/ada/project.git (fetch)
origin	https://github.com/ada/project.git (push)

$ grit remote set-url origin git@github.com:ada/project.git
Set remote origin fetch URL to git@github.com:ada/project.git

$ grit remote show origin
* remote origin
  Fetch URL: https://github.com/ada/project.git
  Push  URL: https://github.com/ada/project.git
  HEAD branch: main
  Remote branches:
    main tracked
    feature new
  Local branches configured for pull:
    main merges with remote main

$ grit remote refs origin
a1b2c3d4e5f6789012345678901234567890abcd	refs/heads/main
```

## JSON output

Schema: [v1](/schemas/remote/v1.json) · [remote.add v1](/schemas/remote.add/v1.json) · [remote.remove v1](/schemas/remote.remove/v1.json) · [remote.show v1](/schemas/remote.show/v1.json) · [remote.refs v1](/schemas/remote.refs/v1.json) (set-url uses the main remote schema)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `action` | string | `list`, `add`, `set_url`, `remove`, `prune`, `set_head`, `show`, or `refs`. |
| `remotes` | array | For `list`, each remote with `name` and `url`; with `-v`, also `fetch_url`, `push_url`, and `refspecs`. |
| `name` | string | For `add`, `set_url`, `remove`, or `show`, the remote name. |
| `url` | string | For `add` or `set_url`, the remote URL or path. |
| `fetched` | boolean | For `add`, whether a fetch ran after adding (`--fetch`). |
| `refs_pruned` | array | For `prune`, full ref names deleted or that would be deleted. |
| `target` | string or null | For `set_head`, new symref target, or null when deleted. |
| `previous_target` | string or null | For `set_head`, symref target before the update, if any. |
| `kind` | string | For `set_url`, `fetch` or `push`. |
| `old_url` | string or null | For `set_url`, previous URL for that kind, if any. |
| `refs_deleted` | array | For `remove`, deleted remote-tracking ref names. |
| `branches_untracked` | array | For `remove`, local branch short names whose upstream keys were cleared. |
| `dry_run` | boolean | For `set_url` or `remove`, whether changes were only reported. |
| `fetch_url` | string | For `show`, fetch URL after config rewrite. |
| `push_url` | string | For `show`, push URL (`pushurl` when set). |
| `head_branch` | string or null | For `show`, remote default branch when known. |
| `branches` | array | For `show`, each `{ name, state }` with `state` `tracked`, `new`, or `stale`. |
| `pull` | array | For `show`, each `{ local, merges }` from `branch.*` config. |
| `refs` | array | For `refs`, each entry with `name`, `oid`, optional `peeled`, optional `symref_target`. |

Listing remotes:

```json
{
  "action": "list",
  "remotes": [
    {
      "name": "origin",
      "url": "https://github.com/ada/project.git"
    }
  ]
}
```

Adding a remote:

```json
{
  "action": "add",
  "name": "origin",
  "url": "https://github.com/ada/project.git",
  "fetched": false
}
```

Showing a remote:

```json
{
  "action": "show",
  "name": "origin",
  "fetch_url": "https://github.com/ada/project.git",
  "push_url": "https://github.com/ada/project.git",
  "head_branch": "main",
  "branches": [
    { "name": "main", "state": "tracked" },
    { "name": "feature", "state": "new" }
  ],
  "pull": [
    { "local": "main", "merges": "refs/heads/main" }
  ]
}
```

Pruning stale tracking refs:

```json
{
  "action": "prune",
  "name": "origin",
  "refs_pruned": ["refs/remotes/origin/topic"],
  "dry_run": false
}
```

Setting remote HEAD:

```json
{
  "action": "set_head",
  "name": "origin",
  "target": "refs/remotes/origin/main",
  "previous_target": null,
  "dry_run": false
}
```

Setting a URL:

```json
{
  "action": "set_url",
  "name": "origin",
  "kind": "fetch",
  "old_url": "https://github.com/ada/old.git",
  "url": "https://github.com/ada/project.git",
  "dry_run": false
}
```

Removing a remote:

```json
{
  "action": "remove",
  "name": "origin",
  "refs_deleted": ["refs/remotes/origin/HEAD", "refs/remotes/origin/main"],
  "branches_untracked": ["main"],
  "dry_run": false
}
```

Listing refs:

```json
{
  "action": "refs",
  "refs": [
    {
      "name": "refs/heads/main",
      "oid": "a1b2c3d4e5f6789012345678901234567890abcd",
      "peeled": null,
      "symref_target": null
    }
  ]
}
```

## Markdown output

Schema: [v1](/schemas/remote/v1.json)
Pass `--markdown` for an agent-friendly Markdown table of refs (a “Remote refs” heading, then columns — not JSON). Example shape:

```text
Remote refs

| Ref | Object | Peeled | Symref target |
| --- | --- | --- | --- |
| `refs/heads/main` | `a1b2c3…` | — | — |
| `refs/tags/v1` | `tagoid…` | `commitoid…` | — |
```


## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Success ([`success`](../exit-codes/)). |
| `1` | Command failed ([`failure`](../exit-codes/)). |
| `2` | Invalid flags or arguments ([`usage`](../exit-codes/)). |

See [Exit codes](../exit-codes/) for shared rules and structured `--json` `kind` values.


## See also

[grit fetch](../fetch/), [grit push](../push/), [grit clone](../clone/)