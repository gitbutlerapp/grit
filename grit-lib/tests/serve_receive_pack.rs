//! `receive_pack_with_hook`: typed refusals, in-process policy hooks, streamed
//! pack storage, and hook environment, checked against the system `git`.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::objects::odb::Odb;
use grit_lib::objects::ObjectId;
use grit_lib::refs::resolve_ref;
use grit_lib::remote::serve::{
    receive_pack, receive_pack_with_hook, ProtocolVersion, ReceiveHook, ReceiveOutcome,
    ReceivePolicy, RefCommand, RefUpdateRefusal, ServeOptions, UnpackError,
};
use grit_lib::repo::Repository;

const ZERO: &str = "0000000000000000000000000000000000000000";

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_AUTHOR_DATE", "2005-04-07T22:13:13 +0200")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_DATE", "2005-04-07T22:13:13 +0200")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8")
}

fn rev_parse(dir: &Path, rev: &str) -> ObjectId {
    ObjectId::from_hex(git(dir, &["rev-parse", rev]).trim()).expect("oid")
}

/// A source repository with `commits` commits on `main`, each adding a file.
fn source_repo(dir: &Path, commits: usize) -> ObjectId {
    std::fs::create_dir_all(dir).expect("mkdir");
    git(dir, &["init", "-q", "-b", "main", "."]);
    for i in 0..commits {
        std::fs::write(dir.join(format!("f{i}")), format!("content {i}\n")).expect("write");
        git(dir, &["add", "."]);
        git(dir, &["commit", "-q", "-m", &format!("c{i}")]);
    }
    rev_parse(dir, "HEAD")
}

/// The pack `git push` would send for `revs` (`^excluded` entries allowed).
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

/// A push request body: one command line per `(old, new, ref)`, then the pack.
fn push_body(commands: &[(Option<ObjectId>, Option<ObjectId>, &str)], pack: &[u8]) -> Vec<u8> {
    let hex = |o: Option<ObjectId>| o.map_or_else(|| ZERO.to_owned(), |o| o.to_hex());
    let mut body = Vec::new();
    for (i, (old, new, refname)) in commands.iter().enumerate() {
        let caps = if i == 0 { "\0report-status" } else { "" };
        body.extend(pkt_line(&format!(
            "{} {} {refname}{caps}\n",
            hex(*old),
            hex(*new)
        )));
    }
    body.extend_from_slice(b"0000");
    body.extend_from_slice(pack);
    body
}

fn bare_repo(path: &Path) -> Repository {
    std::fs::create_dir_all(path).expect("mkdir");
    git(path, &["init", "-q", "--bare", "."]);
    open(path)
}

fn open(path: &Path) -> Repository {
    let mut env = Environment::empty();
    env.cwd = path.to_path_buf();
    Repository::open(path, &RepositoryOptions::with_environment(env)).expect("open")
}

fn serve_opts() -> ServeOptions {
    ServeOptions {
        protocol: ProtocolVersion::V0,
        stateless_rpc: true,
        agent: "grit-test".to_owned(),
        ..ServeOptions::default()
    }
}

fn serve(
    repo: &Repository,
    body: &[u8],
    policy: &ReceivePolicy,
    hook: &dyn ReceiveHook,
) -> (ReceiveOutcome, String) {
    let mut output = Vec::new();
    let outcome = receive_pack_with_hook(
        repo,
        &mut std::io::Cursor::new(body),
        &mut output,
        &serve_opts(),
        policy,
        hook,
    )
    .expect("receive_pack");
    (outcome, String::from_utf8_lossy(&output).into_owned())
}

fn fsck(dir: &Path) {
    git(dir, &["fsck", "--strict", "--no-progress"]);
}

fn pack_files(objects: &Path) -> Vec<String> {
    std::fs::read_dir(objects.join("pack"))
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn has_quarantine_leftovers(objects: &Path) -> bool {
    std::fs::read_dir(objects)
        .map(|rd| {
            rd.filter_map(|e| e.ok()).any(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                name.starts_with("tmp_objdir") || name == "incoming"
            })
        })
        .unwrap_or(false)
}

/// Refuses every update to refs under `refs/heads/frozen/`.
struct FreezeHook;

