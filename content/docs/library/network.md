---
title: Network
summary: Remote dispatch, fetch, push, and ls-remote over every transport.
---

Network operations in grit-lib center on `Remote`: a typed URL (`RemoteUrl`), configured fetch refspecs, and one dispatcher for **fetch**, **push**, and **list refs**. Wire bytes still flow through the same fetch/push engines as before; `Remote` picks the transport from the URL scheme.

## Remote URL and config

| Scheme / form | `RemoteUrl` variant | Transport |
| --- | --- | --- |
| Path or `file://` | `Local` / `File` | [`fetch_local`](rustdoc:grit_lib::remote::transfer::fetch_local) / [`push_local`](rustdoc:grit_lib::remote::transfer::push_local) |
| `git://` | `Git` | [`GitDaemonTransport`](rustdoc:grit_lib::remote::transport::GitDaemonTransport) |
| `ssh://`, scp-style | `Ssh` | [`SshTransport`](rustdoc:grit_lib::remote::transport::SshTransport) (repo [`CommandRunner`](rustdoc:grit_lib::command_runner::CommandRunner)) |
| `http(s)://` | `Http` / `Https` | [`http_fetch`](rustdoc:grit_lib::remote::transport::http::http_fetch) / [`push_http`](rustdoc:grit_lib::remote::push::push_http) |

Parse a literal URL with [`RemoteUrl::try_from`](rustdoc:grit_lib::remote::RemoteUrl). Load `remote.<name>.url`, optional `pushurl`, and `fetch` refspecs (default `+refs/heads/*:refs/remotes/<name>/*`) via [`Remote::from_config`](rustdoc:grit_lib::remote::Remote). Config applies [`url_rewrite`](rustdoc:grit_lib::remote::url_rewrite) `insteadOf` / `pushInsteadOf` rules the same way Git does.

Local paths resolve through [`resolve_local_remote_git_dir`](rustdoc:grit_lib::remote::transport_path::resolve_local_remote_git_dir) from the repository root, not the process cwd.

To turn a user's repository argument (a configured name, a URL, an scp-style `host:path`, or a path) into a `Remote`, call [`resolve_remote_argument`](rustdoc:grit_lib::remote::fetch_request::resolve_remote_argument). A configured name wins. A name with a config section but no URL fails with `RemoteError::NoUrl`, and anything else fails with [`RemoteError::UnknownRemote`](rustdoc:grit_lib::remote::RemoteError). `grit fetch`, `grit pull` and `grit remote refs` all resolve their argument this way.

## Managing remotes

[`repo.remotes().configured()`](rustdoc:grit_lib::repo_handles::RepositoryRemotes::configured) lists every remote with a `remote.<name>.*` section, sorted by name, the same set `git remote` prints. Each [`ConfiguredRemote`](rustdoc:grit_lib::repo_handles::ConfiguredRemote) carries its URL, or `None` when the section has no URL, plus raw `push_urls`, `fetch_refspecs`, and effective fetch/push URLs after `url.*.insteadOf` / `pushInsteadOf`. [`list()`](rustdoc:grit_lib::repo_handles::RepositoryRemotes::list) returns only the names that have a URL.

[`repo.remotes().add(name, url)`](rustdoc:grit_lib::repo_handles::RepositoryRemotes::add) writes `remote.<name>.url` and the default fetch refspec to the shared repository config (also from a linked worktree), then reloads the repository's config. Like `git remote add`, it refuses an existing remote (`RemoteError::RemoteExists`) and a name whose tracking refs `refs/remotes/<name>/…` would be invalid (`RemoteError::InvalidRemoteName`), and it stores the URL as given.

[`repo.remotes().set_url(name, url, kind, dry_run)`](rustdoc:grit_lib::repo_handles::RepositoryRemotes::set_url) updates fetch or push URLs on an existing remote (`RemoteUrlKind::Fetch` or `Push`), returning [`UrlChange`](rustdoc:grit_lib::repo_handles::UrlChange). Push replaces every `pushurl` with one value. Returns [`RemoteError::UnknownRemote`](rustdoc:grit_lib::remote::RemoteError::UnknownRemote) when the section is missing.

[`repo.remotes().prune(name, dry_run, http_factory)`](rustdoc:grit_lib::repo_handles::RepositoryRemotes::prune) deletes stale remote-tracking refs without fetching, using the same stale-ref rules as fetch prune ([`tracking_refs_gone_on_remote`](rustdoc:grit_lib::remote::transfer::tracking_refs_gone_on_remote)). Returns the full ref names removed (or that would be removed when `dry_run` is true).

