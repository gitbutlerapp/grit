//! `receive_pack_with_hook`: typed refusals, in-process policy hooks, streamed
//! pack storage, and hook environment, checked against the system `git`.

use grit_test_support::{git, rev_parse};
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
    ReceivePolicy, RefCommand, RefUpdateRefusal, ServeError, ServeOptions, UnpackError,
};
use grit_lib::repo::Repository;

const ZERO: &str = "0000000000000000000000000000000000000000";

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
    assert!(
        outcome.unpack_error.is_none(),
        "{:?} {report}",
        outcome.unpack_error
    );
    assert!(
        pack_files(&bare.join("objects")).is_empty(),
        "few-object push unpacks to loose, not pack"
    );
    fsck(&bare);
    assert_eq!(resolve_ref(&bare, "refs/heads/main").expect("main"), tip);
}

/// A `--depth 1` clone of `src` with one new commit on top, as a CI checkout
/// that pushes back. Returns the clone, its shallow boundary and the new tip.
fn shallow_client(tmp: &Path, src: &Path) -> (std::path::PathBuf, ObjectId, ObjectId) {
    let client = tmp.join("shallow-client");
    let url = format!("file://{}", src.display());
    git(
        tmp,
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            &url,
            client.to_str().expect("utf8 path"),
        ],
    );
    let boundary = rev_parse(&client, "HEAD");
    std::fs::write(client.join("pushed"), "from a shallow clone\n").expect("write");
    git(&client, &["add", "pushed"]);
    git(&client, &["commit", "-q", "-m", "pushed from shallow"]);
    let tip = rev_parse(&client, "HEAD");
    (client, boundary, tip)
}

/// A push body that announces `shallow` boundaries before the commands, as
/// `git send-pack` does from a shallow repository.
fn shallow_push_body(
    shallow: &[ObjectId],
    commands: &[(Option<ObjectId>, Option<ObjectId>, &str)],
    pack: &[u8],
) -> Vec<u8> {
    let mut body: Vec<u8> = shallow
        .iter()
        .flat_map(|oid| pkt_line(&format!("shallow {}\n", oid.to_hex())))
        .collect();
    body.extend(push_body(commands, pack));
    body
}

fn loose_ref_paths_under(git_dir: &Path, prefix: &str) -> Vec<std::path::PathBuf> {
    let root = git_dir.join(prefix.trim_start_matches('/'));
    let mut paths = Vec::new();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                paths.push(path);
            }
        }
    }
    paths
}

/// Issue #2309: many sibling ref creates in one non-atomic push must commit in
/// one packed-refs transaction, stay git-fsck clean, and scale roughly linearly.
#[test]
fn receive_pack_many_sibling_creates_use_one_packed_refs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("src");
    let tip = source_repo(&src, 1);
    let bare = tmp.path().join("bare.git");
    let repo = bare_repo(&bare);
    let count = 2_000usize;
    let names: Vec<String> = (0..count).map(|i| format!("refs/heads/b/n{i}")).collect();
    let commands: Vec<(Option<ObjectId>, Option<ObjectId>, &str)> = names
        .iter()
        .map(|name| (None, Some(tip), name.as_str()))
        .collect();
    let pack = pack_for(&src, &[tip.to_hex()]);
    let body = push_body(&commands, &pack);
    let (outcome, report) = serve(&repo, &body, &ReceivePolicy::default(), &());
    assert!(outcome.unpack_error.is_none(), "{report}");
    assert!(
        outcome.updates.iter().all(|u| u.error.is_none()),
        "first failure: {:?}, report: {report}",
        outcome.updates.iter().find(|u| u.error.is_some())
    );
    assert!(
        loose_ref_paths_under(&bare, "refs/heads/b").is_empty(),
        "sibling creates must land in packed-refs, not loose files"
    );
    assert!(bare.join("packed-refs").is_file());
    let listed = git(&bare, &["for-each-ref", "--format=%(refname)"])
        .lines()
        .filter(|l| !l.is_empty())
        .count();
    assert_eq!(listed, count);
    fsck(&bare);
}