impl ReceiveHook for FreezeHook {
    fn pre_receive(&self, commands: &[RefCommand], _odb: &Odb) -> Result<(), RefUpdateRefusal> {
        if commands
            .iter()
            .any(|c| c.refname.starts_with("refs/heads/frozen/"))
        {
            return Err(RefUpdateRefusal::Policy(
                "frozen branches are read-only".into(),
            ));
        }
        Ok(())
    }
}

#[test]
fn pre_receive_hook_refusal_refuses_whole_push_and_keeps_objects_out() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let tip = source_repo(&tmp.path().join("src"), 2);
    let pack = pack_for(&tmp.path().join("src"), &[tip.to_hex()]);
    let bare = tmp.path().join("bare.git");
    let repo = bare_repo(&bare);

    let body = push_body(
        &[
            (None, Some(tip), "refs/heads/main"),
            (None, Some(tip), "refs/heads/frozen/v1"),
        ],
        &pack,
    );
    let (outcome, report) = serve(&repo, &body, &ReceivePolicy::default(), &FreezeHook);

    let refusal = Some(RefUpdateRefusal::Policy(
        "frozen branches are read-only".into(),
    ));
    assert!(outcome.unpack_error.is_none());
    assert_eq!(outcome.updates[0].error, refusal);
    assert_eq!(outcome.updates[1].error, refusal);
    assert!(report.contains("ng refs/heads/main frozen branches are read-only"));
    assert!(resolve_ref(&bare, "refs/heads/main").is_err());
    assert!(
        !open(&bare).odb().exists(&tip),
        "refused objects stay quarantined"
    );
    assert!(!has_quarantine_leftovers(&bare.join("objects")));
}

#[test]
fn pre_receive_hook_sees_quarantined_objects_and_only_accepted_commands() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let tip = source_repo(&tmp.path().join("src"), 1);
    let pack = pack_for(&tmp.path().join("src"), &[tip.to_hex()]);
    let bare = tmp.path().join("bare.git");
    let repo = bare_repo(&bare);

    struct Recorder(Mutex<Vec<(String, bool)>>);
    impl ReceiveHook for Recorder {
        fn pre_receive(&self, commands: &[RefCommand], odb: &Odb) -> Result<(), RefUpdateRefusal> {
            let mut seen = self.0.lock().unwrap_or_else(|e| e.into_inner());
            for c in commands {
                seen.push((c.refname.clone(), c.new.is_some_and(|n| odb.exists(&n))));
            }
            Ok(())
        }
    }
    let recorder = Recorder(Mutex::new(Vec::new()));
    let body = push_body(
        &[
            (None, Some(tip), "refs/heads/main"),
            (None, Some(tip), "not-a-ref"),
        ],
        &pack,
    );
    let (outcome, _) = serve(&repo, &body, &ReceivePolicy::default(), &recorder);

    assert_eq!(outcome.updates[0].error, None);
    assert_eq!(
        outcome.updates[1].error,
        Some(RefUpdateRefusal::InvalidRefName)
    );
    let seen = recorder.0.into_inner().unwrap_or_else(|e| e.into_inner());
    assert_eq!(seen, vec![("refs/heads/main".to_owned(), true)]);
    assert_eq!(resolve_ref(&bare, "refs/heads/main").expect("main"), tip);
    fsck(&bare);
}

#[test]
fn update_closure_refuses_one_ref_and_applies_the_rest() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let tip = source_repo(&tmp.path().join("src"), 1);
    let pack = pack_for(&tmp.path().join("src"), &[tip.to_hex()]);
    let bare = tmp.path().join("bare.git");
    let repo = bare_repo(&bare);

    let no_tags = |cmd: &RefCommand, odb: &Odb| -> Result<(), RefUpdateRefusal> {
        assert!(cmd.new.is_none_or(|n| odb.exists(&n)), "objects migrated");
        if cmd.refname.starts_with("refs/tags/") {
            return Err(RefUpdateRefusal::Policy("tags are managed by CI".into()));
        }
        Ok(())
    };
    let body = push_body(
        &[
            (None, Some(tip), "refs/heads/main"),
            (None, Some(tip), "refs/tags/v1"),
        ],
        &pack,
    );
    let (outcome, report) = serve(&repo, &body, &ReceivePolicy::default(), &no_tags);

    assert_eq!(outcome.updates[0].error, None);
    assert_eq!(
        outcome.updates[1].error,
        Some(RefUpdateRefusal::Policy("tags are managed by CI".into()))
    );
    assert!(report.contains("ok refs/heads/main"));
    assert!(report.contains("ng refs/tags/v1 tags are managed by CI"));
    assert_eq!(resolve_ref(&bare, "refs/heads/main").expect("main"), tip);
    assert!(resolve_ref(&bare, "refs/tags/v1").is_err());
    fsck(&bare);
}