[`repo.remotes().set_head(name, mode, dry_run, http_factory)`](rustdoc:grit_lib::repo_handles::RepositoryRemotes::set_head) sets or deletes `refs/remotes/<name>/HEAD` via [`SetHead`](rustdoc:grit_lib::repo_handles::SetHead) (`Auto`, `Branch`, or `Delete`).

[`repo.remotes().inspect(name, options, http_factory)`](rustdoc:grit_lib::repo_handles::RepositoryRemotes::inspect) returns a [`RemoteSummary`](rustdoc:grit_lib::remote::RemoteSummary): fetch and push URLs, optional [`head_branch`](rustdoc:grit_lib::remote::RemoteSummary::head_branch), [`RemoteBranchState`](rustdoc:grit_lib::remote::RemoteBranchState) rows (`tracked`, `new`, or `stale`), and [`RemotePullBranch`](rustdoc:grit_lib::remote::RemotePullBranch) entries from `branch.*` config. With [`InspectOptions::query`](rustdoc:grit_lib::remote::InspectOptions::query) it lists refs on the remote (symrefs included) and compares them to local tracking refs; stale detection reuses [`tracking_refs_gone_on_remote`](rustdoc:grit_lib::remote::transfer::tracking_refs_gone_on_remote), the same rules as fetch prune. Without a query, branch names come from `refs/remotes/<name>/…` and are all reported as tracked.

## list refs

`Remote::list_refs` takes `ListRefsOptions` (prefixes, heads, tags, symrefs, peel) and returns `RemoteRef` entries in `git ls-remote` order. On disk it uses `list_refs_from_git_dir`. Over the wire it uses protocol v2 `ls-refs` when available, otherwise the v0/v1 ref advertisement.

With `peel` set, annotated tags come back as two entries, the tag and a `<tag>^{}` line holding the object it peels to: the first non-tag object, through any chain of tags of tags, as `git ls-remote` reports it. [`RemoteRef::peeled_tag`](rustdoc:grit_lib::remote::RemoteRef::peeled_tag) identifies those lines, and [`fold_peeled_refs`](rustdoc:grit_lib::remote::fold_peeled_refs) folds them into one [`PeeledRemoteRef`](rustdoc:grit_lib::remote::PeeledRemoteRef) per ref, with `peeled` set, keeping the listing order.

## Fetch and push

[`Remote::fetch`](rustdoc:grit_lib::remote::Remote) accepts [`FetchOptions`](rustdoc:grit_lib::remote::transfer::FetchOptions) and a [`Progress`](rustdoc:grit_lib::remote::fetch::Progress) sink; fetch negotiates protocol v2 when the server supports it. Its [`FetchOutcome`](rustdoc:grit_lib::remote::transfer::FetchOutcome) holds one `RefUpdate` per matched ref, including the ones that were already up to date. For a report, [`reported_updates`](rustdoc:grit_lib::remote::transfer::FetchOutcome::reported_updates) keeps the refs that changed or were refused, [`changed_ref_count`](rustdoc:grit_lib::remote::transfer::FetchOutcome::changed_ref_count) counts the changed ones, and [`has_rejections`](rustdoc:grit_lib::remote::transfer::FetchOutcome::has_rejections) says whether any update was refused (`UpdateMode::is_rejected`). `grit fetch` builds its output and exit code from these. [`Remote::push`](rustdoc:grit_lib::remote::Remote) uses [`PushRefSpec`](rustdoc:grit_lib::remote::transfer::PushRefSpec) and returns [`PushOutcome`](rustdoc:grit_lib::remote::transfer::PushOutcome).

