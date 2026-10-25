//! Ref advertisements from `upload_pack` and `receive_pack` (issue 1820).
//!
//! `ls-refs` with `ref-prefix` must cost in proportion to the refs it returns, not the refs
//! stored, and every advertisement must match the system `git` byte for byte (apart from the
//! capability list, which names each implementation's own features).

use std::collections::HashMap;
use std::io::{Cursor, Write};
use std::ops::ControlFlow;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Result;
use grit_lib::objects::odb::store::{ObjectStore, WritableObjectStore};
use grit_lib::objects::odb::{OdbBuilder, WriteOptions};
use grit_lib::objects::{HashAlgo, Object, ObjectId, ObjectInfo, ObjectKind};
use grit_lib::remote::pkt_line;
use grit_lib::remote::serve::{
    receive_pack, upload_pack, ProtocolVersion, ReceivePolicy, ServeOptions,
};
use grit_lib::repo::{InitOptions, Repository, RepositoryBuilder};

/// In-memory object store that counts every lookup the server makes.
#[derive(Debug, Default)]
struct CountingStore {
    objects: RwLock<HashMap<ObjectId, (ObjectKind, Vec<u8>)>>,
    lookups: AtomicUsize,
}

impl CountingStore {
    fn lookups(&self) -> usize {
        self.lookups.load(Ordering::SeqCst)
    }

    fn get<T>(&self, oid: &ObjectId, f: impl FnOnce(&(ObjectKind, Vec<u8>)) -> T) -> Option<T> {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        let guard = self.objects.read().unwrap_or_else(|e| e.into_inner());
        guard.get(oid).map(f)
    }
}

impl ObjectStore for CountingStore {
    fn hash_algo(&self) -> HashAlgo {
        HashAlgo::Sha1
    }

    fn read(&self, oid: &ObjectId) -> Result<Option<Object>> {
        Ok(self.get(oid, |(kind, data)| Object {
            kind: *kind,
            data: data.clone(),
        }))
    }

    fn read_info(&self, oid: &ObjectId) -> Result<Option<ObjectInfo>> {
        Ok(self.get(oid, |(kind, data)| ObjectInfo {
            kind: *kind,
            size: data.len() as u64,
        }))
    }

    fn for_each_object(&self, f: &mut dyn FnMut(&ObjectId) -> ControlFlow<()>) -> Result<()> {
        let guard = self.objects.read().unwrap_or_else(|e| e.into_inner());
        for oid in guard.keys() {
            if f(oid).is_break() {
                break;
            }
        }
        Ok(())
    }
}

impl WritableObjectStore for CountingStore {
    fn write(&self, kind: ObjectKind, data: &[u8], _options: WriteOptions) -> Result<ObjectId> {
        let oid = grit_lib::objects::hash::hash_object(HashAlgo::Sha1, kind, data)?;
        self.objects
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .entry(oid)
            .or_insert_with(|| (kind, data.to_vec()));
        Ok(oid)
    }

    fn freshen(&self, oid: &ObjectId) -> Result<bool> {
        Ok(self.read_info(oid)?.is_some())
    }
}

fn ls_refs_request(args: &[&str]) -> Vec<u8> {
    let mut buf = Vec::new();
    pkt_line::write_line_to_vec(&mut buf, "command=ls-refs").expect("pkt");
    pkt_line::write_line_to_vec(&mut buf, "object-format=sha1").expect("pkt");
    buf.extend_from_slice(pkt_line::DELIM.as_bytes());
    for arg in args {
        pkt_line::write_line_to_vec(&mut buf, arg).expect("pkt");
    }
    buf.extend_from_slice(pkt_line::FLUSH.as_bytes());
    buf
}

fn serve_opts(protocol: ProtocolVersion, stateless_rpc: bool, hidden: &[&str]) -> ServeOptions {
    ServeOptions {
        protocol,
        stateless_rpc,
        advertise_refs: !stateless_rpc,
        agent: String::new(),
        hidden_refs: hidden.iter().map(|h| (*h).to_owned()).collect(),
        ..ServeOptions::default()
    }
}