#[test]
fn atomic_push_refuses_everything_when_the_update_hook_refuses_one() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let tip = source_repo(&tmp.path().join("src"), 1);
    let pack = pack_for(&tmp.path().join("src"), &[tip.to_hex()]);
    let bare = tmp.path().join("bare.git");
    let repo = bare_repo(&bare);

    let deny_topic = |cmd: &RefCommand, _: &Odb| -> Result<(), RefUpdateRefusal> {
        if cmd.refname == "refs/heads/topic" {
            return Err(RefUpdateRefusal::Policy("no".into()));
        }
        Ok(())
    };
    let mut body = pkt_line(&format!(
        "{ZERO} {} refs/heads/main\0report-status atomic\n",
        tip.to_hex()
    ));
    body.extend(pkt_line(&format!(
        "{ZERO} {} refs/heads/topic\n",
        tip.to_hex()
    )));
    body.extend_from_slice(b"0000");
    body.extend_from_slice(&pack);
    let (outcome, _) = serve(&repo, &body, &ReceivePolicy::default(), &deny_topic);

    assert_eq!(
        outcome.updates[0].error,
        Some(RefUpdateRefusal::AtomicTransactionFailed)
    );
    assert_eq!(
        outcome.updates[1].error,
        Some(RefUpdateRefusal::Policy("no".into()))
    );
    assert!(resolve_ref(&bare, "refs/heads/main").is_err());
}

#[test]
fn policy_refusals_are_typed() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("src");
    let second = source_repo(&src, 2);
    let first = rev_parse(&src, "HEAD~1");
    let bare = tmp.path().join("bare.git");
    let repo = bare_repo(&bare);
    let policy = ReceivePolicy {
        deny_non_fast_forwards: true,
        deny_deletes: true,
        ..ReceivePolicy::default()
    };

    let pack = pack_for(&src, &[second.to_hex()]);
    let body = push_body(
        &[
            (None, Some(second), "refs/heads/main"),
            (None, Some(first), "refs/heads/old"),
            (None, Some(first), "refs/heads/other"),
        ],
        &pack,
    );
    let (outcome, _) = serve(&repo, &body, &policy, &());
    assert!(outcome.updates.iter().all(|u| u.error.is_none()));

    let empty = pack_for(&src, &[second.to_hex(), format!("^{}", second.to_hex())]);
    let missing = ObjectId::from_hex("1111111111111111111111111111111111111111").expect("oid");
    let body = push_body(
        &[
            (Some(second), Some(first), "refs/heads/main"),
            (Some(first), None, "refs/heads/old"),
            (Some(second), Some(first), "refs/heads/other"),
            (None, Some(missing), "refs/heads/ghost"),
        ],
        &empty,
    );
    let (outcome, _) = serve(&repo, &body, &policy, &());
    let errors: Vec<_> = outcome.updates.into_iter().map(|u| u.error).collect();
    assert_eq!(
        errors,
        vec![
            Some(RefUpdateRefusal::NonFastForward),
            Some(RefUpdateRefusal::DeletionProhibited),
            Some(RefUpdateRefusal::StaleInfo),
            Some(RefUpdateRefusal::MissingObjects),
        ]
    );
}

