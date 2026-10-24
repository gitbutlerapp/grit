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

It supports Git protocol versions 0, 1 and 2, chosen by the client through the `GIT_PROTOCOL` environment variable. Refs matching `uploadpack.hideRefs` or `transfer.hideRefs` are not shown to clients.

The pack is streamed to the client while it is produced, so the first bytes arrive after object enumeration rather than after the whole pack is built, and server memory stays bounded instead of growing with the pack. Objects already stored in the repository's packs are copied from them: whole objects byte for byte, and deltas whose base is also being sent with their base offset rewritten. Serving a clone of a packed repository therefore costs about as much as copying its pack. Only objects that are not already deltified go through the delta search, which follows `pack.window` and `pack.depth`.

Shallow and deepen requests (`deepen`, `deepen-since`, `deepen-not`, `deepen-relative`, client `shallow` lines) are honored in protocol v0/v1 and in v2 `fetch` (including the `shallow-info` section and sideband-all framing). Partial-clone filters (`filter blob:none`, `blob:limit=<n>`, `tree:<depth>`, and combinations) follow `uploadpack.allowFilter` and `uploadpackfilter.*` policy. Protocol v2 `want-ref` is available when `uploadpack.allowRefInWant` is set. Optional `want` rules follow `uploadpack.allowTipSha1InWant` and `uploadpack.allowReachableSha1InWant`.

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


## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Success ([`success`](../exit-codes/)). |
| `1` | Command failed ([`failure`](../exit-codes/)). |
| `2` | Invalid flags or arguments ([`usage`](../exit-codes/)). |

See [Exit codes](../exit-codes/) for shared rules and structured `--json` `kind` values.


## See also

[grit receive-pack](../receive-pack/), [grit clone](../clone/), [grit fetch](../fetch/)