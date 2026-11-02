//! Receive-pack must reject creating `Foo` and `foo` in one push when `core.ignorecase` is on (#2471 / #2490).

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::objects::ObjectId;
use grit_lib::remote::serve::{
    receive_pack_with_hook, ProtocolVersion, ReceivePolicy, ServeOptions,
};
use grit_lib::repo::Repository;
use grit_test_support::{git, rev_parse};
use std::io::{Cursor, Write};
use std::path::Path;
use std::process::{Command, Stdio};

const ZERO: &str = "0000000000000000000000000000000000000000";

fn bare_with_ignorecase(path: &Path) -> Repository {
    std::fs::create_dir_all(path).expect("mkdir");
    git(path, &["init", "-q", "--bare", "."]);
    git(path, &["config", "core.ignorecase", "true"]);
    let mut env = Environment::empty();
    env.cwd = path.to_path_buf();
    Repository::open(path, &RepositoryOptions::with_environment(env)).expect("open")
}

fn pack_for(source: &Path, revs: &[String]) -> Vec<u8> {
    let mut child = Command::new("git")
        .current_dir(source)
        .args(["pack-objects", "--revs", "--stdout", "-q"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .spawn()
        .expect("pack-objects");
    let input: String = revs.iter().map(|r| format!("{r}\n")).collect();
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write revs");
    let out = child.wait_with_output().expect("pack-objects wait");
    assert!(out.status.success(), "pack-objects failed");
    out.stdout
}

fn pkt_line(payload: &str) -> Vec<u8> {
    format!("{:04x}{payload}", payload.len() + 4).into_bytes()
}

#[test]
fn receive_pack_rejects_same_batch_casefold_branch_creates() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bare = dir.path().join("srv.git");
    let repo = bare_with_ignorecase(&bare);
    let source = dir.path().join("work");
    std::fs::create_dir_all(&source).expect("mkdir");
    git(&source, &["init", "-q"]);
    git(&source, &["commit", "-q", "--allow-empty", "-m", "a"]);
    let tip: ObjectId = rev_parse(&source, "HEAD");

    let pack = pack_for(&source, &[tip.to_hex()]);
    let mut body = Vec::new();
    body.extend(pkt_line(&format!(
        "{ZERO} {} refs/heads/Foo\0report-status\n",
        tip.to_hex()
    )));
    body.extend(pkt_line(&format!(
        "{ZERO} {} refs/heads/foo\n",
        tip.to_hex()
    )));
    body.extend_from_slice(b"0000");
    body.extend_from_slice(&pack);

    let mut report = Vec::new();
    let outcome = receive_pack_with_hook(
        &repo,
        &mut Cursor::new(body),
        &mut report,
        &ServeOptions {
            protocol: ProtocolVersion::V0,
            stateless_rpc: true,
            agent: "grit-test".to_owned(),
            ..ServeOptions::default()
        },
        &ReceivePolicy::default(),
        &(),
    )
    .expect("receive_pack");
    assert!(
        outcome.updates.iter().all(|u| u.error.is_some()),
        "both casefold creates must be refused: {:?}",
        outcome.updates
    );
    let text = String::from_utf8_lossy(&report);
    assert!(
        text.contains("ignorecase") || text.contains("CaseConflict"),
        "report should explain case collision: {text}"
    );
    let listed = git(
        &bare,
        &["for-each-ref", "--format=%(refname)", "refs/heads/"],
    );
    assert!(
        listed.trim().is_empty(),
        "neither colliding ref must be stored: {listed:?}"
    );
}
