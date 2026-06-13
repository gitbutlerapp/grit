# grit-lib networking APIs for the jj integration (replacing gix)

Plan to make `grit-lib` able to back jj's **fetch / push** and the other native
Git replacement paths in [jj PR #9632](https://github.com/jj-vcs/jj/pull/9632),
so jj can drop `gix` for transport.

## What PR #9632 actually showed

The spike (a Codex experiment, explicitly not merge-ready) converted jj's
`git fetch`/`git push`/`git gc` subprocess flows to in-process code. But it is
**not** a grit-lib migration: grit-lib was used only for thin, pure pieces —

- `grit_lib::config::ConfigSet` (read remote config)
- `grit_lib::refspec::valid_{fetch,push}_refspec` (validation)
- `grit_lib::refs::{list_refs, delete_ref}` (branch prune)
- `grit_lib::ls_remote` (local ref listing)
- `grit_lib::wildmatch`, `grit_lib::repo::Repository::open`

…and **everything transport-shaped stayed on `gix`**: `gix::remote::connect`/
`prepare_fetch`/`receive`, `gix::protocol::handshake`, `gix::credentials`,
`gix_pack::data::output` (pack build), `gix::refs::transaction` (ref updates),
plus the receive-pack/send-pack framing.

Reviewer asks (joshka, via Codex; echoed by schacon) crystallize the gap:

> The killer integration point for jj would be `grit-lib::fetch()` and
> `grit-lib::push()` APIs that own Git-compatible credential, negotiation, pack,
> and status behavior end to end.

Two findings from the spike set hard requirements:

1. **Pack-everything is fatal.** The naive packer walked every reachable object
   from the pushed tip — **87,752 objects, a 478 MB pack** — before being
   killed. Push needs negotiation / remote-object exclusion / thin packs.
2. **Credentials must not assume a TTY.** gix fell through to an interactive
   username prompt (missed the system `credential.helper=osxkeychain`); library
   code must use configured helpers and fail clearly when non-interactive.

## Root cause (grit-lib inventory)

grit-lib already ships the *building blocks*, but the *orchestration* lives only
in the **CLI crate** (`grit/src/…`), shaped around argv, subprocesses, stdout,
and on-disk files. So an embedder can't call it.

Already reusable in grit-lib (keep, build on):
- `pkt_line.rs` — wire codec (read/write lines, flush/delim, sideband encode/decode).
- `refspec.rs` — fetch/push refspec parse + match.
- `protocol.rs` / `protocol_v2.rs` — protocol policy + v2 capability parsing.
- `fetch_negotiator.rs::SkippingNegotiator` — the negotiation state machine.
- `push_report.rs` — `PushRefStatus` / `PushRefResult` (push result model).
- `push_cert.rs` — signed-push cert build/verify.
- `ls_remote.rs` — **local-only** ref enumeration.
- `transport_path.rs` — URL/path safety + clone-dir naming.
- `unpack_objects` / `pack.rs` (read side) — pack ingest.

Trapped in the CLI crate (must be lifted into grit-lib):
- `grit/src/fetch_transport.rs` (~3.1k lines) — the real fetch negotiation loop.
- `grit/src/http_smart.rs`, `http_client.rs`, `ssh_transport.rs`,
  `ext_transport.rs`, `file_upload_pack_v2.rs`, `http_push_smart.rs` — every
  transport, all CLI-internal, no shared trait.
- `grit/src/commands/{fetch,push,send_pack,receive_pack,upload_pack,pack_objects}.rs`
  — high-level orchestration, ref/tracking updates, **the real pack builder
  (delta + thin)**.
- `grit/src/commands/credential*.rs` — credential helper protocol, cache, store.

## Strategy: lift, don't reimplement

The transport/negotiation/pack code already passes the upstream C-git test
suite. The work is mostly a **refactor**: extract the reusable core out of the
CLI command shells into grit-lib behind embedder-friendly signatures (structured
inputs/outputs; progress via callbacks; errors as typed enums; **no process /
stdout / file-on-disk assumptions**). Then re-point the CLI commands at the new
lib APIs so the existing test suite validates the lift (refactor, don't fork).

This also keeps the freshly-landed **sha256 awareness** flowing through the new
pack/transport paths — the lib functions already thread `HashAlgo`.

## Workstreams

### 1. Transport abstraction (foundation)
Define a `grit_lib::transport::Transport` trait: given a URL + service
(`upload-pack` / `receive-pack`), open a connection and expose bidirectional
pkt-line streams plus negotiated protocol version and capabilities.
- Lift implementations: `file://` (`file_upload_pack_v2.rs`), `git://` (daemon
  connect), `ssh` (`ssh_transport.rs`), `http(s)` (`http_smart.rs` +
  `http_client.rs`), `ext::` (`ext_transport.rs`).
- Make it object-safe/pluggable so jj and GitButler can inject their own
  transport (e.g. their existing SSH/auth stack) without forking grit-lib.
- Gate with the existing `protocol.rs` policy and `transport_path.rs` checks.

### 2. Fetch API
`grit_lib::fetch::fetch(repo, remote, FetchOptions) -> Result<FetchOutcome>`.
- `FetchOptions`: refspecs (+ negative), tags mode (All / None / Following),
  shallow (`depth` / `deepen` / `since` / `exclude` / unshallow), prune,
  dry-run, atomic, a `Progress` callback trait, and a `CredentialProvider`.
- Drive: handshake → remote ref advertisement → ref-map (refspec→wanted) →
  `SkippingNegotiator` → receive pack → `index-pack`/`unpack-objects` → compute
  ref updates. Reuse `refspec`, `protocol_v2`, `fetch_negotiator`, `pkt_line`.
- `FetchOutcome`: **the structured shape jj already consumes from gix** — per
  ref `(remote_ref, local_tracking_ref, old_oid, new_oid, UpdateMode)` where
  `UpdateMode ∈ {New, FastForward, Forced, NonFastForwardRejected,
  TagUpdateRejected, SourceObjectNotFound, CurrentlyCheckedOutRejected,
  UpToDate, Unborn, …}` (mirrors `gix::remote::fetch::refs::update::Mode`), plus
  the resolved ref-map and shallow boundary updates. This lets jj's
  `gix_fetch_updates` / `fetch_mapping_ref_diff` translation stay thin.
- Lift orchestration from `fetch_transport.rs` + `commands/fetch.rs`; `clone`
  becomes init + fetch (as in the spike).

### 3. Push / send-pack API
`grit_lib::push::push(repo, remote, PushOptions) -> Result<PushOutcome>`.
- `PushOptions`: ref updates with `force` / `delete` / **expected-old-id (CAS)**,
  push-options, atomic, thin, dry-run, signed push (`push_cert`), progress,
  credentials; honor multiple `remote.*.pushurl` (try each).
- Drive: handshake → read remote refs → compute updates → **negotiation-driven
  pack build** (workstream 4) → stream with sideband → parse
  `report-status`/`report-status-v2`.
- `PushOutcome`: reuse `push_report::{PushRefResult, PushRefStatus}` — surface
  rejected refs with reasons (non-ff, fetch-first, stale, remote-rejected).
- Lift from `commands/{push,send_pack}.rs` + `http_push_smart.rs`; reuse the
  sideband framing variants the spike enumerated (pkt-line-before-channel,
  channel-before-pkt-line, coalesced reads, trailing `0000`).

### 4. Pack negotiation & generation (highest-risk gap)
Expose the **real** pack builder (it already does delta + thin), parameterized
by a negotiated want/have set so embedders never pack all reachable objects:
`grit_lib::pack::build_pack(odb, wants, haves, PackBuildOptions{thin, delta,
window, depth, …}) -> writes pack to a sink`.
- Lift from `commands/pack_objects.rs`. This is the single most important new API
  for push viability — it's the fix for the 478 MB finding.
- Pair it with a reusable "what to send" computation (reachable-from-wants minus
  reachable-from-haves) driven by negotiation results.

### 5. Credential layer
`grit_lib::credentials`: a `CredentialProvider` trait (`fill` / `approve` /
`reject`) + a Git-compatible default that runs configured `credential.helper`
programs from config, with **explicit typed non-interactive failure** (no
silent TTY prompt). Lift from `commands/credential*.rs` (protocol, cache,
store). Embedders can plug their own provider (jj/GitButler have their own auth).

### 6. Other replaced paths (the rest of the spike)
- **Remote default branch** (`git remote show` replacement): expose
  `grit_lib::remote::default_branch(repo, remote)` — local via `ls_remote`
  symref, remote via handshake `symref=HEAD:`. (`ls_remote` already does local.)
- **GC**: jj only needs in-process loose-unreachable pruning + recreate no-gc
  refs: `grit_lib::gc::prune_loose_unreachable(odb, reachable_roots, keep_newer)`.
  (Full repack/pack-refs explicitly out of scope, per the spike.)
- **Local push / batch ref updates**: expose a transactional, CAS-aware
  `grit_lib::refs::update_refs(transaction)` for the in-process ref/object update
  path (`delete_ref` is already used; generalize to a batch transaction matching
  what jj built on `gix::refs::transaction`).

## Sequencing

1. Transport trait + `file://` and `git://` impls — exactly what the spike's
   passing tests cover (local + `git daemon`).
2. Fetch API + `FetchOutcome` over those transports.
3. Pack negotiation/build primitive (gate for push).
4. Push API over `file://` + `git://` (+ sideband/status parsing).
5. Credential trait + helper-program default.
6. `http(s)` and `ssh` transports (the spike's thin/unproven area; needed for
   real remotes and HTTPS/auth).
7. GC prune, default-branch, ref-transaction apply.

Items 1–4 reproduce the spike's green path on grit-lib alone; 5–6 close its
biggest "known gaps / risks"; 7 mops up the remaining replaced paths.

## Design constraints (from the spike's lessons)

- **No process/stdout/file assumptions** in lib APIs — progress via callback
  traits, results/errors as typed values (the spike noted error-message
  divergence when native validation replaced Git CLI wording; structured errors
  let embedders choose wording).
- **Negotiation-driven object selection is mandatory** (478 MB lesson).
- **Credentials fail clearly when non-interactive**, never prompt/hang.
- **Keep sha256/object-format awareness** flowing through new pack/transport code.
- **Mirror gix's structured result shapes** (`update::Mode`, ref-map mapping) so
  jj's translation layer stays a thin adapter rather than a rewrite.
- **Pluggable transport + credentials** so embedders with their own stacks
  (GitButler, jj) can supply them.

## Testing

- Port the spike's canaries as grit-lib integration tests (these are the tests
  joshka/schacon flagged as worth keeping regardless of the spike's fate):
  local / `file://` / `git://` fetch+clone+push with **no external git**;
  multiple pushurls all attempted; pushurl overrides fetch URL for non-local
  push; receive-pack sideband framing variants; flush-packet handling; rejected
  pushes (non-ff / expected-old-id mismatch); deletions; receive hooks
  (`pre-receive`/`update`/`post-receive`); and a **large-repo guard asserting the
  generated pack's object count is ≪ all-reachable** (regression test for the
  478 MB finding).
- Re-point the existing CLI commands at the new lib APIs so grit's
  already-passing C-git suite validates the extraction.
- Add an embedder smoke test that consumes the public `fetch`/`push` API the way
  jj would (structured outcome → apply to an external ref store), to keep the API
  ergonomic and stable.