To plan a push the way `grit push` does, start from its positionals: [`resolve_push_invocation`](rustdoc:grit_lib::remote::push_plan::resolve_push_invocation) splits `[<remote>] [<refspec>...]` into a [`PushInvocation`](rustdoc:grit_lib::remote::push_plan::PushInvocation) (configured remote, literal URL or path, or refspecs for the default remote). [`load_push_plan_context`](rustdoc:grit_lib::remote::push_plan::load_push_plan_context) reads the selected remote's refs ([`Remote::list_push_refs`](rustdoc:grit_lib::remote::Remote::list_push_refs), one scan of the push destination) into a [`PushPlanContext`](rustdoc:grit_lib::remote::push_plan::PushPlanContext), [`resolve_push`](rustdoc:grit_lib::remote::push_plan::resolve_push) turns a [`PushRequest`](rustdoc:grit_lib::remote::push_plan::PushRequest) into concrete ref updates, and [`open_push_destination`](rustdoc:grit_lib::remote::push_plan::open_push_destination) opens the `Remote` to send them to. Pass `PushPlanContext::remote_tips` on as `PushOptions::remote_ref_tips` so a local push does not scan the destination's refs a second time.

[`pull`](rustdoc:grit_lib::porcelain::pull::pull) is the high-level fetch-and-integrate operation. Build [`PullOptions`](rustdoc:grit_lib::porcelain::pull::PullOptions) (including shared [`FetchOptions`](rustdoc:grit_lib::remote::transfer::FetchOptions), explicit refspec/`FETCH_HEAD` integration, [`resolve_pull_autostash`](rustdoc:grit_lib::porcelain::pull::resolve_pull_autostash), and dry-run `--check` via [`PullCheck`](rustdoc:grit_lib::porcelain::pull::PullCheck)), pass a progress sink and HTTP client factory, and receive a typed [`PullOutcome`](rustdoc:grit_lib::porcelain::pull::PullOutcome). It maps `branch.<name>.merge` through the remote's fetch refspecs, uses [`resolve_pull_integration_base`](rustdoc:grit_lib::refs::branch_tracking::resolve_pull_integration_base) after explicit fetches, skips the fetch for a local `remote = .` upstream, and fast-forwards, merges, or adopts the upstream on an unborn branch. Failures are typed as [`PullError`](rustdoc:grit_lib::porcelain::pull::PullError); `grit pull` only builds options, invokes this API, and renders the outcome.

HTTP remotes need an [`HttpClient`](rustdoc:grit_lib::remote::transport::http::HttpClient). Implement [`HttpClientFactory`](rustdoc:grit_lib::remote::HttpClientFactory) or, with the `http-ureq` feature, pass `None` to use the default ureq-backed factory (`UreqHttpClient` + [`HelperCredentialProvider`](rustdoc:grit_lib::remote::credentials::HelperCredentialProvider)). The `grit` binary on macOS enables `http-ureq-native-tls` (Secure Transport) for HTTPS throughput; other platforms use rustls unless you opt into `http-ureq-native-tls` when depending on `grit-lib`.

## Credentials and progress

[`HelperCredentialProvider`](rustdoc:grit_lib::remote::credentials::HelperCredentialProvider) satisfies HTTP `401` responses from configured `credential.helper` programs and never opens a TTY; [`helpers_for`](rustdoc:grit_lib::remote::credentials::HelperCredentialProvider::helpers_for) lists the helpers it would run for a credential, so you can tell up front whether storing one would be a no-op.

To sign a user in to GitHub without a client secret, run the OAuth device flow with [`DeviceFlow`](rustdoc:grit_lib::remote::github_auth::DeviceFlow): [`request_device_code`](rustdoc:grit_lib::remote::github_auth::DeviceFlow::request_device_code) returns the user code to display, and [`wait_for_token`](rustdoc:grit_lib::remote::github_auth::DeviceFlow::wait_for_token) polls (through your `HttpClient` and an injectable [`PollClock`](rustdoc:grit_lib::remote::github_auth::PollClock)) until the user authorizes. Store the token with `approve(&github_token_credential(Some(&token)))` ([`github_token_credential`](rustdoc:grit_lib::remote::github_auth::github_token_credential)) and later HTTPS fetches and pushes to github.com send it as the password; `grit auth` is built on exactly this.

When formatting your own transport errors, strip embedded userinfo with [`redact_url_for_display`](rustdoc:grit_lib::remote::transport_path::redact_url_for_display) so passwords never reach logs or JSON. Pass [`NoProgress`](rustdoc:grit_lib::remote::fetch::NoProgress) to ignore sideband progress, or implement [`Progress::message`](rustdoc:grit_lib::remote::fetch::Progress) for side-band channel 2. Set [`FetchOptions`](rustdoc:grit_lib::remote::transfer::FetchOptions) `remote_progress` to omit `no-progress` on the wire so the remote may emit channel 2; decode chunks with [`SidebandLines`](rustdoc:grit_lib::remote::sideband_lines::SidebandLines) into [`RemoteLine`](rustdoc:grit_lib::remote::sideband_lines::RemoteLine) updates and lines.

