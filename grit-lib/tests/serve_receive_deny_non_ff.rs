//! `receive.denyNonFastForwards` in receive-pack: fast-forwards of pushed
//! (still quarantined) commits are accepted, history rewrites of branches are
//! refused, and other refs are left alone, as with `git receive-pack`.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::objects::{HashAlgo, ObjectId};
use grit_lib::refs::resolve_ref;
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
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    cmd
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = git_cmd(dir, args).output().expect("run git");
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

/// Pack of everything reachable from `tip` but not from `exclude`.
fn pack_between(source: &Path, tip: ObjectId, exclude: Option<ObjectId>) -> Vec<u8> {
    let mut revs = format!("{}\n", tip.to_hex());
    if let Some(base) = exclude {
        revs.push_str(&format!("^{}\n", base.to_hex()));
    }
    pack_objects(source, &revs)
}

/// Header-only pack, as a client sends when the server already has every object.
fn empty_pack(source: &Path) -> Vec<u8> {
    pack_objects(source, "")
}

fn pack_objects(source: &Path, revs: &str) -> Vec<u8> {
    let mut child = git_cmd(source, &["pack-objects", "--revs", "--stdout"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("pack-objects spawn");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(revs.as_bytes())
        .expect("write revs");
    let out = child.wait_with_output().expect("pack-objects wait");
    assert!(out.status.success(), "pack-objects failed");
    out.stdout
}

fn pkt_line(payload: &str) -> Vec<u8> {
    format!("{:04x}{}", payload.len() + 4, payload).into_bytes()
}

fn push_body(old: Option<ObjectId>, new: ObjectId, refname: &str, pack: &[u8]) -> Vec<u8> {
    let zero = ObjectId::null(HashAlgo::Sha1).to_hex();
    let old_s = old.map(|o| o.to_hex()).unwrap_or(zero);
    let mut body = pkt_line(&format!(
        "{old_s} {} {refname}\0report-status\n",
        new.to_hex()
    ));
    body.extend_from_slice(b"0000");
    body.extend_from_slice(pack);
    body
}

fn serve_push(repo: &Repository, body: &[u8]) -> ReceiveOutcome {
    let opts = ServeOptions {
        protocol: ProtocolVersion::V0,
        stateless_rpc: true,
        advertise_refs: false,
        agent: "grit-test".to_owned(),
        hidden_refs: Vec::new(),
        ..ServeOptions::default()
    };
    let policy = ReceivePolicy {
        deny_non_fast_forwards: true,
        ..ReceivePolicy::default()
    };
    let mut output = Vec::new();
    receive_pack(
        repo,
        &mut std::io::Cursor::new(body),
        &mut output,
        &opts,
        &policy,
    )
    .expect("receive_pack")
}

fn open_bare(path: &Path) -> Repository {
    std::fs::create_dir_all(path).expect("mkdir");
    git(path, &["init", "-q", "--bare", "-b", "main", "."]);
    open_existing(path)
}

fn open_existing(path: &Path) -> Repository {
    let mut env = Environment::empty();
    env.cwd = path.to_path_buf();
    Repository::open(path, &RepositoryOptions::with_environment(env)).expect("open bare")
}

fn commit(source: &Path, file: &str, body: &str) -> ObjectId {
    std::fs::write(source.join(file), body).expect("write file");
    git(source, &["add", file]);
    git(source, &["commit", "-q", "-m", body.trim()]);
    rev_parse(source, "HEAD")
}

/// Chain `count` commits on HEAD, all committed at `date`.
fn commit_chain_at(source: &Path, count: usize, date: &str) -> ObjectId {
    for i in 0..count {
        let out = git_cmd(
            source,
            &["commit", "-q", "--allow-empty", "-m", &format!("s{i}")],
        )
        .env("GIT_COMMITTER_DATE", date)
        .env("GIT_AUTHOR_DATE", date)
        .output()
        .expect("git commit");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    rev_parse(source, "HEAD")
}

struct Fixture {
    _tmp: tempfile::TempDir,
    bare: std::path::PathBuf,
    source: std::path::PathBuf,
    repo: Repository,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("srv.git");
    let repo = open_bare(&bare);
    let source = tmp.path().join("w");
    std::fs::create_dir_all(&source).expect("mkdir");
    git(&source, &["init", "-q", "-b", "main", "."]);
    Fixture {
        _tmp: tmp,
        bare,
        source,
        repo,
    }
}

#[test]
fn create_then_fast_forward_is_accepted_and_git_fsck_passes() {
    let f = fixture();
    let one = commit(&f.source, "a", "1\n");
    let outcome = serve_push(
        &f.repo,
        &push_body(
            None,
            one,
            "refs/heads/main",
            &pack_between(&f.source, one, None),
        ),
    );
    assert!(outcome.updates[0].error.is_none(), "{:?}", outcome.updates);
    assert_eq!(resolve_ref(&f.bare, "refs/heads/main").unwrap(), one);

    let two = commit(&f.source, "a", "1\n2\n");
    let three = commit(&f.source, "b", "3\n");
    let outcome = serve_push(
        &f.repo,
        &push_body(
            Some(one),
            three,
            "refs/heads/main",
            &pack_between(&f.source, three, Some(one)),
        ),
    );
    assert!(
        outcome.updates[0].error.is_none(),
        "fast-forward must be accepted: {:?}",
        outcome.updates
    );
    assert_eq!(resolve_ref(&f.bare, "refs/heads/main").unwrap(), three);
    assert!(f.repo.odb().exists(&two), "pushed objects must be migrated");
    git(&f.bare, &["fsck", "--strict"]);
}

/// Committer dates are not monotonic: descendants dated long before their
/// ancestor are still a fast-forward, with or without a server commit-graph.
#[test]
fn fast_forward_with_descendants_dated_before_the_old_tip_is_accepted() {
    for with_commit_graph in [false, true] {
        let f = fixture();
        let old = commit_chain_at(&f.source, 1, "2035-01-01T00:00:00Z");
        let outcome = serve_push(
            &f.repo,
            &push_body(
                None,
                old,
                "refs/heads/main",
                &pack_between(&f.source, old, None),
            ),
        );
        assert!(outcome.updates[0].error.is_none(), "{:?}", outcome.updates);
        if with_commit_graph {
            git(&f.bare, &["commit-graph", "write", "--reachable"]);
        }
        let server = open_existing(&f.bare);

        let new = commit_chain_at(&f.source, 150, "2000-01-01T00:00:00Z");
        let outcome = serve_push(
            &server,
            &push_body(
                Some(old),
                new,
                "refs/heads/main",
                &pack_between(&f.source, new, Some(old)),
            ),
        );
        assert!(
            outcome.updates[0].error.is_none(),
            "skewed fast-forward refused (commit-graph: {with_commit_graph}): {:?}",
            outcome.updates
        );
        assert_eq!(resolve_ref(&f.bare, "refs/heads/main").unwrap(), new);
        git(&f.bare, &["fsck", "--strict"]);
    }
}

#[test]
fn non_fast_forward_branch_update_is_refused() {
    let f = fixture();
    let one = commit(&f.source, "a", "1\n");
    let two = commit(&f.source, "a", "1\n2\n");
    let outcome = serve_push(
        &f.repo,
        &push_body(
            None,
            two,
            "refs/heads/main",
            &pack_between(&f.source, two, None),
        ),
    );
    assert!(outcome.updates[0].error.is_none(), "{:?}", outcome.updates);

    git(&f.source, &["reset", "-q", "--hard", &one.to_hex()]);
    let rewritten = commit(&f.source, "c", "rewritten\n");
    let outcome = serve_push(
        &f.repo,
        &push_body(
            Some(two),
            rewritten,
            "refs/heads/main",
            &pack_between(&f.source, rewritten, Some(one)),
        ),
    );
    assert_eq!(
        outcome.updates[0].error,
        Some(RefUpdateRefusal::NonFastForward)
    );
    assert_eq!(resolve_ref(&f.bare, "refs/heads/main").unwrap(), two);

    // Rewinding to an existing ancestor is a non-fast-forward too.
    let rewind = serve_push(
        &f.repo,
        &push_body(Some(two), one, "refs/heads/main", &empty_pack(&f.source)),
    );
    assert_eq!(
        rewind.updates[0].error,
        Some(RefUpdateRefusal::NonFastForward)
    );
    assert_eq!(resolve_ref(&f.bare, "refs/heads/main").unwrap(), two);
    git(&f.bare, &["fsck", "--strict"]);
}

#[test]
fn non_branch_ref_rewrite_is_not_subject_to_the_policy() {
    let f = fixture();
    let one = commit(&f.source, "a", "1\n");
    let outcome = serve_push(
        &f.repo,
        &push_body(
            None,
            one,
            "refs/tags/v1",
            &pack_between(&f.source, one, None),
        ),
    );
    assert!(outcome.updates[0].error.is_none(), "{:?}", outcome.updates);

    git(&f.source, &["checkout", "-q", "--orphan", "other"]);
    let unrelated = commit(&f.source, "z", "unrelated\n");
    let outcome = serve_push(
        &f.repo,
        &push_body(
            Some(one),
            unrelated,
            "refs/tags/v1",
            &pack_between(&f.source, unrelated, None),
        ),
    );
    assert!(
        outcome.updates[0].error.is_none(),
        "git only applies denyNonFastForwards to refs/heads/: {:?}",
        outcome.updates
    );
    assert_eq!(resolve_ref(&f.bare, "refs/tags/v1").unwrap(), unrelated);
}

#[test]
fn branch_update_to_a_non_commit_is_refused_as_bad_ref() {
    let f = fixture();
    let one = commit(&f.source, "a", "1\n");
    let outcome = serve_push(
        &f.repo,
        &push_body(
            None,
            one,
            "refs/heads/main",
            &pack_between(&f.source, one, None),
        ),
    );
    assert!(outcome.updates[0].error.is_none(), "{:?}", outcome.updates);

    let tree = rev_parse(&f.source, "HEAD^{tree}");
    let outcome = serve_push(
        &f.repo,
        &push_body(Some(one), tree, "refs/heads/main", &empty_pack(&f.source)),
    );
    assert_eq!(outcome.updates[0].error, Some(RefUpdateRefusal::BadRef));
    assert_eq!(resolve_ref(&f.bare, "refs/heads/main").unwrap(), one);
}
