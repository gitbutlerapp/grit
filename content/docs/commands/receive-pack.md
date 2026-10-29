---
title: grit receive-pack
summary: Accept a push to a repository over stdin and stdout.
group: Plumbing
order: 2
---

## Synopsis

```text
grit receive-pack [--stateless-rpc] [--advertise-refs] <directory>
```

## Description

The server side of push. It speaks the Git wire protocol on stdin and stdout, receives the objects a client sends, and updates the repository's refs. You don't normally run it yourself: an SSH server runs it when a client pushes, and `grit-http-server` answers smart HTTP requests by calling the same logic in `grit-lib` in-process (no separate `grit` binary next to the server). It doesn't appear in `grit --help`.

`<directory>` is the repository to push to: a bare repository, a working tree, or a path that names one once `.git` is added. It doesn't search parent directories.

Every ref update is checked before it's applied, and the client is told which updates were accepted and why any were refused. These settings in the served repository's config control what's allowed:

| Setting | Effect |
| --- | --- |
| `receive.denyNonFastForwards` | Refuse updates that would discard commits from a branch (`refs/heads/*`). Fast-forwards are accepted, and other refs, such as tags, are not checked. |
| `receive.denyDeletes` | Refuse to delete refs. |
| `receive.denyCurrentBranch` | Refuse to update the branch checked out in a non-bare repository. On by default; set it to `ignore` or `warn` to allow it. |
| `receive.denyDeleteCurrent` | Refuse to delete the branch `HEAD` points at, in a bare repository too. On by default; set it to `ignore` or `warn` to allow it. |
| `receive.hideRefs`, `transfer.hideRefs` | Hide refs from pushing clients and refuse updates to them. |
| `receive.maxInputSize` | Refuse pushes whose pack is larger than this many bytes. Unlimited by default. |
| `receive.unpackLimit`, `transfer.unpackLimit` | Store pushes with fewer objects than this as loose objects; keep larger ones as a pack with an index. 100 by default. |
| `receive.fsckObjects` | Check every pushed commit, tree and tag before any of them is stored. One malformed object refuses the whole push: the client sees `[remote rejected] ... (unpacker error)`, no ref changes, and nothing from the push enters the repository. Off by default. |
| `transfer.fsckObjects` | Used when `receive.fsckObjects` is unset. |
| `receive.fsck.<msg-id>` | Set one check to `error`, `warn` or `ignore`, for example `receive.fsck.missingEmail=warn`. The check after an accepted problem still runs, so ignoring one problem doesn't let another through. `nulInHeader` and `unterminatedHeader` are always errors. |
| `receive.fsck.skipList` | A file of object ids, one per line (`#` starts a comment), that are never checked. A relative path is relative to the repository's git directory. If the file can't be read, every push is refused. |
| `receive.shallowUpdate` | Accept pushes from a shallow clone that would make this repository shallow, and record the client's boundaries in its `shallow` file. Off by default: such an update is refused with `[remote rejected] ... (shallow update not allowed)`. A shallow clone pushing onto history this repository already has never needs it. |

The pushed pack is streamed to a temp file in a quarantine directory inside the repository's object store, so memory use stays flat however large the push is. Its objects become visible in the repository only after the `pre-receive` hook accepts the push.

Every pushed history is checked to be complete, as `git receive-pack` checks it (`rev-list --objects <new tips> --not --all`), however the push was stored: unpacked into loose objects below `receive.unpackLimit`, installed as a pack above it, or with no objects at all. Each new ref tip is walked down to what the repository's refs and `HEAD` already reach, and every object on the way must be there. An object the repository has is not enough by itself: a ref refused for missing objects leaves the objects that did arrive behind (the push is stored for the refs that were accepted, as Git stores it), and a later push naming that commit again is refused too. The commit walk paints the new tips against the commits the refs point at, newest first, using the commit-graph where there is one, and stops once no queued commit is new; each new commit's tree is then compared with its parents' trees, and only the entries that differ are followed. So the check costs about as much as the history the push adds, not the repository's. A ref whose tip reaches a missing object is refused with `[remote rejected] ... (missing necessary objects)`, as Git refuses it, and the other refs of the push are applied as usual. Objects a promisor remote of the repository promises (blobs offloaded with [`grit gc offload`](../gc/#grit-gc-offload), say) may be missing, and so may the parents of the pushing client's shallow boundaries and of the repository's own.

There is no other limit on push size. `grit-http-server` streams a push body straight into receive-pack instead of buffering it, so a push of any size works over smart HTTP, as with `git http-backend`. Fetch requests, which are small, are capped at 32 MiB (after gzip decoding). Git doesn't gzip push bodies, but if another client does, the decoded body may reach 32 MiB and beyond that at most 32 times the compressed bytes received; a body that expands further is refused with `413`, and one that isn't valid gzip with `400`.

In a repository with [snapshot checkouts](../checkout/) (once [`grit serve`](../serve/#snapshot-checkouts) has made one), pushes that create, update or delete the target branch, the one `HEAD` names, are refused whatever these settings say, as `grit serve` refuses them: that branch changes only through [`grit integrate`](../integrate/) and [`grit slice`](../slice/). The refused client is told to push another branch and integrate it.

Pushes from a shallow clone work, such as a CI job that cloned with `--depth 1` and pushes a new branch back. Git sends the clone's shallow boundaries along with the push. When this repository already has the history below them, the push is applied as usual and the repository stays complete.

Pushes that ask for an atomic update are applied all or nothing. The served repository's `pre-receive`, `update`, `post-receive` and `post-update` hooks run as they do under Git: from the repository directory with `GIT_DIR=.`, with the quarantine variables during `pre-receive`, and with push options in `GIT_PUSH_OPTION_*`. Under `receive.shallowUpdate`, a push from a shallow clone also sets `GIT_SHALLOW_FILE` to a file listing the client's boundaries, so hooks can walk the pushed history.

Pushed objects are checked as strictly as `git receive-pack` checks them: problems a local `git fsck` only warns about, such as zero-padded file modes or `.git` tree entries, refuse the push unless `receive.fsck.<msg-id>` says otherwise. Problems that are only informational, such as `badFilemode`, are accepted.

## Options

| Option | Description |
| --- | --- |
| `<directory>` | The repository to push to. |
| `--stateless-rpc` | Answer a single request and exit, without advertising refs first. Used for smart HTTP. |
| `--advertise-refs` | Print the ref advertisement and exit. Used for the first request of smart HTTP. Also accepted as `--http-backend-info-refs`. |

## Examples

Push over SSH to a server that has `grit` but not `git` installed:

```console
$ git push --receive-pack='grit receive-pack' origin main
```

Set it for a remote you already have:

```console
$ git config remote.origin.receivepack 'grit receive-pack'
```

## JSON output

None. `--json` has no effect, since stdout carries the Git protocol.


## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Success ([`success`](../exit-codes/)). |
| `1` | Command failed ([`failure`](../exit-codes/)). |
| `2` | Invalid flags or arguments ([`usage`](../exit-codes/)). |

See [Exit codes](../exit-codes/) for shared rules and structured `--json` `kind` values.


## See also

[grit upload-pack](../upload-pack/), [grit push](../push/)