fn grit_ls_refs(repo: &Repository, args: &[&str], hidden: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut opts = serve_opts(ProtocolVersion::V2, true, hidden);
    upload_pack(
        repo,
        &mut Cursor::new(ls_refs_request(args)),
        &mut out,
        &mut opts,
    )
    .expect("grit ls-refs");
    out
}

/// Data lines of a pkt-line stream (`None` for flush), without trailing newlines.
fn pkt_lines(mut body: &[u8]) -> Vec<Option<String>> {
    let mut lines = Vec::new();
    while let Some(pkt) = pkt_line::read_packet(&mut body).expect("pkt stream") {
        lines.push(match pkt {
            pkt_line::Packet::Data(s) => Some(s.trim_end_matches('\n').to_owned()),
            _ => None,
        });
    }
    lines
}

/// Repository over a [`CountingStore`] with `n` packed branches and `n` packed tags, all at
/// one commit, plus a loose branch.
fn many_ref_repo(n: usize) -> (tempfile::TempDir, Repository, Arc<CountingStore>, ObjectId) {
    let store = Arc::new(CountingStore::default());
    let dir = tempfile::tempdir().expect("tempdir");
    Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_bare(true)
            .with_initial_branch("main"),
    )
    .expect("init");
    let repo = RepositoryBuilder::new(RepositoryOptions::empty())
        .odb_builder(
            OdbBuilder::files(dir.path().join("objects"))
                .primary(Arc::clone(&store) as Arc<dyn WritableObjectStore>)
                .alternates(false),
        )
        .open(dir.path(), None)
        .expect("open over counting store");
    let tree = repo.odb().write(ObjectKind::Tree, b"").expect("tree");
    let commit = repo
        .odb()
        .write(
            ObjectKind::Commit,
            format!("tree {tree}\nauthor A <a@e> 1 +0000\ncommitter A <a@e> 1 +0000\n\nc\n")
                .as_bytes(),
        )
        .expect("commit");
    let mut packed = String::from("# pack-refs with: peeled fully-peeled sorted \n");
    for i in 0..n {
        packed.push_str(&format!("{commit} refs/heads/b{i:05}\n"));
    }
    for i in 0..n {
        packed.push_str(&format!("{commit} refs/tags/t{i:05}\n"));
    }
    std::fs::write(dir.path().join("packed-refs"), packed).expect("packed-refs");
    std::fs::write(dir.path().join("refs/heads/main"), format!("{commit}\n")).expect("main");
    (dir, repo, store, commit)
}

#[test]
fn ls_refs_prefix_reads_objects_only_for_the_refs_it_returns() {
    let (_dir, repo, store, commit) = many_ref_repo(500);

    for peel in [false, true] {
        let mut args = vec!["symrefs", "ref-prefix refs/tags/t00042"];
        if peel {
            args.push("peel");
        }
        let before = store.lookups();
        let out = grit_ls_refs(&repo, &args, &[]);
        let lookups = store.lookups() - before;
        assert_eq!(
            pkt_lines(&out),
            vec![Some(format!("{commit} refs/tags/t00042")), None],
            "peel={peel}"
        );
        let allowed = usize::from(peel);
        assert!(
            lookups <= allowed,
            "ls-refs for one of 1001 refs made {lookups} object lookups (peel={peel})"
        );
    }

    let before = store.lookups();
    let out = grit_ls_refs(&repo, &["ref-prefix HEAD"], &[]);
    assert_eq!(pkt_lines(&out), vec![Some(format!("{commit} HEAD")), None]);
    assert_eq!(store.lookups(), before, "HEAD alone must not list refs/");
}

