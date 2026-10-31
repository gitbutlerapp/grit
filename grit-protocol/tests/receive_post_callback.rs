//! Post-receive in-process hook refusals surface on [`ReceiveOutcome`].

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::objects::ObjectId;
use grit_lib::remote::serve::{
    ReceiveHook, ReceiveHookContext, ReceivePolicy, RefCommand, RefUpdateRefusal,
};
use grit_protocol::{receive_pack, ServeSession};

struct AuditHook;

impl ReceiveHook for AuditHook {
    fn post_receive(
        &self,
        _commands: &[&RefCommand],
        _odb: &grit_lib::objects::odb::Odb,
        _ctx: &ReceiveHookContext<'_>,
    ) -> Result<(), RefUpdateRefusal> {
        Err(RefUpdateRefusal::Policy("audit failed".into()))
    }
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git");
    assert!(status.success(), "git {args:?}");
}

fn rev_parse(dir: &std::path::Path, rev: &str) -> ObjectId {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["rev-parse", rev])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("rev-parse");
    assert!(out.status.success());
    ObjectId::from_hex(String::from_utf8(out.stdout).expect("utf8").trim()).expect("oid")
}

fn pack_for_commit(source: &std::path::Path, oid: ObjectId) -> Vec<u8> {
    let mut child = Command::new("git")
        .current_dir(source)
        .args(["pack-objects", "--revs", "--stdout"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .spawn()
        .expect("pack-objects");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(format!("{}\n", oid.to_hex()).as_bytes())
        .expect("write");
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success());
    out.stdout
}

fn pkt_line(payload: &str) -> Vec<u8> {
    format!("{:04x}{payload}", payload.len() + 4).into_bytes()
}

fn push_body(new: ObjectId, refname: &str, pack: &[u8]) -> Vec<u8> {
    let zero = "0000000000000000000000000000000000000000";
    let mut body = pkt_line(&format!(
        "{zero} {} {refname}\0report-status\n",
        new.to_hex()
    ));
    body.extend_from_slice(b"0000");
    body.extend_from_slice(pack);
    body
}

#[test]
fn post_receive_callback_error_is_recorded_on_outcome() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("bare.git");
    std::fs::create_dir_all(&bare).expect("mkdir");
    git(&bare, &["init", "-q", "--bare", "."]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let hook = bare.join("hooks/pre-receive");
        std::fs::write(&hook, "#!/bin/sh\ncat >/dev/null\n").expect("hook");
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    let source = tmp.path().join("src");
    std::fs::create_dir_all(&source).expect("mkdir");
    git(&source, &["init", "-q", "-b", "main", "."]);
    std::fs::write(source.join("f"), "x\n").expect("write");
    git(&source, &["add", "f"]);
    git(&source, &["commit", "-q", "-m", "c"]);
    let oid = rev_parse(&source, "HEAD");
    let pack = pack_for_commit(&source, oid);
    let body = push_body(oid, "refs/heads/main", &pack);

    let mut env = Environment::empty();
    env.cwd = bare.clone();
    let options = RepositoryOptions::with_environment(env).for_serving();
    let session = ServeSession {
        receive_hook: Some(Arc::new(AuditHook)),
        cancel: None,
    };
    let result = receive_pack::stateless_rpc_with_session(
        &bare,
        &mut std::io::Cursor::new(body),
        &options,
        &session,
    )
    .expect("push");
    assert!(result.outcome.updates[0].error.is_none());
    assert_eq!(
        result.outcome.post_receive_callback_error,
        Some(RefUpdateRefusal::Policy("audit failed".into()))
    );
    assert!(result.outcome.post_update_callback_error.is_none());

    assert_eq!(
        rev_parse(&bare, "refs/heads/main"),
        oid,
        "refs must stay applied when the post-receive callback refuses"
    );
}
