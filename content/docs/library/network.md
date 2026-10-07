---
title: Network
summary: ls-remote, fetch and push over file:// and smart HTTP, credentials, and progress.
---

Network operations in grit-lib are **typed**: you pass option structs, get outcome structs, and wire a [`Progress`](rustdoc:grit_lib::fetch::Progress) sink for sideband messages. No subprocess `git` on the wire — local, `git://`, SSH, and smart HTTP share the same fetch/push engines.

## Transport matrix

| URL | Fetch | Push | Authentication |
| --- | --- | --- | --- |
| `file://` or path | [`fetch_local`](rustdoc:grit_lib::transfer::fetch_local) | [`push_local`](rustdoc:grit_lib::transfer::push_local) | none |
| `git://` | [`fetch_remote`](rustdoc:grit_lib::fetch::fetch_remote) over [`GitDaemonTransport`](rustdoc:grit_lib::transport::GitDaemonTransport) | [`push_remote`](rustdoc:grit_lib::push::push_remote) | none |
| `ssh` | same over [`SshTransport`](rustdoc:grit_lib::transport::SshTransport) | same | SSH keys/agent |
| `http(s)` | [`http_fetch`](rustdoc:grit_lib::transport::http::http_fetch) | [`push_http`](rustdoc:grit_lib::push::push_http) | [`CredentialProvider`](rustdoc:grit_lib::credentials::CredentialProvider) |

Wire transports use [`Transport::connect`](rustdoc:grit_lib::transport::Transport) with [`Service::UploadPack`](rustdoc:grit_lib::transport::Service) (fetch) or `ReceivePack` (push). Fetch negotiates protocol v2 when possible; push uses v0/v1.

Local remotes resolve through [`resolve_local_remote_git_dir`](rustdoc:grit_lib::transport_path::resolve_local_remote_git_dir) so relative `remote.*.url` values behave like Git (repository root, not process cwd).

## ls-remote

For a **local** git directory (bare repo or `.git`), [`ls_remote`](rustdoc:grit_lib::ls_remote::ls_remote) lists refs with the same ordering and peeling rules as `git ls-remote` on a filesystem remote. Pass [`ls_remote::Options`](rustdoc:grit_lib::ls_remote::Options) to filter heads, tags, or patterns. Remote listing over HTTP/SSH is driven by fetch negotiation; local enumeration is the usual first step for `file://` tests and tools.

## Fetch and push inputs

[`FetchOptions`](rustdoc:grit_lib::transfer::FetchOptions) carries refspecs, tag mode ([`TagMode`](rustdoc:grit_lib::transfer::TagMode)), prune, shallow depth, and related flags. [`FetchOutcome`](rustdoc:grit_lib::transfer::FetchOutcome) reports ref updates and optional default-branch hints.

Push uses [`PushRefSpec`](rustdoc:grit_lib::transfer::PushRefSpec) entries (source oid, destination ref, force, lease fields) and returns [`PushOutcome`](rustdoc:grit_lib::transfer::PushOutcome) with per-ref [`PushRefStatus`](rustdoc:grit_lib::push_report::PushRefStatus).

## Credentials and progress

HTTP smart transport accepts an [`HttpClient`](rustdoc:grit_lib::transport::http::HttpClient) implementation. With the `http-ureq` feature, `UreqHttpClient::from_config` honors `http.proxy`, cookies, and extra headers from [`ConfigSet`](rustdoc:grit_lib::config::ConfigSet). [`SmartHttpTransport`](rustdoc:grit_lib::transport::http::SmartHttpTransport) wraps any client for combined fetch/push entry points.

[`HelperCredentialProvider`](rustdoc:grit_lib::credentials::HelperCredentialProvider) runs configured `credential.helper` programs on `401` and retries with HTTP Basic. It **never** opens a TTY — missing credentials surface as [`Error::Auth`](rustdoc:grit_lib::error::Error).

Pass [`NoProgress`](rustdoc:grit_lib::fetch::NoProgress) to ignore sideband progress, or implement [`Progress::message`](rustdoc:grit_lib::fetch::Progress) to receive raw progress bytes from side-band channel 2.

## Example

This example resolves `origin`, lists refs on a **local** bare remote, fetches, creates a commit reusing the fetched tip’s tree, and pushes — all over `file://`:

<!-- include: grit-examples/src/bin/guide_network.rs -->

The `grit-examples` crate also ships `gritx-fetch` and `gritx-push`, which dispatch on URL scheme and print transport/auth discovery lines. Shared wiring lives in `grit-examples/src/remote.rs`.

Integration test `guide_network` builds bare and consumer repos with system Git, runs the binary, then requires clean `git fsck --strict` on both sides and matching `git rev-parse` on the pushed ref.
