# Network

> Remote dispatch, fetch, push, and ls-remote over every transport.

Network operations in grit-lib center on [`Remote`](https://docs.rs/grit-lib/latest/grit_lib/remote/struct.Remote.html): a typed URL ([`RemoteUrl`](https://docs.rs/grit-lib/latest/grit_lib/remote/enum.RemoteUrl.html)), configured fetch refspecs, and one dispatcher for **fetch**, **push**, and **list refs**. Wire bytes still flow through the same fetch/push engines as before; `Remote` picks the transport from the URL scheme.

## Remote URL and config

| Scheme / form | [`RemoteUrl`](https://docs.rs/grit-lib/latest/grit_lib/remote/enum.RemoteUrl.html) variant | Transport |
| --- | --- | --- |
| Path or `file://` | `Local` / `File` | [`fetch_local`](https://docs.rs/grit-lib/latest/grit_lib/transfer/fn.fetch_local.html) / [`push_local`](https://docs.rs/grit-lib/latest/grit_lib/transfer/fn.push_local.html) |
| `git://` | `Git` | [`GitDaemonTransport`](https://docs.rs/grit-lib/latest/grit_lib/transport/struct.GitDaemonTransport.html) |
| `ssh://`, scp-style | `Ssh` | [`SshTransport`](https://docs.rs/grit-lib/latest/grit_lib/transport/struct.SshTransport.html) (repo [`CommandRunner`](https://docs.rs/grit-lib/latest/grit_lib/command_runner/trait.CommandRunner.html)) |
| `http(s)://` | `Http` / `Https` | [`http_fetch`](https://docs.rs/grit-lib/latest/grit_lib/transport/http/fn.http_fetch.html) / [`push_http`](https://docs.rs/grit-lib/latest/grit_lib/push/fn.push_http.html) |

Parse a literal URL with [`RemoteUrl::try_from`](https://docs.rs/grit-lib/latest/grit_lib/remote/enum.RemoteUrl.html). Load `remote.<name>.url`, optional `pushurl`, and `fetch` refspecs (default `+refs/heads/*:refs/remotes/<name>/*`) via [`Remote::from_config`](https://docs.rs/grit-lib/latest/grit_lib/remote/struct.Remote.html). Config applies [`url_rewrite`](https://docs.rs/grit-lib/latest/grit_lib/url_rewrite/index.html) `insteadOf` / `pushInsteadOf` rules the same way Git does.

Local paths resolve through [`resolve_local_remote_git_dir`](https://docs.rs/grit-lib/latest/grit_lib/transport_path/fn.resolve_local_remote_git_dir.html) from the repository root, not the process cwd.

## list refs

[`Remote::list_refs`](https://docs.rs/grit-lib/latest/grit_lib/remote/struct.Remote.html) takes [`ListRefsOptions`](https://docs.rs/grit-lib/latest/grit_lib/remote/struct.ListRefsOptions.html) (prefixes, heads, tags, symrefs, peel) and returns [`RemoteRef`](https://docs.rs/grit-lib/latest/grit_lib/remote/struct.RemoteRef.html) entries in `git ls-remote` order. On disk it uses [`list_refs_from_git_dir`](https://docs.rs/grit-lib/latest/grit_lib/remote/fn.list_refs_from_git_dir.html). Over the wire it uses protocol v2 `ls-refs` when available, otherwise the v0/v1 ref advertisement.

## Fetch and push

[`Remote::fetch`](https://docs.rs/grit-lib/latest/grit_lib/remote/struct.Remote.html) accepts [`FetchOptions`](https://docs.rs/grit-lib/latest/grit_lib/transfer/struct.FetchOptions.html) and a [`Progress`](https://docs.rs/grit-lib/latest/grit_lib/fetch/trait.Progress.html) sink; fetch negotiates protocol v2 when the server supports it. [`Remote::push`](https://docs.rs/grit-lib/latest/grit_lib/remote/struct.Remote.html) uses [`PushRefSpec`](https://docs.rs/grit-lib/latest/grit_lib/transfer/struct.PushRefSpec.html) and returns [`PushOutcome`](https://docs.rs/grit-lib/latest/grit_lib/transfer/struct.PushOutcome.html).

HTTP remotes need an [`HttpClient`](https://docs.rs/grit-lib/latest/grit_lib/transport/http/trait.HttpClient.html). Implement [`HttpClientFactory`](https://docs.rs/grit-lib/latest/grit_lib/remote/trait.HttpClientFactory.html) or, with the `http-ureq` feature, pass `None` to use the default ureq-backed factory (`UreqHttpClient` + [`HelperCredentialProvider`](https://docs.rs/grit-lib/latest/grit_lib/credentials/struct.HelperCredentialProvider.html)).

## Credentials and progress

[`HelperCredentialProvider`](https://docs.rs/grit-lib/latest/grit_lib/credentials/struct.HelperCredentialProvider.html) satisfies HTTP `401` responses from configured `credential.helper` programs and never opens a TTY. Pass [`NoProgress`](https://docs.rs/grit-lib/latest/grit_lib/fetch/struct.NoProgress.html) to ignore sideband progress, or implement [`Progress::message`](https://docs.rs/grit-lib/latest/grit_lib/fetch/trait.Progress.html) for side-band channel 2.

## Example

This example resolves `origin`, lists refs on a **local** bare remote, fetches, creates a commit reusing the fetched tip’s tree, and pushes — all over `file://`:

```rust
//! List refs on a local remote, fetch, create a commit, and push over `file://`.
//!
//! Source for the library guide "Network" page (included in the docs site).

use grit_examples::remote;
use grit_lib::config::ConfigSet;
use grit_lib::objects::{parse_commit, serialize_commit, CommitData, ObjectKind};
use grit_lib::refs;
use grit_lib::remote::{list_refs_from_git_dir, ListRefsOptions};
use grit_lib::repo::Repository;
use grit_lib::transfer::{FetchOptions, PushOptions, PushRefSpec, TagMode};
use grit_lib::transport_path::resolve_local_remote_git_dir;
use std::path::Path;

fn main() -> Result<(), grit_lib::error::Error> {
    let consumer = std::env::args().nth(1).ok_or_else(|| {
        grit_lib::error::Error::Message("usage: guide_network <consumer-repo>".to_owned())
    })?;
    let consumer = Path::new(&consumer);
    let repo = Repository::discover(Some(consumer))?;
    let git_dir = repo.git_dir.clone();
    let work_tree = repo.work_tree.clone();
    let config = ConfigSet::load(
        &grit_lib::environment::Environment::capture_process(),
        Some(&git_dir),
        true,
    )?;

    let remote_info = remote::resolve_remote(&config, &git_dir, Some("origin"), false)
        .map_err(|e| grit_lib::error::Error::Message(e.to_string()))?;

    let remote_git_dir =
        resolve_local_remote_git_dir(&remote_info.url, &git_dir, work_tree.as_deref());
    let remote_repo = Repository::open(&remote_git_dir, None)?;
    let refs_on_remote = list_refs_from_git_dir(
        &remote_git_dir,
        &remote_repo.odb,
        &ListRefsOptions::default(),
    )
    .map_err(grit_lib::error::Error::from)?;
    eprintln!(
        "ls-remote: {} ref(s) on {}",
        refs_on_remote.len(),
        remote_git_dir.display()
    );

    let fetch_opts = FetchOptions {
        refspecs: remote_info.fetch_refspecs.clone(),
        tags: TagMode::Following,
        ..Default::default()
    };
    remote::fetch(&repo, &remote_info, &fetch_opts)
        .map_err(|e| grit_lib::error::Error::Message(e.to_string()))?;

    let tracking = refs::resolve_ref(&git_dir, "refs/remotes/origin/main")?;
    refs::write_ref(&git_dir, "refs/heads/main", &tracking)?;

    let parent_obj = repo.odb.read(&tracking)?;
    let parent = parse_commit(&parent_obj.data)?;
    let commit = CommitData {
        tree: parent.tree,
        parents: vec![tracking],
        author: parent.author.clone(),
        committer: parent.committer.clone(),
        author_raw: parent.author_raw.clone(),
        committer_raw: parent.committer_raw.clone(),
        encoding: parent.encoding.clone(),
        message: "library guide network example\n".to_owned(),
        raw_message: None,
        extra_headers: Vec::new(),
    };
    let new_oid = repo
        .odb
        .write(ObjectKind::Commit, &serialize_commit(&commit))?;
    refs::write_ref(&git_dir, "refs/heads/main", &new_oid)?;

    let spec = PushRefSpec {
        src: Some(new_oid),
        dst: "refs/heads/main".to_owned(),
        force: false,
        delete: false,
        expected_old: None,
        expect_absent: false,
    };
    remote::push(&repo, &remote_info, &[spec], &PushOptions::default())
        .map_err(|e| grit_lib::error::Error::Message(e.to_string()))?;

    println!("{new_oid}");
    Ok(())
}
```

The `grit-examples` crate also ships `gritx-fetch` and `gritx-push`, which dispatch on URL scheme and print transport/auth discovery lines. Shared wiring lives in `grit-examples/src/remote.rs` (slated to thin over [`Remote`](https://docs.rs/grit-lib/latest/grit_lib/remote/struct.Remote.html) in a follow-up step).

Integration test `guide_network` builds bare and consumer repos with system Git, runs the binary, then requires clean `git fsck --strict` on both sides and matching `git rev-parse` on the pushed ref.

## Bundles

Git’s [bundle format](https://git-scm.com/docs/gitformat-bundle) combines a text header (prerequisite commits, ref tips, optional v3 capabilities) with a thin packfile. [`grit_lib::bundle`](https://docs.rs/grit-lib/latest/grit_lib/bundle/index.html) reads and writes that format for offline transfer and tests:

- [`Bundle`](https://docs.rs/grit-lib/latest/grit_lib/bundle/struct.Bundle.html) (`open`, `verify`, `unbundle`) and [`read_header`](https://docs.rs/grit-lib/latest/grit_lib/bundle/fn.read_header.html) parse v2/v3 headers and leave the stream at the `PACK` magic.
- `verify` checks prerequisite OIDs against the ODB and ref connectivity (matching `git bundle verify` semantics).
- `unbundle` ingests the pack via the index-pack path (`fix-thin`) and returns ref tips without updating refs.
- [`write_bundle`](https://docs.rs/grit-lib/latest/grit_lib/bundle/fn.write_bundle.html) builds v2 (SHA-1, no filter) or v3 bundles with a thin pack stream.
- [`bundle_remote::fetch_from_bundle`](https://docs.rs/grit-lib/latest/grit_lib/bundle_remote/fn.fetch_from_bundle.html) ingests a bundle and applies fetch refspecs (clone/fetch from a `.bundle` path).

Integration test `bundle_git_compat` round-trips bundles with system `git bundle` (verify, list-heads, clone/fetch, `fsck --strict`).