#[test]
fn receive_pack_mass_create_time_scales_near_linear() {
    use std::time::Instant;

    fn push_n(n: usize) -> std::time::Duration {
        let tmp = tempfile::tempdir().expect("tempdir");
        let src = tmp.path().join("src");
        let tip = source_repo(&src, 1);
        let bare = tmp.path().join("bare.git");
        let repo = bare_repo(&bare);
        let names: Vec<String> = (0..n).map(|i| format!("refs/heads/b/n{i}")).collect();
        let commands: Vec<(Option<ObjectId>, Option<ObjectId>, &str)> = names
            .iter()
            .map(|name| (None, Some(tip), name.as_str()))
            .collect();
        let pack = pack_for(&src, &[tip.to_hex()]);
        let body = push_body(&commands, &pack);
        let start = Instant::now();
        let (outcome, report) = serve(&repo, &body, &ReceivePolicy::default(), &());
        assert!(outcome.unpack_error.is_none(), "{report}");
        assert!(
            outcome.updates.iter().all(|u| u.error.is_none()),
            "{report}"
        );
        start.elapsed()
    }

    let t_small = push_n(500);
    let t_large = push_n(2_000);
    let ratio = t_large.as_secs_f64() / t_small.as_secs_f64().max(0.001);
    assert!(
        ratio < 8.0,
        "expected near-linear scaling (ratio {ratio:.2} for 500→2000 refs): small={t_small:?} large={t_large:?}"
    );
}

#[test]
fn receive_pack_mixed_create_and_update_falls_back_from_initial_batch() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("src");
    let tip = source_repo(&src, 1);
    let bare = tmp.path().join("bare.git");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            "--bare",
            src.to_str().expect("utf8"),
            bare.to_str().expect("utf8"),
        ],
    );
    let old_main = resolve_ref(&bare, "refs/heads/main").expect("main");
    let repo = open(&bare);
    let commands: Vec<(Option<ObjectId>, Option<ObjectId>, &str)> = vec![
        (Some(old_main), Some(tip), "refs/heads/main"),
        (None, Some(tip), "refs/heads/extra/a"),
        (None, Some(tip), "refs/heads/extra/b"),
    ];
    let pack = pack_for(&src, &[tip.to_hex()]);
    let body = push_body(&commands, &pack);
    let (outcome, report) = serve(&repo, &body, &ReceivePolicy::default(), &());
    assert!(outcome.unpack_error.is_none(), "{report}");
    assert!(
        outcome.updates.iter().all(|u| u.error.is_none()),
        "{report}"
    );
    assert_eq!(resolve_ref(&bare, "refs/heads/main").expect("main"), tip);
    assert_eq!(resolve_ref(&bare, "refs/heads/extra/a").expect("a"), tip);
    fsck(&bare);
}

#[test]
fn receive_pack_non_atomic_batch_conflict_still_applies_valid_refs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("src");
    let tip = source_repo(&src, 1);
    let bare = tmp.path().join("bare.git");
    let repo = bare_repo(&bare);
    let commands: Vec<(Option<ObjectId>, Option<ObjectId>, &str)> = vec![
        (None, Some(tip), "refs/heads/ok"),
        (None, Some(tip), "refs/heads/ok/nested"),
    ];
    let pack = pack_for(&src, &[tip.to_hex()]);
    let body = push_body(&commands, &pack);
    let (outcome, report) = serve(&repo, &body, &ReceivePolicy::default(), &());
    assert!(outcome.unpack_error.is_none(), "{report}");
    assert_eq!(outcome.updates[0].error, None, "{report}");
    assert!(
        outcome.updates[1].error.is_some(),
        "ancestor conflict must refuse nested ref: {report}"
    );
    assert_eq!(resolve_ref(&bare, "refs/heads/ok").expect("ok"), tip);
    assert!(resolve_ref(&bare, "refs/heads/ok/nested").is_err());
    fsck(&bare);
}

