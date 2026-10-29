//! The connectivity check on pushes large enough to be installed as a pack
//! (`receive.unpackLimit`, 100 objects by default): a ref whose new tip reaches an
//! object in neither the pack nor the repository is refused, as `git receive-pack`
//! refuses it, while objects a promisor remote promises may be missing.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::maintenance::{offload_blobs, OffloadOptions};
use grit_lib::objects::pack::clear_pack_cache;
use grit_lib::objects::{HashAlgo, ObjectId};
use grit_lib::refs::resolve_ref;
use grit_lib::remote::partial_clone::ObjectFilterSpec;
use grit_lib::remote::serve::{
    receive_pack, ProtocolVersion, ReceiveOutcome, ReceivePolicy, RefUpdateRefusal, ServeOptions,
};
use grit_lib::repo::Repository;

fn git_cmd(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_AUTHOR_DATE", "2005-04-07T22:13:13 +0200")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_DATE", "2005-04-07T22:13:13 +0200")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_NO_LAZY_FETCH", "1");
    cmd
}

fn git_stdin(dir: &Path, args: &[&str], input: &[u8]) -> Vec<u8> {
    let mut child = git_cmd(dir, args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn git");
    child.stdin.as_mut().unwrap().write_all(input).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn git(dir: &Path, args: &[&str]) -> String {
    String::from_utf8(git_stdin(dir, args, b"")).unwrap()
}

fn oid(hex: &str) -> ObjectId {
    ObjectId::from_hex(hex.trim()).unwrap()
}

fn pkt_line(payload: &str) -> Vec<u8> {
    format!("{:04x}{}", payload.len() + 4, payload).into_bytes()
}

fn push_body(old: Option<ObjectId>, new: ObjectId, pack: &[u8]) -> Vec<u8> {
    let old = old.unwrap_or_else(|| ObjectId::null(HashAlgo::Sha1));
    let mut body = pkt_line(&format!(
        "{} {} refs/heads/main\0report-status\n",
        old.to_hex(),
        new.to_hex()
    ));
    body.extend_from_slice(b"0000");
    body.extend_from_slice(pack);
    body
}

fn serve_push(repo_dir: &Path, body: &[u8]) -> (ReceiveOutcome, String) {
    let mut env = Environment::empty();
    env.cwd = repo_dir.to_path_buf();
    let repo = Repository::open(repo_dir, &RepositoryOptions::with_environment(env)).unwrap();
    let opts = ServeOptions {
        protocol: ProtocolVersion::V0,
        stateless_rpc: true,
        advertise_refs: false,
        agent: "grit-test".to_owned(),
        ..ServeOptions::default()
    };
    let policy = ReceivePolicy::from_config(&repo.config().unwrap());
    let mut output = Vec::new();
    let outcome = receive_pack(
        &repo,
        &mut std::io::Cursor::new(body),
        &mut output,
        &opts,
        &policy,
    )
    .unwrap();
    (outcome, String::from_utf8_lossy(&output).into_owned())
}

/// `git receive-pack`'s report for the same push.
fn git_receive(repo_dir: &Path, body: &[u8]) -> String {
    let out = git_stdin(repo_dir, &["receive-pack", "--stateless-rpc", "."], body);
    String::from_utf8_lossy(&out).into_owned()
}

/// A work repository whose `main` adds 150 small files on top of `base`.
fn work_with_many_files(root: &Path) -> (PathBuf, ObjectId) {
    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main", "."]);
    std::fs::write(work.join("base.txt"), "base\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "base"]);
    let base = oid(&git(&work, &["rev-parse", "HEAD"]));
    for i in 0..150 {
        std::fs::write(work.join(format!("f{i:03}.txt")), format!("file {i}\n")).unwrap();
    }
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "many"]);
    (work, base)
}

#[cfg_attr(
    not(feature = "full-tests"),
    ignore = "starts system git or is heavy; run with --features full-tests"
)]
#[test]
fn a_large_push_missing_an_object_is_refused_like_git() {
    let tmp = tempfile::tempdir().unwrap();
    let (work, _) = work_with_many_files(tmp.path());
    let tip = oid(&git(&work, &["rev-parse", "HEAD"]));
    let dropped = oid(&git(&work, &["rev-parse", "HEAD:f042.txt"]));
    // Every object of `main` but one blob.
    let objects: String = git(&work, &["rev-list", "--objects", "HEAD"])
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|o| *o != dropped.to_hex())
        .map(|o| format!("{o}\n"))
        .collect();
    let pack = git_stdin(
        &work,
        &["pack-objects", "--stdout", "-q"],
        objects.as_bytes(),
    );
    let body = push_body(None, tip, &pack);

    let grit_dir = tmp.path().join("grit.git");
    let git_dir = tmp.path().join("git.git");
    for dir in [&grit_dir, &git_dir] {
        git(tmp.path(), &["init", "-q", "--bare", dir.to_str().unwrap()]);
    }
    let (outcome, report) = serve_push(&grit_dir, &body);
    assert!(outcome.unpack_error.is_none(), "{report}");
    assert_eq!(
        outcome.updates[0].error,
        Some(RefUpdateRefusal::Unconnected {
            missing: dropped,
            referrer: oid(&git(&work, &["rev-parse", "HEAD^{tree}"])),
        })
    );
    assert!(resolve_ref(&grit_dir, "refs/heads/main").is_err());
    let line = "ng refs/heads/main missing necessary objects";
    assert!(report.contains(line), "{report}");
    let from_git = git_receive(&git_dir, &body);
    assert!(from_git.contains(line), "{from_git}");
}

