# Network

> ls-remote, fetch and push over file:// and smart HTTP, credentials, and progress.

Network operations in grit-lib are **typed**: you pass option structs, get outcome structs, and wire a [`Progress`](https://docs.rs/grit-lib/latest/grit_lib/fetch/trait.Progress.html) sink for sideband messages. No subprocess `git` on the wire — local, `git://`, SSH, and smart HTTP share the same fetch/push engines.

## Transport matrix

| URL | Fetch | Push | Authentication |
| --- | --- | --- | --- |
| `file://` or path | [`fetch_local`](https://docs.rs/grit-lib/latest/grit_lib/transfer/fn.fetch_local.html) | [`push_local`](https://docs.rs/grit-lib/latest/grit_lib/transfer/fn.push_local.html) | none |
| `git://` | [`fetch_remote`](https://docs.rs/grit-lib/latest/grit_lib/fetch/fn.fetch_remote.html) over [`GitDaemonTransport`](https://docs.rs/grit-lib/latest/grit_lib/transport/struct.GitDaemonTransport.html) | [`push_remote`](https://docs.rs/grit-lib/latest/grit_lib/push/fn.push_remote.html) | none |
| `ssh` | same over [`SshTransport`](https://docs.rs/grit-lib/latest/grit_lib/transport/struct.SshTransport.html) | same | SSH keys/agent |
| `http(s)` | [`http_fetch`](https://docs.rs/grit-lib/latest/grit_lib/transport/http/fn.http_fetch.html) | [`push_http`](https://docs.rs/grit-lib/latest/grit_lib/push/fn.push_http.html) | [`CredentialProvider`](https://docs.rs/grit-lib/latest/grit_lib/credentials/trait.CredentialProvider.html) |

Wire transports use [`Transport::connect`](https://docs.rs/grit-lib/latest/grit_lib/transport/trait.Transport.html) with [`Service::UploadPack`](https://docs.rs/grit-lib/latest/grit_lib/transport/enum.Service.html) (fetch) or `ReceivePack` (push). Fetch negotiates protocol v2 when possible; push uses v0/v1.

Local remotes resolve through [`resolve_local_remote_git_dir`](https://docs.rs/grit-lib/latest/grit_lib/transport_path/fn.resolve_local_remote_git_dir.html) so relative `remote.*.url` values behave like Git (repository root, not process cwd).

## ls-remote

For a **local** git directory (bare repo or `.git`), [`ls_remote`](https://docs.rs/grit-lib/latest/grit_lib/ls_remote/fn.ls_remote.html) lists refs with the same ordering and peeling rules as `git ls-remote` on a filesystem remote. Pass [`ls_remote::Options`](https://docs.rs/grit-lib/latest/grit_lib/ls_remote/struct.Options.html) to filter heads, tags, or patterns. Remote listing over HTTP/SSH is driven by fetch negotiation; local enumeration is the usual first step for `file://` tests and tools.

## Fetch and push inputs

[`FetchOptions`](https://docs.rs/grit-lib/latest/grit_lib/transfer/struct.FetchOptions.html) carries refspecs, tag mode ([`TagMode`](https://docs.rs/grit-lib/latest/grit_lib/transfer/enum.TagMode.html)), prune, shallow depth, and related flags. [`FetchOutcome`](https://docs.rs/grit-lib/latest/grit_lib/transfer/struct.FetchOutcome.html) reports ref updates and optional default-branch hints.

Push uses [`PushRefSpec`](https://docs.rs/grit-lib/latest/grit_lib/transfer/struct.PushRefSpec.html) entries (source oid, destination ref, force, lease fields) and returns [`PushOutcome`](https://docs.rs/grit-lib/latest/grit_lib/transfer/struct.PushOutcome.html) with per-ref [`PushRefStatus`](https://docs.rs/grit-lib/latest/grit_lib/push_report/enum.PushRefStatus.html).

## Credentials and progress

HTTP smart transport accepts an [`HttpClient`](https://docs.rs/grit-lib/latest/grit_lib/transport/http/trait.HttpClient.html) implementation. With the `http-ureq` feature, `UreqHttpClient::from_config` honors `http.proxy`, cookies, and extra headers from [`ConfigSet`](https://docs.rs/grit-lib/latest/grit_lib/config/struct.ConfigSet.html). [`SmartHttpTransport`](https://docs.rs/grit-lib/latest/grit_lib/transport/http/struct.SmartHttpTransport.html) wraps any client for combined fetch/push entry points.

[`HelperCredentialProvider`](https://docs.rs/grit-lib/latest/grit_lib/credentials/struct.HelperCredentialProvider.html) runs configured `credential.helper` programs on `401` and retries with HTTP Basic. It **never** opens a TTY — missing credentials surface as [`Error::Auth`](https://docs.rs/grit-lib/latest/grit_lib/error/enum.Error.html).

Pass [`NoProgress`](https://docs.rs/grit-lib/latest/grit_lib/fetch/struct.NoProgress.html) to ignore sideband progress, or implement [`Progress::message`](https://docs.rs/grit-lib/latest/grit_lib/fetch/trait.Progress.html) to receive raw progress bytes from side-band channel 2.

## Example

This example resolves `origin`, lists refs on a **local** bare remote, fetches, creates a commit reusing the fetched tip’s tree, and pushes — all over `file://`:

```rust
//! List refs on a local remote, fetch, create a commit, and push over `file://`.
//!
//! Source for the library guide "Network" page (included in the docs site).

use grit_examples::remote;
use grit_lib::config::ConfigSet;
use grit_lib::ls_remote::{self, Options as LsRemoteOptions};
use grit_lib::objects::{parse_commit, serialize_commit, CommitData, ObjectKind};
use grit_lib::refs;
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
    let config = ConfigSet::load(Some(&git_dir), true)?;

    let remote_info = remote::resolve_remote(&config, &git_dir, Some("origin"), false)
        .map_err(|e| grit_lib::error::Error::Message(e.to_string()))?;

    let remote_git_dir =
        resolve_local_remote_git_dir(&remote_info.url, &git_dir, work_tree.as_deref());
    let remote_repo = Repository::open(&remote_git_dir, None)?;
    let refs_on_remote = ls_remote::ls_remote(
        &remote_git_dir,
        &remote_repo.odb,
        &LsRemoteOptions::default(),
    )?;
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
    remote::fetch(&git_dir, &remote_info, &fetch_opts)
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
    remote::push(&git_dir, &remote_info, &[spec], &PushOptions::default())
        .map_err(|e| grit_lib::error::Error::Message(e.to_string()))?;

    println!("{new_oid}");
    Ok(())
}
```

The `grit-examples` crate also ships `gritx-fetch` and `gritx-push`, which dispatch on URL scheme and print transport/auth discovery lines. Shared wiring lives in `grit-examples/src/remote.rs`.

Integration test `guide_network` builds bare and consumer repos with system Git, runs the binary, then requires clean `git fsck --strict` on both sides and matching `git rev-parse` on the pushed ref.