/// Issue #2136: the server has the full history, the client is a depth-1
/// clone. Its `shallow` line must not be a protocol error, and the push needs
/// no shallow update, so the ref moves and the server stays complete.
#[test]
fn shallow_client_push_onto_full_history_is_applied() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("src");
    source_repo(&src, 3);
    let bare = tmp.path().join("bare.git");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            "--bare",
            src.to_str().expect("utf8"),
            bare.to_str().expect("utf8"),
        ],
    );
    let (client, boundary, tip) = shallow_client(tmp.path(), &src);
    let pack = pack_for(&client, &[tip.to_hex(), format!("^{}", boundary.to_hex())]);

    let repo = open(&bare);
    let body = shallow_push_body(&[boundary], &[(None, Some(tip), "refs/heads/t1")], &pack);
    let (outcome, report) = serve(&repo, &body, &ReceivePolicy::default(), &());

    assert!(outcome.unpack_error.is_none(), "{report}");
    assert_eq!(outcome.updates[0].error, None, "{report}");
    assert!(report.contains("ok refs/heads/t1"), "{report}");
    assert_eq!(resolve_ref(&bare, "refs/heads/t1").expect("t1"), tip);
    assert!(
        !bare.join("shallow").exists(),
        "no shallow update was needed"
    );
    fsck(&bare);
    assert_eq!(git(&bare, &["rev-list", "--count", "t1"]).trim(), "4");
}

#[test]
fn malformed_shallow_line_is_a_protocol_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let tip = source_repo(&tmp.path().join("src"), 1);
    let bare = tmp.path().join("bare.git");
    let repo = bare_repo(&bare);

    let mut body = pkt_line("shallow not-a-hex-object-id\n");
    body.extend(push_body(&[(None, Some(tip), "refs/heads/main")], &[]));
    let result = receive_pack(
        &repo,
        &mut std::io::Cursor::new(body),
        &mut Vec::new(),
        &serve_opts(),
        &ReceivePolicy::default(),
    );
    assert!(matches!(result, Err(ServeError::Protocol(_))), "{result:?}");
    assert!(resolve_ref(&bare, "refs/heads/main").is_err());
}

/// Into an empty repository, a shallow client's history would make the server
/// shallow: refused by default, with Git's reason, and nothing changes.
#[test]
fn shallow_push_that_needs_a_shallow_update_is_refused_by_default() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("src");
    source_repo(&src, 3);
    let (client, boundary, tip) = shallow_client(tmp.path(), &src);
    let bare = tmp.path().join("bare.git");
    let repo = bare_repo(&bare);

    let pack = pack_for(&client, &[tip.to_hex()]);
    let body = shallow_push_body(&[boundary], &[(None, Some(tip), "refs/heads/t")], &pack);
    let (outcome, report) = serve(&repo, &body, &ReceivePolicy::default(), &());

    assert!(outcome.unpack_error.is_none(), "{:?}", outcome.unpack_error);
    assert_eq!(
        outcome.updates[0].error,
        Some(RefUpdateRefusal::ShallowUpdateNotAllowed)
    );
    assert!(
        report.contains("ng refs/heads/t shallow update not allowed"),
        "{report}"
    );
    assert!(resolve_ref(&bare, "refs/heads/t").is_err());
    assert!(!bare.join("shallow").exists());
    assert!(!has_quarantine_leftovers(&bare.join("objects")));
}

