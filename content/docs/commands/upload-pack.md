---
title: grit upload-pack
summary: Serve a fetch or clone of a repository over stdin and stdout.
group: Plumbing
order: 1
---

## Synopsis

```text
grit upload-pack [--stateless-rpc] [--advertise-refs] <directory>
```

## Description

The server side of fetch and clone. It speaks the Git wire protocol on stdin and stdout, so any Git client, `git` or `grit`, can fetch from a repository it serves. You don't normally run it yourself: an SSH server runs it when a client connects, and `grit-http-server` answers smart HTTP requests by calling the same logic in `grit-lib` in-process (no separate `grit` binary next to the server). It doesn't appear in `grit --help`.

`<directory>` is the repository to serve: a bare repository, a working tree, or a path that names one once `.git` is added. Unlike other commands, `grit upload-pack` doesn't search parent directories, so it serves exactly the repository it was given.

It supports Git protocol versions 0, 1 and 2, chosen by the client through the `GIT_PROTOCOL` environment variable. Refs matching `uploadpack.hideRefs` or `transfer.hideRefs` are not shown to clients. Shallow clones and partial-clone filters are not supported yet.

## Options

| Option | Description |
| --- | --- |
| `<directory>` | The repository to serve. |
| `--stateless-rpc` | Answer a single request and exit, without advertising refs first. Used for smart HTTP. |
| `--advertise-refs` | Print the ref advertisement and exit. Used for the first request of smart HTTP. Also accepted as `--http-backend-info-refs`. |

## Examples

Clone over SSH from a server that has `grit` but not `git` installed:

```console
$ git clone --upload-pack='grit upload-pack' ssh://example.com/srv/git/project.git
```

Set it for a remote you already have:

```console
$ git config remote.origin.uploadpack 'grit upload-pack'
```

## JSON output

None. `--json` has no effect, since stdout carries the Git protocol.

## See also

[grit receive-pack](../receive-pack/), [grit clone](../clone/), [grit fetch](../fetch/)