A received pack's length in bytes is not known until it ends, so byte counts alone cannot say how far a transfer is. Return `true` from [`Progress::wants_received_objects`](rustdoc:grit_lib::remote::fetch::Progress::wants_received_objects) and the receive also counts the pack's objects on a background thread, reporting [`ReceivedObjects`](rustdoc:grit_lib::progress::ReceivedObjects) (received, total from the pack header, and deltas so far) through [`Progress::received_objects`](rustdoc:grit_lib::remote::fetch::Progress::received_objects). The counting never makes the receive wait; if it falls far behind a very fast transfer, it stops. The same counter is available on its own as [`PackStreamScanner`](rustdoc:grit_lib::objects::pack_stream_scan::PackStreamScanner). To turn the phases of a fetch or clone into one percentage and time-left figure, feed the same events to an [`OverallEstimator`](rustdoc:grit_lib::progress::OverallEstimator); it takes elapsed time as an argument rather than reading the clock.

## Example

This example resolves `origin`, lists refs on a **local** bare remote, fetches, creates a commit reusing the fetched tip’s tree, and pushes — all over `file://`:

<!-- include: grit-examples/src/bin/guide_network.rs -->

The `grit-examples` crate also ships `gritx-fetch` and `gritx-push`, which dispatch on URL scheme and print transport/auth discovery lines. Shared wiring lives in `grit-examples/src/remote.rs` (slated to thin over `Remote` in a follow-up step).

Integration test `guide_network` builds bare and consumer repos with system Git, runs the binary, then requires clean `git fsck --strict` on both sides and matching `git rev-parse` on the pushed ref.

## Serving fetches and pushes

The server side of the protocol lives in [`remote::serve`](rustdoc:grit_lib::remote::serve). [`upload_pack`](rustdoc:grit_lib::remote::serve::upload_pack) answers clones and fetches (protocol v0, v1 and v2) and [`receive_pack`](rustdoc:grit_lib::remote::serve::receive_pack) accepts pushes. Both take an opened `Repository`, a reader and a writer, and [`ServeOptions`](rustdoc:grit_lib::remote::serve::ServeOptions), so the same code serves a pipe or ssh channel (the defaults) and smart HTTP (`advertise_refs` for `GET info/refs`, `stateless_rpc` for the POST). The library never reads the environment: map `GIT_PROTOCOL` with [`ProtocolVersion::from_git_protocol`](rustdoc:grit_lib::remote::serve::ProtocolVersion::from_git_protocol) at your process boundary. Open the repository with `RepositoryOptions::for_serving()`.

A protocol v2 `ls-refs` request asks the ref store for its `ref-prefix` ranges only, and reads objects only when the client sent `peel`. Even then, a fully peeled `packed-refs` answers without opening the tags. So the `ls-refs` call that every `git fetch` and `git ls-remote` sends costs in proportion to the refs it returns, not the refs stored. The v0/v1 advertisement always peels tags, and the push advertisement never does. Like Git, every advertisement includes refs whose objects are missing; such a ref just has no peeled value.

A pushed pack is streamed into a temp file in a quarantine object directory, so memory use does not grow with the pack size, and only bytes up to the pack's trailing checksum are read (a push client keeps its side open until it has the status report). Packs with fewer objects than `receive.unpackLimit` are stored as loose objects; larger ones are indexed and kept as a pack. `receive.maxInputSize` caps the pack size.

What a push may change is decided in three layers, in this order for each push:

1. [`ReceivePolicy`](rustdoc:grit_lib::remote::serve::ReceivePolicy), usually [`ReceivePolicy::from_config`](rustdoc:grit_lib::remote::serve::ReceivePolicy::from_config) (`receive.denyNonFastForwards`, `receive.denyDeletes`, `receive.denyCurrentBranch`), plus the checks every server makes: valid ref names, hidden refs, the expected old value, and that the new objects exist.
2. An in-process [`ReceiveHook`](rustdoc:grit_lib::remote::serve::ReceiveHook) passed to [`receive_pack_with_hook`](rustdoc:grit_lib::remote::serve::receive_pack_with_hook). Its `pre_receive` method sees every accepted update and an object database that includes the still-quarantined pushed objects; refusing there refuses the whole push and nothing is moved into the repository. Its `update` method checks one ref at a time after the objects are moved in. A closure `Fn(&RefCommand, &Odb) -> Result<(), RefUpdateRefusal>` is a `ReceiveHook` that implements `update`.
3. The repository's hook scripts (`pre-receive`, `update`, `post-receive`, `post-update`), run through the repository's `CommandRunner` with Git's environment (`GIT_DIR=.`, the quarantine variables and `GIT_PUSH_OPTION_*`). Each in-process check runs just before the matching script.

