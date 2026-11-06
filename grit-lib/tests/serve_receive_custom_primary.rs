//! [`receive_pack`](grit_lib::remote::serve::receive_pack) with a custom object
//! primary and injected ref store (issue #2506).

use grit_test_support::git;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::odb::store::{MemoryStore, ObjectStore};
use grit_lib::objects::odb::OdbBuilder;
use grit_lib::objects::{HashAlgo, ObjectId};
use grit_lib::refs::store::{MemoryRefStore, RawRef, RefNameRef, RefStore};
use grit_lib::remote::serve::{
    receive_pack, ProtocolVersion, ReceivePolicy, RefUpdateRefusal, ServeOptions, UnpackError,
};
use grit_lib::repo::Repository;

const ZERO: &str = "0000000000000000000000000000000000000000";

fn source_repo(dir: &Path, commits: usize) -> ObjectId {
    fs::create_dir_all(dir).expect("mkdir");
    git(dir, &["init", "-q", "-b", "main", "."]);
    for i in 0..commits {
        fs::write(dir.join(format!("f{i}")), format!("content {i}\n")).expect("write");
        git(dir, &["add", "."]);
        git(dir, &["commit", "-q", "-m", &format!("c{i}")]);
    }
    grit_test_support::rev_parse(dir, "HEAD")
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

fn memory_serve_repo(root: &Path) -> (Repository, Arc<MemoryStore>, Arc<MemoryRefStore>) {
    let store = Arc::new(MemoryStore::new(HashAlgo::Sha1));
    let refs = Arc::new(MemoryRefStore::new());
    let repo = Repository::open(
        root,
        &(RepositoryOptions::empty()
            .for_serving()
            .with_odb_builder(
                OdbBuilder::files(root.join("objects"))
                    .primary(store.clone())
                    .alternates(false),
            )
            .with_ref_store(refs.clone())
            .with_initial_branch("main")),
    )
    .expect("open");
    (repo, store, refs)
}

fn serve_opts() -> ServeOptions {
    ServeOptions {
        protocol: ProtocolVersion::V0,
        stateless_rpc: true,
        agent: "grit-test".to_owned(),
        ..ServeOptions::default()
    }
}

fn object_files(objects: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![objects.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = fs::read_dir(&dir) else {
            continue;
        };
        for ent in read.flatten() {
            let path = ent.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(
                    path.strip_prefix(objects)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    out.sort();
    out
}

fn count_store_objects(store: &MemoryStore) -> usize {
    let mut n = 0usize;
    store
        .for_each_object(&mut |_| {
            n += 1;
            std::ops::ControlFlow::Continue(())
        })
        .expect("for_each");
    n
}

#[test]
fn receive_pack_publishes_push_into_custom_primary() {
    let upstream = tempfile::tempdir().expect("tempdir");
    let tip = source_repo(upstream.path(), 5);
    let pack = pack_for(upstream.path(), &[tip.to_hex()]);

    let root = tempfile::tempdir().expect("tempdir");
    let git_dir = root.path().join("dummy.git");
    fs::create_dir_all(&git_dir).expect("mkdir");
    let (repo, store, refs) = memory_serve_repo(&git_dir);

    let body = push_body(&[(None, Some(tip), "refs/heads/main")], &pack);
    let outcome = receive_pack(
        &repo,
        &mut std::io::Cursor::new(body),
        &mut std::io::sink(),
        &serve_opts(),
        &ReceivePolicy::default(),
    )
    .expect("receive_pack");
    assert!(outcome.unpack_error.is_none());
    assert!(outcome.updates[0].error.is_none());

    assert_eq!(
        repo.ref_store()
            .resolve("refs/heads/main")
            .expect("resolve"),
        tip
    );
    assert!(store.read(&tip).expect("read").is_some());
    assert!(count_store_objects(store.as_ref()) > 0);
    assert!(
        object_files(&git_dir.join("objects")).is_empty(),
        "custom primary must not write loose shards or pack files"
    );

    let export = tempfile::tempdir().expect("tempdir");
    git(export.path(), &["init", "-q", "--bare", "."]);
    let rebuilt = grit_lib::objects::pack_objects::build_pack(
        repo.odb(),
        &[tip],
        &[],
        &grit_lib::objects::pack_objects::PackBuildOptions::default(),
    )
    .expect("pack");
    let outcome = grit_test_support::objects::write_pack_and_index(
        &export.path().join("objects"),
        "received",
        &rebuilt,
        grit_test_support::objects::HashAlgo::Sha1,
        &Default::default(),
    );
    assert!(outcome.index_ok, "{}", outcome.index_stderr);
    git(
        export.path(),
        &["update-ref", "refs/heads/main", &tip.to_hex()],
    );
    git(export.path(), &["fsck", "--full", "--strict"]);

    let main = refs
        .read_raw(RefNameRef::from_trusted("refs/heads/main"))
        .expect("read ref")
        .expect("main set");
    assert_eq!(
        match main {
            RawRef::Direct(oid) => oid,
            RawRef::Symbolic(_) => panic!("expected direct ref"),
        },
        tip
    );
}

#[test]
fn non_fast_forward_leaves_custom_store_unchanged() {
    let upstream = tempfile::tempdir().expect("tempdir");
    let tip = source_repo(upstream.path(), 3);
    let pack = pack_for(upstream.path(), &[tip.to_hex()]);

    let root = tempfile::tempdir().expect("tempdir");
    let git_dir = root.path().join("repo.git");
    fs::create_dir_all(&git_dir).expect("mkdir");
    let (repo, store, refs) = memory_serve_repo(&git_dir);

    let body = push_body(&[(None, Some(tip), "refs/heads/main")], &pack);
    receive_pack(
        &repo,
        &mut std::io::Cursor::new(&body),
        &mut std::io::sink(),
        &serve_opts(),
        &ReceivePolicy::default(),
    )
    .expect("first push");

    let old = tip;
    let orphan = tempfile::tempdir().expect("tempdir");
    let new_tip = source_repo(orphan.path(), 2);
    let rewrite = pack_for(orphan.path(), &[new_tip.to_hex()]);
    let body = push_body(&[(Some(old), Some(new_tip), "refs/heads/main")], &rewrite);
    let policy = ReceivePolicy {
        deny_non_fast_forwards: true,
        ..ReceivePolicy::default()
    };
    let outcome = receive_pack(
        &repo,
        &mut std::io::Cursor::new(body),
        &mut std::io::sink(),
        &serve_opts(),
        &policy,
    )
    .expect("receive_pack");
    assert!(outcome.unpack_error.is_none());
    assert!(matches!(
        outcome.updates[0].error,
        Some(RefUpdateRefusal::NonFastForward)
    ));
    assert!(count_store_objects(store.as_ref()) > 0);
    let kept = refs
        .read_raw(RefNameRef::from_trusted("refs/heads/main"))
        .expect("read")
        .expect("main");
    assert_eq!(
        match kept {
            RawRef::Direct(oid) => oid,
            RawRef::Symbolic(_) => panic!("expected direct ref"),
        },
        old
    );
}

#[test]
fn truncated_pack_leaves_custom_store_empty() {
    let upstream = tempfile::tempdir().expect("tempdir");
    let tip = source_repo(upstream.path(), 2);
    let mut pack = pack_for(upstream.path(), &[tip.to_hex()]);
    pack.truncate(pack.len().saturating_sub(20));

    let root = tempfile::tempdir().expect("tempdir");
    let git_dir = root.path().join("bare.git");
    fs::create_dir_all(&git_dir).expect("mkdir");
    let (repo, store, refs) = memory_serve_repo(&git_dir);

    let body = push_body(&[(None, Some(tip), "refs/heads/main")], &pack);
    let outcome = receive_pack(
        &repo,
        &mut std::io::Cursor::new(body),
        &mut std::io::sink(),
        &serve_opts(),
        &ReceivePolicy::default(),
    )
    .expect("receive_pack");
    assert!(matches!(
        outcome.unpack_error,
        Some(UnpackError::Truncated) | Some(UnpackError::Ingest(_))
    ));
    assert_eq!(count_store_objects(store.as_ref()), 0);
    assert!(refs
        .read_raw(RefNameRef::from_trusted("refs/heads/main"))
        .expect("read")
        .is_none());
}