#[test]
fn truncated_pack_is_a_typed_unpack_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let tip = source_repo(&tmp.path().join("src"), 1);
    let pack = pack_for(&tmp.path().join("src"), &[tip.to_hex()]);
    let bare = tmp.path().join("bare.git");
    let repo = bare_repo(&bare);

    let body = push_body(
        &[(None, Some(tip), "refs/heads/main")],
        &pack[..pack.len() - 3],
    );
    let (outcome, report) = serve(&repo, &body, &ReceivePolicy::default(), &());
    assert!(matches!(outcome.unpack_error, Some(UnpackError::Truncated)));
    assert_eq!(
        outcome.updates[0].error,
        Some(RefUpdateRefusal::UnpackerError)
    );
    assert!(report.contains("unpack unexpected end of pack stream"));
    assert!(report.contains("ng refs/heads/main unpacker error"));
    assert!(!has_quarantine_leftovers(&bare.join("objects")));
}

#[test]
fn corrupt_pack_checksum_is_an_ingest_error_for_both_storage_paths() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let tip = source_repo(&tmp.path().join("src"), 1);
    let mut pack = pack_for(&tmp.path().join("src"), &[tip.to_hex()]);
    let last = pack.len() - 1;
    pack[last] ^= 0xff;
    for unpack_limit in ["100", "1"] {
        let bare = tmp.path().join(format!("bare-{unpack_limit}.git"));
        bare_repo(&bare);
        git(&bare, &["config", "receive.unpackLimit", unpack_limit]);
        let repo = open(&bare);
        let body = push_body(&[(None, Some(tip), "refs/heads/main")], &pack);
        let (outcome, _) = serve(&repo, &body, &ReceivePolicy::default(), &());
        assert!(
            matches!(outcome.unpack_error, Some(UnpackError::Ingest(_))),
            "unpackLimit={unpack_limit}: {:?}",
            outcome.unpack_error
        );
        assert!(resolve_ref(&bare, "refs/heads/main").is_err());
        assert!(!has_quarantine_leftovers(&bare.join("objects")));
    }
}

#[test]
fn max_input_size_is_a_typed_unpack_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let tip = source_repo(&tmp.path().join("src"), 3);
    let pack = pack_for(&tmp.path().join("src"), &[tip.to_hex()]);
    let bare = tmp.path().join("bare.git");
    bare_repo(&bare);
    git(&bare, &["config", "receive.maxInputSize", "64"]);
    let repo = open(&bare);

    let body = push_body(&[(None, Some(tip), "refs/heads/main")], &pack);
    let (outcome, _) = serve(&repo, &body, &ReceivePolicy::default(), &());
    assert!(matches!(
        outcome.unpack_error,
        Some(UnpackError::TooLarge { limit: 64 })
    ));
    assert!(resolve_ref(&bare, "refs/heads/main").is_err());
}

/// Above `receive.unpackLimit` the pushed pack is installed as a pack with an
/// index; below it the objects are stored loose. Both must satisfy `git fsck`.
#[test]
fn pushed_pack_is_stored_streamed_and_fsck_clean_on_both_paths() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("src");
    let tip = source_repo(&src, 40);
    let pack = pack_for(&src, &[tip.to_hex()]);

    for (unpack_limit, expect_pack) in [("1000", false), ("10", true)] {
        let bare = tmp.path().join(format!("bare-{unpack_limit}.git"));
        bare_repo(&bare);
        git(&bare, &["config", "receive.unpackLimit", unpack_limit]);
        let repo = open(&bare);
        let body = push_body(&[(None, Some(tip), "refs/heads/main")], &pack);
        let (outcome, report) = serve(&repo, &body, &ReceivePolicy::default(), &());
        assert!(outcome.unpack_error.is_none(), "{:?}", outcome.unpack_error);
        assert!(report.contains("unpack ok"), "{report}");

        let objects = bare.join("objects");
        let packs = pack_files(&objects);
        assert_eq!(
            packs.iter().any(|n| n.ends_with(".pack")),
            expect_pack,
            "unpackLimit={unpack_limit}: {packs:?}"
        );
        assert!(
            packs.iter().all(|n| !n.starts_with("tmp_")),
            "no spool files may reach objects/pack: {packs:?}"
        );
        assert!(!has_quarantine_leftovers(&objects));
        assert_eq!(resolve_ref(&bare, "refs/heads/main").expect("main"), tip);
        fsck(&bare);
        assert_eq!(git(&bare, &["rev-list", "--count", "main"]).trim(), "40");
    }
}

