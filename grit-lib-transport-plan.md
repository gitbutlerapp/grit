# grit-lib network transports — phased implementation plan

Goal: give `grit-lib` reusable, embedder-shaped APIs for `git://`, `ssh`, and
`http(s)` fetch/push, a credential layer, and delta/thin packs — so jj and
GitButler can drop `gix` for real remotes.

## Framing: lift, don't rewrite

~10.5k lines of transport logic already exist in the **CLI crate** (`grit/src/`)
and pass the upstream C-git suite. They are CLI-shaped (argv, subprocess,
stdout). The work is extracting the reusable core into `grit-lib` behind a
`Transport` trait and a couple of pluggable seams. Building blocks already in
`grit-lib`: `pkt_line`, `fetch_negotiator::SkippingNegotiator`, `protocol`,
`protocol_v2`, `push_report`, `push_cert`, `delta_encode`, `delta_islands`,
`transfer` (in-process local fetch/push + whole-object `build_pack`).

Lift sources (verified file:line):
- Fetch negotiation: `grit/src/fetch_transport.rs`
  (`fetch_upload_pack_negotiate_pack_bytes_with_streams` ~2226, `read_advertisement`
  ~464, v2 path ~1525), `grit/src/file_upload_pack_v2.rs`
  (`write_v2_fetch_request` ~353, `read_v2_capability_block` ~145).
- git:// client: `grit/src/git_daemon_url.rs` (`connect_git_daemon_upload_pack`
  ~100, `fetch_via_git_protocol_skipping` ~2670). Server fixture:
  `grit/src/commands/daemon.rs::run_inetd` ~171; `grit/src/commands/upload_pack.rs`.
- ssh: `grit/src/ssh_transport.rs` (`parse_ssh_url` ~54, `spawn_git_ssh_service`
  ~698, GIT_SSH/GIT_SSH_COMMAND handling).
- http(s): `grit/src/http_smart.rs` (`http_ls_refs` ~637, `http_fetch_pack` ~1529),
  `grit/src/http_push_smart.rs` (`discover_receive_pack` ~127, `send_receive_pack`
  ~446), `grit/src/http_client.rs` (`HttpClientContext`, minimal surface =
  `get/post(+git_protocol header)`, `git_protocol_header`, `smart_http_enabled`).
  HTTP test fixture: the `grit-http-server` crate (axum, serves any repo).
- credentials: `grit/src/commands/credential.rs` (`invoke_helper` ~763,
  `credential_helpers` ~507), `credential_cache.rs`, `credential_store.rs`.
- delta/thin packs: `grit/src/commands/pack_objects.rs` (`optimize_blob_deltas`
  ~4811, `build_pack` ~5196), `grit/src/commands/send_pack.rs`
  (`run` ~89, `report_has_rejections` ~481, `demux_report_and_remote_messages` ~372).

## Phases (sequential; each compiles + tests before the next)

### Phase 1 — Transport trait + wire fetch + `git://`
- `grit_lib::transport::{Transport, Connection, Service, ConnectOptions}` — a
  `Connection` exposes the ref/cap advertisement and bidirectional pkt-line
  streams; `Transport::connect(url, service, opts) -> Connection`.
- `grit_lib::fetch::fetch_remote(repo, &FetchOptions, &mut dyn Connection, &mut dyn Progress) -> FetchOutcome`
  — lift the v0/v1 + v2 negotiation loop (SkippingNegotiator want/have/ACK →
  receive pack → `unpack_objects`/`index-pack` → ref updates). Reuse the
  `FetchOutcome` type from `transfer`.
- `GitDaemonTransport` (TCP + `git-upload-pack <path>\0host=…\0` request, lifted
  from `git_daemon_url.rs`).
- **Test:** start a git daemon (grit's `daemon --inetd` or system `git daemon`)
  on a temp repo; fetch via the trait; assert refs+objects; cross-check git.

### Phase 2 — `ssh` transport
- `grit_lib::transport::SshTransport` — lift URL parsing + GIT_SSH/GIT_SSH_COMMAND
  spawn; pluggable ssh command; pkt-line over child stdio. Reuse `fetch_remote`.
- **Test:** `GIT_SSH_COMMAND` → a script execing `grit upload-pack`; fetch over it.

### Phase 3 — push / send-pack over the wire
- `grit_lib::push::push_remote(repo, &PushOptions, &mut dyn Connection) -> PushOutcome`
  — read advertisement, write ref commands + caps, build pack (reuse
  `transfer::build_pack`), stream with sideband, parse `report-status(-v2)`
  (lift the report parsing/sideband demux). Reuse `push_report` types.
- **Test:** push over `git://` to a bare repo; verify with `git fsck`.

### Phase 4 — credential layer
- `grit_lib::credentials::{CredentialProvider, Credential}` + a Git-compatible
  default running `credential.helper` programs (lift `invoke_helper` /
  `credential_helpers`), with **typed non-interactive failure** (no TTY prompt).
- **Test:** a fake helper; fill/approve/reject; non-interactive failure is typed.

### Phase 5 — `http(s)` over a pluggable `HttpClient`
- `grit_lib::transport::http::{HttpClient, SmartHttpTransport}` — the smart-HTTP
  protocol (info/refs + stateless-rpc POST, v2 preamble, sideband) over the
  minimal `HttpClient` trait. Default `ureq` impl behind a `http-ureq` feature.
  Wire `CredentialProvider` for 401/basic auth.
- **Test:** run `grit-http-server` on a temp repo; fetch + push over http;
  cross-check git.

### Phase 6 — thin / delta packs
- Extend `transfer::build_pack` (and `PackBuildOptions`) with delta (call
  `delta_encode` + a simplified `optimize_blob_deltas`) and thin (omit bases the
  peer advertised). Wire into `push_remote`.
- **Test:** thin pack omits receiver-held bases; delta pack is smaller and still
  re-indexes + `fsck`s clean.

### Phase 7 — verify + (proof) jj wiring
- Whole-workspace build; run all new tests; re-point the relevant CLI commands at
  the new lib APIs where clean (validated by the existing suite). Optional proof:
  route jj's `git://` remote fetch through `grit_lib::transport::GitDaemonTransport`.

## Up-front design decisions
- **Pluggable HTTP client** (don't force ureq/TLS on embedders) — the biggest call.
- **Pluggable credentials** and **pluggable/optional ssh command**.
- Public API = structured `FetchOutcome`/`PushOutcome` + `Progress`/`CredentialProvider`
  traits; transport *impls* legitimately do socket/subprocess/HTTP I/O.
- Thread sha256/object-format awareness (already in the lib paths).
- Mirror gix result shapes so embedder adapters stay thin.

## Sequencing rationale
Trait + `git://` first (smallest end-to-end proof, unblocks the spike's git://
canaries) → ssh (self-contained) → push → credentials → http (largest) → delta/
thin (perf, independent) → verify/jj. Phases 1–4 are moderate, mostly-mechanical
lifts; phase 5–6 are the larger, higher-risk half.
