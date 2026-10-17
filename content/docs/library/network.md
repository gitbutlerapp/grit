---
title: Network
summary: Remote dispatch, fetch, push, and ls-remote over every transport.
---

Network operations in grit-lib center on `Remote`: a typed URL (`RemoteUrl`), configured fetch refspecs, and one dispatcher for **fetch**, **push**, and **list refs**. Wire bytes still flow through the same fetch/push engines as before; `Remote` picks the transport from the URL scheme.

## Remote URL and config

| Scheme / form | `RemoteUrl` variant | Transport |
| --- | --- | --- |
| Path or `file://` | `Local` / `File` | `fetch_local` / `push_local` |
| `git://` | `Git` | `GitDaemonTransport` |
| `ssh://`, scp-style | `Ssh` | `SshTransport` (repo `CommandRunner`) |
| `http(s)://` | `Http` / `Https` | `http_fetch` / `push_http` |

Parse a literal URL with `RemoteUrl::try_from`. Load `remote.<name>.url`, optional `pushurl`, and `fetch` refspecs (default `+refs/heads/*:refs/remotes/<name>/*`) via `Remote::from_config`. Config applies `insteadOf` / `pushInsteadOf` URL rewrite rules the same way Git does.

Local paths resolve through `resolve_local_remote_git_dir` from the repository root, not the process cwd.

## list refs

`Remote::list_refs` takes `ListRefsOptions` (prefixes, heads, tags, symrefs, peel) and returns `RemoteRef` entries in `git ls-remote` order. On disk it uses `list_refs_from_git_dir`. Over the wire it uses protocol v2 `ls-refs` when available, otherwise the v0/v1 ref advertisement.

## Fetch and push

`Remote::fetch` accepts `FetchOptions` and a `Progress` sink; fetch negotiates protocol v2 when the server supports it. `Remote::push` uses `PushRefSpec` and returns `PushOutcome`.

HTTP remotes need an `HttpClient`. Implement `HttpClientFactory` or, with the `http-ureq` feature, pass `None` to use the default ureq-backed factory (`UreqHttpClient` + `HelperCredentialProvider`).

## Credentials and progress

`HelperCredentialProvider` satisfies HTTP `401` responses from configured `credential.helper` programs and never opens a TTY. Pass `NoProgress` to ignore sideband progress, or implement `Progress::message` for side-band channel 2. Set `FetchOptions` `remote_progress` to omit `no-progress` on the wire so the remote may emit channel 2; decode chunks with `SidebandLines` into `RemoteLine` updates and lines.

## Example

This example resolves `origin`, lists refs on a **local** bare remote, fetches, creates a commit reusing the fetched tip’s tree, and pushes — all over `file://`:

<!-- include: grit-examples/src/bin/guide_network.rs -->

The `grit-examples` crate also ships `gritx-fetch` and `gritx-push`, which dispatch on URL scheme and print transport/auth discovery lines. Shared wiring lives in `grit-examples/src/remote.rs` (slated to thin over `Remote` in a follow-up step).

Integration test `guide_network` builds bare and consumer repos with system Git, runs the binary, then requires clean `git fsck --strict` on both sides and matching `git rev-parse` on the pushed ref.

## Bundles

Offline bundle files are documented on the [Bundle](../bundle/) guide (`RepositoryBundles`). Smart HTTP and local remotes above cover online fetch/push.