#[cfg_attr(
    not(feature = "full-tests"),
    ignore = "starts system git or is heavy; run with --features full-tests"
)]
#[test]
fn a_large_complete_push_is_accepted() {
    let tmp = tempfile::tempdir().unwrap();
    let (work, base) = work_with_many_files(tmp.path());
    let server = tmp.path().join("srv.git");
    git(&work, &["init", "-q", "--bare", server.to_str().unwrap()]);
    git(
        &work,
        &[
            "push",
            "-q",
            server.to_str().unwrap(),
            "HEAD~1:refs/heads/main",
        ],
    );
    let tip = oid(&git(&work, &["rev-parse", "HEAD"]));
    let revs = format!("{}\n^{}\n", tip.to_hex(), base.to_hex());
    let pack = git_stdin(
        &work,
        &["pack-objects", "--revs", "--stdout", "-q"],
        revs.as_bytes(),
    );
    let (outcome, report) = serve_push(&server, &push_body(Some(base), tip, &pack));
    assert!(outcome.unpack_error.is_none(), "{report}");
    assert_eq!(outcome.updates[0].error, None, "{report}");
    assert_eq!(resolve_ref(&server, "refs/heads/main").unwrap(), tip);
    git(&server, &["fsck", "--no-dangling"]);
}

#[cfg_attr(
    not(feature = "full-tests"),
    ignore = "starts system git or is heavy; run with --features full-tests"
)]
#[test]
fn a_large_push_naming_an_offloaded_blob_is_accepted() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main", "."]);
    let big: Vec<u8> = (0..64 * 1024u32).map(|i| (i * 13 % 251) as u8).collect();
    std::fs::write(work.join("big.bin"), &big).unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "big"]);
    let base = oid(&git(&work, &["rev-parse", "HEAD"]));
    let server = tmp.path().join("srv.git");
    git(
        tmp.path(),
        &["clone", "-q", "--bare", "--no-local", "work", "srv.git"],
    );
    git(tmp.path(), &["init", "-q", "--bare", "lop.git"]);
    git(&server, &["remote", "add", "lop", "../lop.git"]);
    {
        let repo = Repository::open(&server, &RepositoryOptions::empty()).unwrap();
        let mut opts = OffloadOptions::new("lop", 4_000_000_000);
        opts.filter = Some(ObjectFilterSpec::BlobLimit(10 * 1024));
        assert_eq!(offload_blobs(&repo, &opts).unwrap().dropped_from_main, 1);
        clear_pack_cache();
    }

    // 150 new files beside the offloaded blob: the new tree names it, the pack
    // does not carry it, and the repository only has it promised.
    for i in 0..150 {
        std::fs::write(work.join(format!("f{i:03}.txt")), format!("file {i}\n")).unwrap();
    }
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "many"]);
    let tip = oid(&git(&work, &["rev-parse", "HEAD"]));
    let revs = format!("{}\n^{}\n", tip.to_hex(), base.to_hex());
    let pack = git_stdin(
        &work,
        &["pack-objects", "--revs", "--stdout", "-q"],
        revs.as_bytes(),
    );
    let (outcome, report) = serve_push(&server, &push_body(Some(base), tip, &pack));
    assert!(outcome.unpack_error.is_none(), "{report}");
    assert_eq!(outcome.updates[0].error, None, "{report}");
    assert_eq!(resolve_ref(&server, "refs/heads/main").unwrap(), tip);
    git(&server, &["fsck", "--no-dangling"]);
}

