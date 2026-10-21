//! Golden snapshots for network commands (clone, fetch, pull, push, remote refs).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use grit_test_support::{
    git, git_fsck, grit, CliFixture, CliFixtureWithUpstream, NetworkSnapshotOptions,
    PreparedNetworkRun, SnapshotOutputMode,
};

fn snapshot_dir() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/snapshots"))
}

fn push_opts() -> NetworkSnapshotOptions {
    NetworkSnapshotOptions {
        fsck_upstream: true,
    }
}

fn noop_opts() -> NetworkSnapshotOptions {
    NetworkSnapshotOptions::default()
}

fn empty_commit_on_bare(upstream: &Path, message: &str) {
    let tree = git(upstream, &["rev-parse", "HEAD^{tree}"]);
    let parent = git(upstream, &["rev-parse", "HEAD"]);
    let mut cmd = std::process::Command::new("git");
    cmd.args([
        "commit-tree",
        tree.trim(),
        "-p",
        parent.trim(),
        "-m",
        message,
    ])
    .current_dir(upstream);
    grit_test_support::configure_git_env(&mut cmd);
    let out = cmd.output().expect("git commit-tree");
    assert!(
        out.status.success(),
        "commit-tree: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let oid = String::from_utf8_lossy(&out.stdout).trim().to_string();
    git(upstream, &["update-ref", "refs/heads/main", &oid]);
}

#[test]
fn snapshot_clone_local() {
    let net = CliFixture::new().with_upstream();
    let upstream = net
        .upstream_path()
        .to_str()
        .expect("upstream utf8")
        .to_owned();
    net.snapshot_network("clone_local", snapshot_dir(), noop_opts(), |mode| {
        let dest = format!("cloned-{}", mode.suffix());
        PreparedNetworkRun::from_shared_upstream(
            &net,
            net.workspace_path().to_path_buf(),
            vec!["clone".into(), upstream.clone(), dest],
        )
    });
}

#[test]
fn snapshot_fetch_noop() {
    let net = CliFixture::new().with_upstream();
    net.snapshot_network("fetch_noop", snapshot_dir(), noop_opts(), |mode| {
        let client = client_synced(&net, mode);
        PreparedNetworkRun::from_shared_upstream(&net, client, vec!["fetch".into()])
    });
}

#[test]
fn snapshot_fetch_new() {
    CliFixture::new().with_upstream().snapshot_network(
        "fetch_new",
        snapshot_dir(),
        noop_opts(),
        |mode| {
            let net = Box::new(CliFixture::new().with_upstream());
            let client = client_synced(net.as_ref(), mode);
            empty_commit_on_bare(net.upstream_path(), "upstream-ahead");
            PreparedNetworkRun::from_owned_upstream(net, client, vec!["fetch".into()])
        },
    );
}

#[test]
fn snapshot_fetch_prune() {
    CliFixture::new().with_upstream().snapshot_network(
        "fetch_prune",
        snapshot_dir(),
        noop_opts(),
        |mode| {
            let net = Box::new(CliFixture::new().with_upstream());
            git(
                net.upstream_path(),
                &["update-ref", "refs/heads/prune-me", "HEAD"],
            );
            let client = net.git_clone_client(&format!("client-{}", mode.suffix()));
            git(&client, &["fetch", "-q", "origin"]);
            git(
                net.upstream_path(),
                &["update-ref", "-d", "refs/heads/prune-me"],
            );
            PreparedNetworkRun::from_owned_upstream(
                net,
                client,
                vec!["fetch".into(), "--prune".into()],
            )
        },
    );
}

#[test]
fn snapshot_pull_ff() {
    CliFixture::new().with_upstream().snapshot_network(
        "pull_ff",
        snapshot_dir(),
        noop_opts(),
        |mode| {
            let net = Box::new(CliFixture::new().with_upstream());
            let client = client_synced(net.as_ref(), mode);
            empty_commit_on_bare(net.upstream_path(), "pull-ff-tip");
            PreparedNetworkRun::from_owned_upstream(net, client, vec!["pull".into()])
        },
    );
}

#[test]
fn snapshot_push_ok() {
    CliFixture::new().with_upstream().snapshot_network(
        "push_ok",
        snapshot_dir(),
        push_opts(),
        |mode| prepared_push_with_commit(mode, "push ok", "push ok\n", "push.txt"),
    );
}

#[test]
fn snapshot_push_up_to_date() {
    CliFixture::new().with_upstream().snapshot_network(
        "push_up_to_date",
        snapshot_dir(),
        push_opts(),
        |_mode| {
            let net = Box::new(CliFixture::new().with_upstream());
            let client = synced_client(net.as_ref());
            PreparedNetworkRun::from_owned_upstream(net, client, vec!["push".into()])
        },
    );
}

#[test]
fn snapshot_push_rejected() {
    CliFixture::new().with_upstream().snapshot_network(
        "push_rejected",
        snapshot_dir(),
        push_opts(),
        |_mode| {
            let net = Box::new(CliFixture::new().with_upstream());
            let client = synced_client(net.as_ref());
            empty_commit_on_bare(net.upstream_path(), "remote-first");
            std::fs::write(client.join("local.txt"), "local only\n").unwrap();
            let add = grit(&client, &["add", "local.txt"]);
            assert!(add.ok(), "grit add: {}", add.stderr);
            let commit = grit(&client, &["commit", "-m", "local"]);
            assert!(commit.ok(), "grit commit: {}", commit.stderr);
            PreparedNetworkRun::from_owned_upstream(net, client, vec!["push".into()])
        },
    );
}

#[test]
fn snapshot_push_tags() {
    CliFixture::new().with_upstream().snapshot_network(
        "push_tags",
        snapshot_dir(),
        push_opts(),
        |_mode| {
            let net = Box::new(CliFixture::new().with_upstream());
            let client = synced_client(net.as_ref());
            git(&client, &["tag", "-a", "v0.2", "-m", "two"]);
            PreparedNetworkRun::from_owned_upstream(
                net,
                client,
                vec!["push".into(), "--tags".into()],
            )
        },
    );
}

#[test]
fn snapshot_push_delete() {
    CliFixture::new().with_upstream().snapshot_network(
        "push_delete",
        snapshot_dir(),
        push_opts(),
        |_mode| {
            let net = Box::new(CliFixture::new().with_upstream());
            let client = synced_client(net.as_ref());
            git(&client, &["checkout", "-b", "old-topic"]);
            git(&client, &["commit", "--allow-empty", "-qm", "topic"]);
            git(&client, &["push", "-u", "origin", "old-topic"]);
            git(&client, &["checkout", "main"]);
            git(&client, &["branch", "-D", "old-topic"]);
            let fsck_before = git_fsck(net.upstream_path(), true);
            assert!(fsck_before.ok);
            PreparedNetworkRun::from_owned_upstream(
                net,
                client,
                vec![
                    "push".into(),
                    "--delete".into(),
                    "old-topic".into(),
                    "--remote".into(),
                    "origin".into(),
                ],
            )
        },
    );
}

#[test]
fn snapshot_remote_refs_heads() {
    let net = CliFixture::new().with_upstream();
    let upstream = net
        .upstream_path()
        .to_str()
        .expect("upstream utf8")
        .to_owned();
    net.snapshot_network("remote_refs_heads", snapshot_dir(), noop_opts(), |_mode| {
        PreparedNetworkRun::from_shared_upstream(
            &net,
            net.workspace_path().to_path_buf(),
            vec![
                "remote".into(),
                "refs".into(),
                upstream.clone(),
                "--heads".into(),
            ],
        )
    });
}

#[test]
fn snapshot_remote_refs_tags() {
    let net = CliFixture::new().with_upstream();
    let upstream = net
        .upstream_path()
        .to_str()
        .expect("upstream utf8")
        .to_owned();
    net.snapshot_network("remote_refs_tags", snapshot_dir(), noop_opts(), |_mode| {
        PreparedNetworkRun::from_shared_upstream(
            &net,
            net.workspace_path().to_path_buf(),
            vec![
                "remote".into(),
                "refs".into(),
                upstream.clone(),
                "--tags".into(),
            ],
        )
    });
}

fn client_synced(net: &CliFixtureWithUpstream, mode: SnapshotOutputMode) -> PathBuf {
    let client = net.git_clone_client(&format!("client-{}", mode.suffix()));
    git(&client, &["fetch", "-q", "origin"]);
    git(&client, &["reset", "--hard", "origin/main"]);
    client
}

fn synced_client(net: &CliFixtureWithUpstream) -> PathBuf {
    client_synced(net, SnapshotOutputMode::Human)
}

fn prepared_push_with_commit(
    _mode: SnapshotOutputMode,
    message: &str,
    contents: &str,
    filename: &str,
) -> PreparedNetworkRun {
    let net = Box::new(CliFixture::new().with_upstream());
    let client = synced_client(net.as_ref());
    std::fs::write(client.join(filename), contents).unwrap();
    let add = grit(&client, &["add", filename]);
    assert!(add.ok(), "grit add: {}", add.stderr);
    let commit = grit(&client, &["commit", "-m", message]);
    assert!(commit.ok(), "grit commit: {}", commit.stderr);
    PreparedNetworkRun::from_owned_upstream(net, client, vec!["push".into()])
}
