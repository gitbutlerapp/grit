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
| `receive.denyNonFastForwards` | Refuse updates that would discard commits from a branch. |
| `receive.denyDeletes` | Refuse to delete refs. |
| `receive.denyCurrentBranch` | Refuse to update the branch checked out in a non-bare repository. On by default; set it to `ignore` or `warn` to allow it. |
| `receive.hideRefs`, `transfer.hideRefs` | Hide refs from pushing clients and refuse updates to them. |

Pushes that ask for an atomic update are applied all or nothing. Server-side hooks are not supported yet.

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

## See also

[grit upload-pack](../upload-pack/), [grit push](../push/)