The [`ReceiveOutcome`](rustdoc:grit_lib::remote::serve::ReceiveOutcome) is typed: each [`RefUpdateResult`](rustdoc:grit_lib::remote::serve::RefUpdateResult) carries an `Option<RefUpdateRefusal>` ([`RefUpdateRefusal`](rustdoc:grit_lib::remote::serve::RefUpdateRefusal): `NonFastForward`, `StaleInfo`, `DeletionProhibited`, `PreReceiveHookDeclined`, `Policy(reason)` from your hook, ...), and `unpack_error` is an [`UnpackError`](rustdoc:grit_lib::remote::serve::UnpackError) (`Truncated`, `TooLarge`, `Malformed`, `Ingest`, `Io`). Match on the variants; their `Display` text is what the client is sent on its `ng <ref> <reason>` and `unpack <reason>` status lines. Neither is an `Err`: `receive_pack` returns [`ServeError`](rustdoc:grit_lib::remote::serve::ServeError) only when the session itself fails (a malformed command list, a broken stream, or a repository error while applying the push).

This server serves fetches, and refuses pushes to `refs/heads/protected/*` with a closure. Git runs it as `git push --receive-pack='guide_serve receive-pack' <path>` (and `--upload-pack='guide_serve upload-pack'` for clone and fetch):

<!-- include: grit-examples/src/bin/guide_serve.rs -->

Integration test `guide_serve` clones through it over protocol v0 and v2, pushes a commit, checks `git fsck --strict` on the server, and checks that a push to a protected branch is reported as `[remote rejected] (protected branch)`. The `grit upload-pack` / `grit receive-pack` commands and `grit-http-server` are built on the same functions.

## Partial clone and promisor fetch

Filtered fetch and clone record a promisor remote (`remote.<name>.promisor`, `extensions.partialclone`, and `remote.<name>.partialclonefilter`). Blobs omitted from the initial pack are **promised**: they appear in the object database as missing until fetched on demand.

Install a [`PromisorFetcher`](rustdoc:grit_lib::objects::promisor::PromisorFetcher) on [`RepositoryOptions::with_promisor_fetcher`](rustdoc:grit_lib::config::environment::RepositoryOptions::with_promisor_fetcher) (no global state). The default [`DefaultPromisorFetcher`](rustdoc:grit_lib::objects::promisor::DefaultPromisorFetcher) sends `want` lines for the missing object ids with the remote's filter, does not update refs, and ingests the reply as a promisor pack. The `grit` CLI installs this fetcher for every opened repository.

When a fetcher is installed and [`fetch.promisorLazy`](rustdoc:grit_lib::objects::promisor::PROMISOR_LAZY_FETCH_CONFIG) is true (the default), an ODB read miss in a partial-clone repository triggers one batched fetch (with recursion protection) and retries the read. With no fetcher, or when lazy fetch is disabled, reads return [`Error::PromisedObjectMissing`](rustdoc:grit_lib::error::Error::PromisedObjectMissing). Set `fetch.promisorLazy=false` in config, or `GRIT_NO_LAZY_FETCH=1` / `GIT_NO_LAZY_FETCH=1` in the environment, to opt out.

[`Repository::prefetch_promised`](rustdoc:grit_lib::repo::Repository::prefetch_promised) batch-fetches object ids before bulk reads. Tree-to-tree checkout (including clone checkout and [`grit switch`](../commands/switch.md)) collects missing blob oids from the diff and prefetches them in a single request when a fetcher is present.

[`CloneOptions::promisor_fetcher`](rustdoc:grit_lib::remote::clone::CloneOptions) carries the fetcher through to the opened repository so blobless clone checkout can prefetch without a separate reopen.

## Bundles

Offline bundle files are documented on the [Bundle](../bundle/) guide (`RepositoryBundles`). Smart HTTP and local remotes above cover online fetch/push.