/// `receive.shallowUpdate`: the client's boundary is recorded in the server's
/// `shallow` file, so `git fsck` accepts the missing parents, on both storage
/// paths. Hook scripts see the boundary through `GIT_SHALLOW_FILE`.
#[cfg(unix)]
#[test]
fn shallow_update_records_the_new_boundary_on_both_storage_paths() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("src");
    source_repo(&src, 3);
    let (client, boundary, tip) = shallow_client(tmp.path(), &src);
    let pack = pack_for(&client, &[tip.to_hex()]);

    for unpack_limit in ["100", "1"] {
        let bare = tmp.path().join(format!("bare-{unpack_limit}.git"));
        bare_repo(&bare);
        git(&bare, &["config", "receive.unpackLimit", unpack_limit]);
        git(&bare, &["config", "receive.shallowUpdate", "true"]);
        let log = bare.join("hook-shallow-file");
        let hook = bare.join("hooks").join("pre-receive");
        std::fs::write(
            &hook,
            format!(
                "#!/bin/sh\ncat >/dev/null\ncat \"$GIT_SHALLOW_FILE\" >'{}'\n",
                log.display()
            ),
        )
        .expect("hook");
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let repo = open(&bare);
        let policy = ReceivePolicy::from_config(&repo.config().expect("config"));
        assert!(policy.shallow_update);

        let body = shallow_push_body(&[boundary], &[(None, Some(tip), "refs/heads/t")], &pack);
        let (outcome, report) = serve(&repo, &body, &policy, &());

        assert!(outcome.unpack_error.is_none(), "{report}");
        assert_eq!(outcome.updates[0].error, None, "{report}");
        assert_eq!(resolve_ref(&bare, "refs/heads/t").expect("t"), tip);
        assert_eq!(
            std::fs::read_to_string(bare.join("shallow")).expect("shallow file"),
            format!("{}\n", boundary.to_hex()),
            "unpackLimit={unpack_limit}"
        );
        assert_eq!(
            std::fs::read_to_string(&log).expect("hook log"),
            format!("{}\n", boundary.to_hex())
        );
        fsck(&bare);
        assert_eq!(git(&bare, &["rev-list", "--count", "t"]).trim(), "2");
        let leftovers: Vec<_> = std::fs::read_dir(&bare)
            .expect("read bare")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("shallow_"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }
}

#[test]
fn non_atomic_receive_pack_many_creates_one_reftable_table() {
    use grit_lib::refs::ref_storage::RefStorageFormat;
    use grit_lib::repo::InitOptions;

    let tmp = tempfile::tempdir().expect("tempdir");
    let tip = source_repo(&tmp.path().join("src"), 1);
    let pack = pack_for(&tmp.path().join("src"), &[tip.to_hex()]);
    let bare = tmp.path().join("rt.git");
    std::fs::create_dir_all(&bare).expect("mkdir");
    Repository::init(
        &bare,
        &InitOptions::new()
            .with_bare(true)
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Reftable),
    )
    .expect("reftable bare init");
    let repo = open(&bare);
    let table_count = |dir: &Path| {
        std::fs::read_to_string(dir.join("reftable/tables.list"))
            .map(|s| s.lines().filter(|l| !l.is_empty()).count())
            .unwrap_or(0)
    };
    let before = table_count(&bare);

    let names: Vec<String> = (0..32).map(|i| format!("refs/heads/b{i}")).collect();
    let commands: Vec<(Option<ObjectId>, Option<ObjectId>, &str)> = names
        .iter()
        .map(|name| (None, Some(tip), name.as_str()))
        .collect();
    let body = push_body(&commands, &pack);
    let (outcome, _report) = serve(&repo, &body, &ReceivePolicy::default(), &());
    assert!(
        outcome.unpack_error.is_none(),
        "unpack failed: {:?}",
        outcome.unpack_error
    );
    assert_eq!(
        outcome.updates.len(),
        32,
        "expected one result per ref command"
    );
    assert!(outcome.updates.iter().all(|u| u.error.is_none()));
    let after = table_count(&bare);
    assert!(
        after <= before + 1,
        "non-atomic receive-pack must batch ref updates (before={before} after={after}, \
         per-ref tables would be >= {})",
        before + 32
    );
    for i in 0..32 {
        assert_eq!(
            resolve_ref(&bare, &format!("refs/heads/b{i}")).ok(),
            Some(tip)
        );
    }
}