#[test]
fn ls_refs_peels_packed_tags_without_reading_them() {
    let (dir, repo, store, commit) = many_ref_repo(100);
    let tag = repo
        .odb()
        .write(
            ObjectKind::Tag,
            format!("object {commit}\ntype commit\ntag v1\ntagger A <a@e> 1 +0000\n\nv1\n")
                .as_bytes(),
        )
        .expect("tag");
    let mut packed = String::from("# pack-refs with: peeled fully-peeled sorted \n");
    for i in 0..100 {
        packed.push_str(&format!("{commit} refs/tags/t{i:05}\n"));
    }
    packed.push_str(&format!("{tag} refs/tags/v1\n^{commit}\n"));
    std::fs::write(dir.path().join("packed-refs"), packed).expect("packed-refs");

    let before = store.lookups();
    let out = grit_ls_refs(&repo, &["peel", "ref-prefix refs/tags/"], &[]);
    let lines = pkt_lines(&out);
    assert_eq!(lines.len(), 102);
    assert_eq!(
        lines[100],
        Some(format!("{tag} refs/tags/v1 peeled:{commit}"))
    );
    assert_eq!(
        store.lookups(),
        before,
        "a fully-peeled packed-refs answers every peel without reading objects"
    );
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "A")
        .env("GIT_AUTHOR_EMAIL", "a@example.com")
        .env("GIT_COMMITTER_NAME", "A")
        .env("GIT_COMMITTER_EMAIL", "a@example.com")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

/// Run `git <service>` on `repo` with `input` on stdin and return its stdout.
fn git_serve(repo: &Path, args: &[&str], protocol: Option<&str>, input: &[u8]) -> Vec<u8> {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .arg(repo)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(p) = protocol {
        cmd.env("GIT_PROTOCOL", p);
    }
    let mut child = cmd.spawn().expect("spawn git");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input)
        .expect("write request");
    let out = child.wait_with_output().expect("git output");
    assert!(out.status.success(), "git {args:?} failed");
    out.stdout
}

/// Advertisement lines with the capability list after the first ref's NUL removed.
fn without_capabilities(body: &[u8]) -> Vec<Option<String>> {
    pkt_lines(body)
        .into_iter()
        .map(|line| line.map(|l| l.split('\0').next().unwrap_or_default().to_owned()))
        .collect()
}

/// A repository with branches, lightweight and annotated tags (nested, and one naming a
/// tree), some packed and some loose, a loose ref overriding a packed one, a hidden ref, and a
/// ref whose object is missing (Git advertises it, without a peeled value).
fn git_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["commit", "-q", "--allow-empty", "-m", "one"]);
    git(root, &["branch", "feature"]);
    git(root, &["tag", "light"]);
    git(root, &["tag", "-a", "v0.9", "-m", "v0.9"]);
    git(root, &["commit", "-q", "--allow-empty", "-m", "two"]);
    git(root, &["tag", "-a", "v1.0", "-m", "v1.0"]);
    git(root, &["tag", "-a", "nested", "-m", "nested", "v1.0"]);
    git(root, &["tag", "-a", "treetag", "-m", "tree", "HEAD^{tree}"]);
    git(root, &["branch", "hidden/secret"]);
    git(root, &["pack-refs", "--all"]);
    git(root, &["commit", "-q", "--allow-empty", "-m", "three"]);
    git(root, &["tag", "-a", "v2.0", "-m", "v2.0"]);
    git(root, &["tag", "-a", "v1.0-nested-loose", "-m", "n", "v1.0"]);
    git(root, &["branch", "topic"]);
    git(root, &["branch", "-f", "feature", "HEAD"]);
    std::fs::write(
        root.join(".git/refs/heads/broken"),
        "0123456789abcdef0123456789abcdef01234567\n",
    )
    .expect("broken ref");
    dir
}