#[test]
fn thin_pack_push_is_completed_from_the_repository() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("src");
    let base = source_repo(&src, 3);
    let bare = tmp.path().join("bare.git");
    bare_repo(&bare);
    git(&bare, &["config", "receive.unpackLimit", "1"]);
    let repo = open(&bare);
    let body = push_body(
        &[(None, Some(base), "refs/heads/main")],
        &pack_for(&src, &[base.to_hex()]),
    );
    let (outcome, _) = serve(&repo, &body, &ReceivePolicy::default(), &());
    assert!(outcome.updates[0].error.is_none());

    std::fs::write(src.join("f0"), "content 0, edited at length so it deltas\n").expect("edit");
    git(&src, &["commit", "-q", "-am", "edit"]);
    let tip = rev_parse(&src, "HEAD");
    let mut child = Command::new("git")
        .current_dir(&src)
        .args(["pack-objects", "--revs", "--thin", "--stdout", "-q"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("pack-objects");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(format!("{}\n^{}\n", tip.to_hex(), base.to_hex()).as_bytes())
        .expect("revs");
    let thin = child.wait_with_output().expect("wait").stdout;

    let repo = open(&bare);
    let body = push_body(&[(Some(base), Some(tip), "refs/heads/main")], &thin);
    let (outcome, _) = serve(&repo, &body, &ReceivePolicy::default(), &());
    assert!(outcome.unpack_error.is_none(), "{:?}", outcome.unpack_error);
    assert!(outcome.updates[0].error.is_none(), "{:?}", outcome.updates);
    fsck(&bare);
}

#[cfg(unix)]
#[test]
fn hook_scripts_see_git_dir_dot() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().expect("tempdir");
    let tip = source_repo(&tmp.path().join("src"), 1);
    let pack = pack_for(&tmp.path().join("src"), &[tip.to_hex()]);
    let bare = tmp.path().join("bare.git");
    bare_repo(&bare);
    let log = bare.join("git-dir-seen");
    let hook = bare.join("hooks").join("pre-receive");
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\ncat >/dev/null\nprintf '%s' \"$GIT_DIR\" >'{}'\n",
            log.display()
        ),
    )
    .expect("hook");
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let repo = open(&bare);

    let body = push_body(&[(None, Some(tip), "refs/heads/main")], &pack);
    let mut output = Vec::new();
    let outcome = receive_pack(
        &repo,
        &mut std::io::Cursor::new(body),
        &mut output,
        &serve_opts(),
        &ReceivePolicy::default(),
    )
    .expect("receive_pack");
    assert!(outcome.updates[0].error.is_none());
    assert_eq!(std::fs::read_to_string(&log).expect("hook log"), ".");
}

/// Regression for issue #1945: a multi-megabyte blob in a small pack must unpack
/// to loose objects under the default `receive.unpackLimit` without requiring
/// the full inflated payload in memory.
#[test]
fn large_blob_push_under_unpack_limit_materializes_loose() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("src");
    std::fs::create_dir_all(&src).expect("mkdir");
    git(&src, &["init", "-q", "-b", "main", "."]);
    let payload: Vec<u8> = (0..3 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    std::fs::write(src.join("big.bin"), &payload).expect("write");
    git(&src, &["add", "big.bin"]);
    git(&src, &["commit", "-q", "-m", "big"]);
    let tip = rev_parse(&src, "HEAD");
    let pack = pack_for(&src, &[tip.to_hex()]);

    let bare = tmp.path().join("bare.git");
    bare_repo(&bare);
    let repo = open(&bare);
    let body = push_body(&[(None, Some(tip), "refs/heads/main")], &pack);
    let (outcome, report) = serve(&repo, &body, &ReceivePolicy::default(), &());
    assert!(outcome.unpack_error.is_none(), "{:?} {report}", outcome.unpack_error);
    assert!(
        pack_files(&bare.join("objects")).is_empty(),
        "few-object push unpacks to loose, not pack"
    );
    fsck(&bare);
    assert_eq!(resolve_ref(&bare, "refs/heads/main").expect("main"), tip);
}
