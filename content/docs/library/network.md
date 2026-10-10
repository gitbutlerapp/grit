---
title: Network
summary: Remote dispatch, fetch, push, and ls-remote over every transport.
---

Network operations in grit-lib center on [`Remote`](rustdoc:grit_lib::remote::Remote): a typed URL ([`RemoteUrl`](rustdoc:grit_lib::remote::RemoteUrl)), configured fetch refspecs, and one dispatcher for **fetch**, **push**, and **list refs**. Wire bytes still flow through the same fetch/push engines as before; `Remote` picks the transport from the URL scheme.

## Remote URL and config

| Scheme / form | [`RemoteUrl`](rustdoc:grit_lib::remote::RemoteUrl) variant | Transport |
| --- | --- | --- |
| Path or `file://` | `Local` / `File` | [`fetch_local`](rustdoc:grit_lib::transfer::fetch_local) / [`push_local`](rustdoc:grit_lib::transfer::push_local) |
| `git://` | `Git` | [`GitDaemonTransport`](rustdoc:grit_lib::transport::GitDaemonTransport) |
| `ssh://`, scp-style | `Ssh` | [`SshTransport`](rustdoc:grit_lib::transport::SshTransport) (repo [`CommandRunner`](rustdoc:grit_lib::command_runner::CommandRunner)) |
| `http(s)://` | `Http` / `Https` | [`http_fetch`](rustdoc:grit_lib::transport::http::http_fetch) / [`push_http`](rustdoc:grit_lib::push::push_http) |

Parse a literal URL with [`RemoteUrl::try_from`](rustdoc:grit_lib::remote::RemoteUrl). Load `remote.<name>.url`, optional `pushurl`, and `fetch` refspecs (default `+refs/heads/*:refs/remotes/<name>/*`) via [`Remote::from_config`](rustdoc:grit_lib::remote::Remote). Config applies [`url_rewrite`](rustdoc:grit_lib::url_rewrite) `insteadOf` / `pushInsteadOf` rules the same way Git does.

Local paths resolve through [`resolve_local_remote_git_dir`](rustdoc:grit_lib::transport_path::resolve_local_remote_git_dir) from the repository root, not the process cwd.

## list refs

[`Remote::list_refs`](rustdoc:grit_lib::remote::Remote) takes [`ListRefsOptions`](rustdoc:grit_lib::remote::ListRefsOptions) (prefixes, heads, tags, symrefs, peel) and returns [`RemoteRef`](rustdoc:grit_lib::remote::RemoteRef) entries in `git ls-remote` order. On disk it uses [`list_refs_from_git_dir`](rustdoc:grit_lib::remote::list_refs_from_git_dir). Over the wire it uses protocol v2 `ls-refs` when available, otherwise the v0/v1 ref advertisement.

## Fetch and push

[`Remote::fetch`](rustdoc:grit_lib::remote::Remote) accepts [`FetchOptions`](rustdoc:grit_lib::transfer::FetchOptions) and a [`Progress`](rustdoc:grit_lib::fetch::Progress) sink; fetch negotiates protocol v2 when the server supports it. [`Remote::push`](rustdoc:grit_lib::remote::Remote) uses [`PushRefSpec`](rustdoc:grit_lib::transfer::PushRefSpec) and returns [`PushOutcome`](rustdoc:grit_lib::transfer::PushOutcome).

HTTP remotes need an [`HttpClient`](rustdoc:grit_lib::transport::http::HttpClient). Implement [`HttpClientFactory`](rustdoc:grit_lib::remote::HttpClientFactory) or, with the `http-ureq` feature, pass `None` to use the default ureq-backed factory (`UreqHttpClient` + [`HelperCredentialProvider`](rustdoc:grit_lib::credentials::HelperCredentialProvider)).

## Credentials and progress

[`HelperCredentialProvider`](rustdoc:grit_lib::credentials::HelperCredentialProvider) satisfies HTTP `401` responses from configured `credential.helper` programs and never opens a TTY. Pass [`NoProgress`](rustdoc:grit_lib::fetch::NoProgress) to ignore sideband progress, or implement [`Progress::message`](rustdoc:grit_lib::fetch::Progress) for side-band channel 2.

## Example

This example resolves `origin`, lists refs on a **local** bare remote, fetches, creates a commit reusing the fetched tip’s tree, and pushes — all over `file://`:

<!-- include: grit-examples/src/bin/guide_network.rs -->

The `grit-examples` crate also ships `gritx-fetch` and `gritx-push`, which dispatch on URL scheme and print transport/auth discovery lines. Shared wiring lives in `grit-examples/src/remote.rs` (slated to thin over [`Remote`](rustdoc:grit_lib::remote::Remote) in a follow-up step).

Integration test `guide_network` builds bare and consumer repos with system Git, runs the binary, then requires clean `git fsck --strict` on both sides and matching `git rev-parse` on the pushed ref.

## Bundles

Git’s [bundle format](https://git-scm.com/docs/gitformat-bundle) combines a text header (prerequisite commits, ref tips, optional v3 capabilities) with a thin packfile. [`grit_lib::bundle`](rustdoc:grit_lib::bundle) reads and writes that format for offline transfer and tests:

- [`Bundle`](rustdoc:grit_lib::bundle::Bundle) (`open`, `verify`, `unbundle`) and [`read_header`](rustdoc:grit_lib::bundle::read_header) parse v2/v3 headers and leave the stream at the `PACK` magic.
- `verify` checks prerequisite OIDs against the ODB and ref connectivity (matching `git bundle verify` semantics).
- `unbundle` ingests the pack via the index-pack path (`fix-thin`) and returns ref tips without updating refs.
- [`write_bundle`](rustdoc:grit_lib::bundle::write_bundle) builds v2 (SHA-1, no filter) or v3 bundles using [`transfer::build_pack`](rustdoc:grit_lib::transfer::build_pack) for the pack stream.

Integration test `bundle_git_compat` round-trips bundles with system `git bundle` (verify, list-heads, clone/fetch, `fsck --strict`).