#[test]
fn ls_refs_matches_git_upload_pack_byte_for_byte() {
    let dir = git_fixture();
    let git_dir = dir.path().join(".git");
    let repo = Repository::open(&git_dir, &RepositoryOptions::empty()).expect("open");
    let hidden = "refs/heads/hidden";
    let requests: &[&[&str]] = &[
        &[],
        &["peel"],
        &["peel", "symrefs"],
        &[
            "peel",
            "symrefs",
            "ref-prefix HEAD",
            "ref-prefix refs/tags/v1",
        ],
        &["ref-prefix refs/heads/", "ref-prefix refs/heads/m"],
        &["peel", "ref-prefix refs/tags/"],
        &["peel", "ref-prefix refs/tags/v1.0"],
        &["ref-prefix re"],
        &["ref-prefix refs/heads/hidden"],
        &["ref-prefix refs/heads/broken"],
        &["ref-prefix refs/nothing/"],
        &["symrefs", "ref-prefix HEAD"],
    ];
    for args in requests {
        let request = ls_refs_request(args);
        let expected = git_serve(
            &git_dir,
            &[
                "-c",
                &format!("uploadpack.hideRefs={hidden}"),
                "upload-pack",
                "--stateless-rpc",
            ],
            Some("version=2"),
            &request,
        );
        let actual = grit_ls_refs(&repo, args, &[hidden]);
        assert_eq!(
            String::from_utf8_lossy(&actual),
            String::from_utf8_lossy(&expected),
            "ls-refs {args:?}"
        );
    }
}

#[test]
fn v0_advertisements_match_git_apart_from_capabilities() {
    let dir = git_fixture();
    let git_dir = dir.path().join(".git");
    let repo = Repository::open(&git_dir, &RepositoryOptions::empty()).expect("open");
    let hidden = "refs/heads/hidden";

    let expected = git_serve(
        &git_dir,
        &[
            "-c",
            &format!("uploadpack.hideRefs={hidden}"),
            "upload-pack",
            "--advertise-refs",
        ],
        None,
        b"",
    );
    let mut actual = Vec::new();
    let mut opts = serve_opts(ProtocolVersion::V0, false, &[hidden]);
    upload_pack(&repo, &mut Cursor::new(b""), &mut actual, &mut opts)
        .expect("grit upload-pack advertisement");
    assert_eq!(
        without_capabilities(&actual),
        without_capabilities(&expected)
    );

    let expected = git_serve(
        &git_dir,
        &[
            "-c",
            &format!("receive.hideRefs={hidden}"),
            "receive-pack",
            "--advertise-refs",
        ],
        None,
        b"",
    );
    let mut actual = Vec::new();
    receive_pack(
        &repo,
        &mut Cursor::new(b""),
        &mut actual,
        &serve_opts(ProtocolVersion::V0, false, &[hidden]),
        &ReceivePolicy::default(),
    )
    .expect("grit receive-pack advertisement");
    assert_eq!(
        without_capabilities(&actual),
        without_capabilities(&expected)
    );
}

#[test]
fn ls_refs_unborn_head_matches_git() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q", "--bare", "-b", "main", "."]);
    let tree = git(root, &["mktree"]);
    let commit = git(root, &["commit-tree", &tree, "-m", "c"]);
    git(root, &["update-ref", "refs/heads/other", &commit]);
    let repo = Repository::open(root, &RepositoryOptions::empty()).expect("open");
    let requests: &[&[&str]] = &[
        &["symrefs", "unborn"],
        &["unborn"],
        &["symrefs"],
        &["symrefs", "unborn", "ref-prefix HEAD"],
    ];
    for args in requests {
        let expected = git_serve(
            root,
            &["upload-pack", "--stateless-rpc"],
            Some("version=2"),
            &ls_refs_request(args),
        );
        let actual = grit_ls_refs(&repo, args, &[]);
        assert_eq!(
            String::from_utf8_lossy(&actual),
            String::from_utf8_lossy(&expected),
            "ls-refs {args:?}"
        );
    }
}