fn push_body_commands(commands: &[(Option<ObjectId>, ObjectId, &str)], pack: &[u8]) -> Vec<u8> {
    let zero = ObjectId::null(HashAlgo::Sha1);
    let mut body = Vec::new();
    for (i, (old, new, name)) in commands.iter().enumerate() {
        let caps = if i == 0 { "\0report-status" } else { "" };
        body.extend(pkt_line(&format!(
            "{} {} {name}{caps}\n",
            old.unwrap_or(zero).to_hex(),
            new.to_hex()
        )));
    }
    body.extend_from_slice(b"0000");
    body.extend_from_slice(pack);
    body
}

/// A broken tip refused in one push leaves its objects in the repository (the
/// quarantine is migrated for the accepted refs, as Git migrates it). A later push
/// naming that tip with no new objects must still be refused: Git walks every new
/// tip down to what the existing refs reach, whatever the pack size.
#[cfg_attr(
    not(feature = "full-tests"),
    ignore = "starts system git or is heavy; run with --features full-tests"
)]
#[test]
fn a_tip_left_by_a_refused_push_is_refused_again_like_git() {
    let tmp = tempfile::tempdir().unwrap();
    let (work, _) = work_with_many_files(tmp.path());
    let good = oid(&git(&work, &["rev-parse", "HEAD"]));
    std::fs::write(work.join("gone.txt"), "never sent\n").unwrap();
    git(&work, &["add", "gone.txt"]);
    git(&work, &["commit", "-q", "-m", "bad"]);
    let bad = oid(&git(&work, &["rev-parse", "HEAD"]));
    let gone = oid(&git(&work, &["rev-parse", "HEAD:gone.txt"]));
    let objects: String = git(&work, &["rev-list", "--objects", "HEAD"])
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|o| *o != gone.to_hex())
        .map(|o| format!("{o}\n"))
        .collect();
    let pack = git_stdin(
        &work,
        &["pack-objects", "--stdout", "-q"],
        objects.as_bytes(),
    );
    let first = push_body_commands(
        &[
            (None, good, "refs/heads/good"),
            (None, bad, "refs/heads/bad"),
        ],
        &pack,
    );
    let empty_pack = git_stdin(&work, &["pack-objects", "--stdout", "-q"], b"");
    let second = push_body_commands(&[(None, bad, "refs/heads/bad")], &empty_pack);

    let grit_dir = tmp.path().join("grit.git");
    let git_dir = tmp.path().join("git.git");
    for dir in [&grit_dir, &git_dir] {
        git(tmp.path(), &["init", "-q", "--bare", dir.to_str().unwrap()]);
    }
    let refused = "ng refs/heads/bad missing necessary objects";

    let from_git = git_receive(&git_dir, &first);
    assert!(from_git.contains("ok refs/heads/good"), "{from_git}");
    assert!(from_git.contains(refused), "{from_git}");
    git(&git_dir, &["cat-file", "-e", &bad.to_hex()]);
    let from_git = git_receive(&git_dir, &second);
    assert!(from_git.contains(refused), "{from_git}");

    let (outcome, report) = serve_push(&grit_dir, &first);
    assert!(report.contains("ok refs/heads/good"), "{report}");
    assert!(report.contains(refused), "{report}");
    assert!(outcome.updates[1].error.is_some());
    clear_pack_cache();
    let (outcome, report) = serve_push(&grit_dir, &second);
    assert!(outcome.unpack_error.is_none(), "{report}");
    assert!(report.contains(refused), "{report}");
    assert!(matches!(
        outcome.updates[0].error,
        Some(RefUpdateRefusal::Unconnected { missing, .. }) if missing == gone
    ));
    assert!(resolve_ref(&grit_dir, "refs/heads/bad").is_err());
}
