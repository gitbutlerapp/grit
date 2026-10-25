//! `receive.fsckObjects` / `transfer.fsckObjects` in receive-pack (issue #1818):
//! malformed pushed objects are refused with an unpack error before any of
//! them enters the repository, on both the unpack-objects (small pack) and
//! index-pack (`receive.unpackLimit`) paths, with `receive.fsck.<msg-id>`
//! overrides and `receive.fsck.skipList` honored as in `git receive-pack`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::maintenance::fsck::{FsckMsgLevel, ObjectFsckPolicy};
use grit_lib::objects::{HashAlgo, ObjectId, ObjectKind};
use grit_lib::refs::resolve_ref;
use grit_lib::remote::serve::{
    receive_pack, ProtocolVersion, ReceiveOutcome, ReceivePolicy, ServeOptions,
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
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    cmd
}

fn git_stdin(dir: &Path, args: &[&str], input: &[u8]) -> Vec<u8> {
    let mut child = git_cmd(dir, args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn git");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(input)
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn git(dir: &Path, args: &[&str]) -> String {
    String::from_utf8(git_stdin(dir, args, b"")).expect("utf8")
}

fn oid(hex: &str) -> ObjectId {
    ObjectId::from_hex(hex.trim()).expect("oid")
}

fn rev_parse(dir: &Path, rev: &str) -> ObjectId {
    oid(&git(dir, &["rev-parse", rev]))
}

fn hash_literally(dir: &Path, kind: &str, body: &[u8]) -> ObjectId {
    let out = git_stdin(
        dir,
        &["hash-object", "-t", kind, "-w", "--literally", "--stdin"],
        body,
    );
    oid(&String::from_utf8(out).expect("utf8"))
}

fn pack_between(source: &Path, tip: ObjectId, exclude: ObjectId) -> Vec<u8> {
    let revs = format!("{}\n^{}\n", tip.to_hex(), exclude.to_hex());
    git_stdin(
        source,
        &["pack-objects", "--revs", "--stdout", "-q"],
        revs.as_bytes(),
    )
}

fn pkt_line(payload: &str) -> Vec<u8> {
    format!("{:04x}{}", payload.len() + 4, payload).into_bytes()
}

fn push_body(new: ObjectId, refname: &str, pack: &[u8]) -> Vec<u8> {
    let zero = ObjectId::null(HashAlgo::Sha1).to_hex();
    let mut body = pkt_line(&format!(
        "{zero} {} {refname}\0report-status\n",
        new.to_hex()
    ));
    body.extend_from_slice(b"0000");
    body.extend_from_slice(pack);
    body
}

struct Pushed {
    outcome: ReceiveOutcome,
    report: String,
}

fn serve_push(repo: &Repository, policy: &ReceivePolicy, body: &[u8]) -> Pushed {
    let opts = ServeOptions {
        protocol: ProtocolVersion::V0,
        stateless_rpc: true,
        advertise_refs: false,
        agent: "grit-test".to_owned(),
        hidden_refs: Vec::new(),
    };
    let mut output = Vec::new();
    let outcome = receive_pack(
        repo,
        &mut std::io::Cursor::new(body),
        &mut output,
        &opts,
        policy,
    )
    .expect("receive_pack");
    Pushed {
        outcome,
        report: String::from_utf8_lossy(&output).into_owned(),
    }
}

fn open_existing(path: &Path) -> Repository {
    let mut env = Environment::empty();
    env.cwd = path.to_path_buf();
    Repository::open(path, &RepositoryOptions::with_environment(env)).expect("open bare")
}

/// A bare server that already has `base`, and a work repo to build pushes in.
struct Fixture {
    _tmp: tempfile::TempDir,
    bare: PathBuf,
    source: PathBuf,
    base: ObjectId,
    tree: ObjectId,
}

impl Fixture {
    fn new(server_config: &[(&str, &str)]) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let source = tmp.path().join("w");
        std::fs::create_dir_all(&source).expect("mkdir");
        git(&source, &["init", "-q", "-b", "main", "."]);
        std::fs::write(source.join("f"), "x\n").expect("write");
        git(&source, &["add", "f"]);
        git(&source, &["commit", "-q", "-m", "base"]);
        let base = rev_parse(&source, "HEAD");
        let tree = rev_parse(&source, "HEAD^{tree}");

        let bare = tmp.path().join("srv.git");
        git(
            tmp.path(),
            &["clone", "-q", "--bare", "w", bare.to_str().expect("utf8")],
        );
        for (key, value) in server_config {
            git(&bare, &["config", key, value]);
        }
        Self {
            _tmp: tmp,
            bare,
            source,
            base,
            tree,
        }
    }

    fn server(&self) -> Repository {
        open_existing(&self.bare)
    }

    fn config_policy(&self) -> ReceivePolicy {
        ReceivePolicy::from_config(&self.server().config().expect("config"))
    }

    /// A commit on `base` whose author line has no email (`missingEmail`).
    fn commit_without_email(&self) -> ObjectId {
        let body = format!(
            "tree {}\nparent {}\nauthor Bad Guy no-email 1700000000 +0000\ncommitter Bad <b@c> 1700000000 +0000\n\nbad\n",
            self.tree.to_hex(),
            self.base.to_hex()
        );
        hash_literally(&self.source, "commit", body.as_bytes())
    }

    /// A clean commit on `base` pointing at the tree written from `entries`.
    fn commit_with_tree(&self, entries: &[(&str, &str, ObjectId)]) -> (ObjectId, ObjectId) {
        let mut tree = Vec::new();
        for (mode, name, target) in entries {
            tree.extend_from_slice(format!("{mode} {name}\0").as_bytes());
            tree.extend_from_slice(target.as_bytes());
        }
        let tree = hash_literally(&self.source, "tree", &tree);
        let body = format!(
            "tree {}\nparent {}\nauthor T <t@example.com> 1700000000 +0000\ncommitter T <t@example.com> 1700000000 +0000\n\ntree\n",
            tree.to_hex(),
            self.base.to_hex()
        );
        (
            hash_literally(&self.source, "commit", body.as_bytes()),
            tree,
        )
    }

    fn blob(&self) -> ObjectId {
        rev_parse(&self.source, "HEAD:f")
    }

    fn push(&self, policy: &ReceivePolicy, tip: ObjectId, refname: &str) -> Pushed {
        let pack = pack_between(&self.source, tip, self.base);
        serve_push(&self.server(), policy, &push_body(tip, refname, &pack))
    }

    fn quarantine_dirs(&self) -> Vec<String> {
        std::fs::read_dir(self.bare.join("objects"))
            .expect("objects dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("tmp_objdir"))
            .collect()
    }

    /// The push was refused as a whole and left the repository untouched.
    fn assert_refused(&self, pushed: &Pushed, bad: ObjectId, kind: ObjectKind, msg_id: &str) {
        let rejected = pushed
            .outcome
            .rejected_object
            .as_ref()
            .unwrap_or_else(|| panic!("push must be refused: {:?}", pushed.outcome));
        assert_eq!(rejected.oid, bad);
        assert_eq!(rejected.kind, kind);
        assert_eq!(rejected.error.id, msg_id);
        let unpack = pushed
            .outcome
            .unpack_error
            .as_deref()
            .expect("unpack error");
        assert!(unpack.contains(msg_id), "{unpack}");
        assert!(
            pushed
                .outcome
                .updates
                .iter()
                .all(|u| u.error.as_deref() == Some("unpacker error")),
            "{:?}",
            pushed.outcome.updates
        );
        assert!(pushed.report.contains("unpack "), "{}", pushed.report);
        assert!(!pushed.report.contains("unpack ok"), "{}", pushed.report);
        assert!(
            pushed.report.contains("ng refs/heads/bad unpacker error"),
            "{}",
            pushed.report
        );
        assert!(resolve_ref(&self.bare, "refs/heads/bad").is_err());
        assert!(
            !self.server().odb().exists(&bad),
            "refused object must not enter the repository"
        );
        assert!(
            self.quarantine_dirs().is_empty(),
            "quarantine must be removed"
        );
        git(&self.bare, &["fsck", "--strict"]);
    }

    fn assert_accepted(&self, pushed: &Pushed, tip: ObjectId) {
        assert!(
            pushed.outcome.unpack_error.is_none() && pushed.outcome.rejected_object.is_none(),
            "{:?}",
            pushed.outcome
        );
        assert!(pushed.outcome.updates[0].error.is_none());
        assert_eq!(resolve_ref(&self.bare, "refs/heads/bad").expect("ref"), tip);
    }
}

#[test]
fn receive_fsck_objects_refuses_missing_email_on_both_ingest_paths() {
    for unpack_limit in [None, Some("1")] {
        let mut config = vec![("receive.fsckObjects", "true")];
        if let Some(limit) = unpack_limit {
            config.push(("receive.unpackLimit", limit));
        }
        let f = Fixture::new(&config);
        let bad = f.commit_without_email();
        let policy = f.config_policy();
        assert!(policy.fsck_objects.is_some());
        let pushed = f.push(&policy, bad, "refs/heads/bad");
        f.assert_refused(&pushed, bad, ObjectKind::Commit, "missingEmail");
    }
}

#[test]
fn transfer_fsck_objects_applies_unless_receive_turns_it_off() {
    let f = Fixture::new(&[("transfer.fsckObjects", "true")]);
    let bad = f.commit_without_email();
    let pushed = f.push(&f.config_policy(), bad, "refs/heads/bad");
    f.assert_refused(&pushed, bad, ObjectKind::Commit, "missingEmail");

    let f = Fixture::new(&[
        ("transfer.fsckObjects", "true"),
        ("receive.fsckObjects", "false"),
    ]);
    let bad = f.commit_without_email();
    let policy = f.config_policy();
    assert!(policy.fsck_objects.is_none());
    let pushed = f.push(&policy, bad, "refs/heads/bad");
    f.assert_accepted(&pushed, bad);
}

#[test]
fn malformed_objects_are_accepted_when_fsck_is_off() {
    let f = Fixture::new(&[]);
    let bad = f.commit_without_email();
    let policy = f.config_policy();
    assert_eq!(policy, ReceivePolicy::default());
    let pushed = f.push(&policy, bad, "refs/heads/bad");
    f.assert_accepted(&pushed, bad);
}

#[test]
fn msg_id_override_turns_the_error_into_a_warning() {
    for level in ["warn", "ignore"] {
        let f = Fixture::new(&[
            ("receive.fsckObjects", "true"),
            ("receive.fsck.missingEmail", level),
        ]);
        let bad = f.commit_without_email();
        let pushed = f.push(&f.config_policy(), bad, "refs/heads/bad");
        f.assert_accepted(&pushed, bad);
    }
}

#[test]
fn skip_list_lets_known_objects_through() {
    let f = Fixture::new(&[
        ("receive.fsckObjects", "true"),
        ("receive.fsck.skipList", "known-bad"),
    ]);
    let bad = f.commit_without_email();
    std::fs::write(
        f.bare.join("known-bad"),
        format!("# imported history\n{}\n", bad.to_hex()),
    )
    .expect("write skip list");
    let pushed = f.push(&f.config_policy(), bad, "refs/heads/bad");
    f.assert_accepted(&pushed, bad);
}

#[test]
fn unreadable_skip_list_refuses_the_push() {
    let f = Fixture::new(&[
        ("receive.fsckObjects", "true"),
        ("receive.fsck.skipList", "does-not-exist"),
    ]);
    let (clean, _) = f.commit_with_tree(&[("100644", "f", f.blob())]);
    let pushed = f.push(&f.config_policy(), clean, "refs/heads/bad");
    assert!(pushed.outcome.rejected_object.is_none());
    let unpack = pushed
        .outcome
        .unpack_error
        .as_deref()
        .expect("unpack error");
    assert!(unpack.contains("skip list"), "{unpack}");
    assert!(resolve_ref(&f.bare, "refs/heads/bad").is_err());
}

#[test]
fn bad_trees_are_refused_with_strict_transfer_levels() {
    let f = Fixture::new(&[("receive.fsckObjects", "true")]);
    let blob = f.blob();
    // A warning in a local `git fsck`, an error for a transfer.
    let (tip, tree) = f.commit_with_tree(&[("0100644", "f", blob)]);
    let pushed = f.push(&f.config_policy(), tip, "refs/heads/bad");
    f.assert_refused(&pushed, tree, ObjectKind::Tree, "zeroPaddedFilemode");

    let (tip, tree) = f.commit_with_tree(&[("100644", "f", blob), ("100644", "f", blob)]);
    let pushed = f.push(&f.config_policy(), tip, "refs/heads/bad");
    f.assert_refused(&pushed, tree, ObjectKind::Tree, "duplicateEntries");
}

#[test]
fn ignoring_one_tree_problem_does_not_hide_another() {
    let config = [
        ("receive.fsckObjects", "true"),
        ("receive.fsck.zeroPaddedFilemode", "ignore"),
    ];
    let f = Fixture::new(&config);
    let (tip, _) = f.commit_with_tree(&[("0100644", "f", f.blob())]);
    let pushed = f.push(&f.config_policy(), tip, "refs/heads/bad");
    f.assert_accepted(&pushed, tip);

    let f = Fixture::new(&config);
    let blob = f.blob();
    let (tip, tree) = f.commit_with_tree(&[("0100644", "f", blob), ("100644", "f", blob)]);
    let pushed = f.push(&f.config_policy(), tip, "refs/heads/bad");
    f.assert_refused(&pushed, tree, ObjectKind::Tree, "duplicateEntries");
}

#[test]
fn clean_push_with_fsck_on_is_accepted_and_git_fsck_passes() {
    for unpack_limit in ["100", "1"] {
        let f = Fixture::new(&[
            ("receive.fsckObjects", "true"),
            ("receive.unpackLimit", unpack_limit),
        ]);
        std::fs::write(f.source.join("g"), "y\n").expect("write");
        git(&f.source, &["add", "g"]);
        git(&f.source, &["commit", "-q", "-m", "clean"]);
        let tip = rev_parse(&f.source, "HEAD");
        let pushed = f.push(&f.config_policy(), tip, "refs/heads/bad");
        f.assert_accepted(&pushed, tip);
        git(&f.bare, &["fsck", "--strict"]);
    }
}

#[test]
fn embedders_can_build_the_policy_in_code() {
    let f = Fixture::new(&[]);
    let bad = f.commit_without_email();
    let policy = ReceivePolicy {
        fsck_objects: Some(ObjectFsckPolicy::default()),
        ..ReceivePolicy::default()
    };
    let pushed = f.push(&policy, bad, "refs/heads/bad");
    f.assert_refused(&pushed, bad, ObjectKind::Commit, "missingEmail");

    let mut lenient = ObjectFsckPolicy::default();
    lenient.set_level("missingEmail", FsckMsgLevel::Warn);
    let policy = ReceivePolicy {
        fsck_objects: Some(lenient),
        ..ReceivePolicy::default()
    };
    let pushed = f.push(&policy, bad, "refs/heads/bad");
    f.assert_accepted(&pushed, bad);
}
