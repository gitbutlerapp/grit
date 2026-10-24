//! Typed tree/commit creation through [`grit_lib::objects::Objects`], cross-checked with
//! system `git fsck --strict` and `git cat-file` (issue #1746).

use grit_lib::error::Error;
use grit_lib::ident::{parse_signature, Signature, SignatureError, SignatureField};
use grit_lib::objects::{
    change_id, serialize_commit, serialize_tree, ChangeId, CommitData, HashAlgo, NewCommit,
    ObjectId, ObjectKind, TreeEntry, TreeEntryError,
};
use grit_lib::path_validate::PathComponentError;
use grit_lib::repo::{InitOptions, Repository};
use grit_test_support::git_cmd;
use std::path::Path;
use time::{OffsetDateTime, UtcOffset};

fn init_bare(dir: &Path, algo: HashAlgo) -> Repository {
    Repository::init(
        dir,
        &InitOptions::new()
            .with_bare(true)
            .with_initial_branch("main")
            .with_object_format(algo),
    )
    .expect("init bare repository")
}

fn when(unix: i64, offset_minutes: i32) -> OffsetDateTime {
    let offset = UtcOffset::from_whole_seconds(offset_minutes * 60).expect("offset");
    OffsetDateTime::from_unix_timestamp(unix)
        .expect("timestamp")
        .to_offset(offset)
}

fn ada(unix: i64) -> Signature {
    Signature::new("Ada Lovelace", "ada@example.com", when(unix, 330)).expect("signature")
}

fn git_fsck_strict(dir: &Path) {
    git_cmd(&["fsck", "--strict", "--no-dangling"])
        .in_dir(dir)
        .suc();
}

fn git_stdout(dir: &Path, args: &[&str]) -> String {
    git_cmd(args).in_dir(dir).suc().stdout
}

fn blob(repo: &Repository, data: &[u8]) -> ObjectId {
    repo.objects().write(ObjectKind::Blob, data).expect("blob")
}

fn file(name: &[u8], oid: ObjectId) -> TreeEntry {
    TreeEntry {
        mode: 0o100644,
        name: name.to_vec(),
        oid,
    }
}

fn hand_rolled_commit(tree: ObjectId, ident: &str) -> CommitData {
    CommitData {
        tree,
        parents: Vec::new(),
        author: ident.to_owned(),
        committer: ident.to_owned(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: "msg\n".to_owned(),
        raw_message: None,
        extra_headers: Vec::new(),
    }
}

#[test]
fn write_rejects_unsorted_tree_and_leaves_no_object() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_bare(dir.path(), HashAlgo::Sha1);
    let a = blob(&repo, b"hello\n");
    let b = blob(&repo, b"world\n");
    let unsorted = serialize_tree(&[file(b"b", a), file(b"a", b)]);

    let err = repo
        .objects()
        .write(ObjectKind::Tree, &unsorted)
        .expect_err("unsorted tree must be refused");
    match err {
        Error::InvalidObject { kind, check } => {
            assert_eq!(kind, ObjectKind::Tree);
            assert_eq!(check.id, "treeNotSorted");
        }
        other => panic!("expected InvalidObject, got {other:?}"),
    }
    let would_be = repo
        .objects()
        .hash(ObjectKind::Tree, &unsorted)
        .expect("hash");
    assert!(!repo.objects().contains(&would_be));
    git_fsck_strict(dir.path());
}

#[test]
fn write_rejects_commit_with_identity_missing_timestamp() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_bare(dir.path(), HashAlgo::Sha1);
    let body = serialize_commit(&hand_rolled_commit(
        ObjectId::empty_tree(),
        "A U Thor <a@example.com>",
    ));
    let err = repo
        .objects()
        .write(ObjectKind::Commit, &body)
        .expect_err("identity without date must be refused");
    assert!(
        matches!(&err, Error::InvalidObject { kind: ObjectKind::Commit, check } if check.id == "missingSpaceBeforeDate"),
        "{err:?}"
    );
    git_fsck_strict(dir.path());
}

#[test]
fn write_rejects_malformed_tag() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_bare(dir.path(), HashAlgo::Sha1);
    let body = format!(
        "object {}\ntype tree\ntag v1\n\nno tagger\n",
        ObjectId::empty_tree()
    );
    let err = repo
        .objects()
        .write(ObjectKind::Tag, body.as_bytes())
        .expect_err("tag without tagger must be refused");
    assert!(
        matches!(&err, Error::InvalidObject { kind: ObjectKind::Tag, check } if check.id == "missingTaggerEntry"),
        "{err:?}"
    );
}

#[test]
fn write_literally_stores_bytes_fsck_rejects() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_bare(dir.path(), HashAlgo::Sha1);
    let a = blob(&repo, b"hello\n");
    let unsorted = serialize_tree(&[file(b"b", a), file(b"a", a)]);
    let oid = repo
        .objects()
        .write_literally(ObjectKind::Tree, &unsorted)
        .expect("literal write");
    assert!(repo.objects().contains(&oid));
    assert_eq!(repo.objects().read(&oid).expect("read").data, unsorted);
}

#[test]
fn write_tree_sorts_entries_and_passes_git_fsck() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_bare(dir.path(), HashAlgo::Sha1);
    let a = blob(&repo, b"hello\n");
    let b = blob(&repo, b"world\n");
    let sub = repo
        .objects()
        .write_tree([file(b"inner", a)])
        .expect("subtree");
    let tree = repo
        .objects()
        .write_tree([
            file(b"foo.c", b),
            TreeEntry {
                mode: 0o040000,
                name: b"foo".to_vec(),
                oid: sub,
            },
            TreeEntry {
                mode: 0o100755,
                name: b"foo-bar".to_vec(),
                oid: a,
            },
            TreeEntry {
                mode: 0o120000,
                name: b"link".to_vec(),
                oid: b,
            },
        ])
        .expect("tree");

    let listing = git_stdout(dir.path(), &["ls-tree", "--name-only", &tree.to_hex()]);
    assert_eq!(listing, "foo-bar\nfoo.c\nfoo\nlink\n");
    let git_tree = git_cmd(&["mktree"])
        .in_dir(dir.path())
        .stdin(format!(
            "100644 blob {b}\tfoo.c\n040000 tree {sub}\tfoo\n100755 blob {a}\tfoo-bar\n120000 blob {b}\tlink\n"
        ))
        .suc()
        .stdout;
    assert_eq!(git_tree.trim(), tree.to_hex(), "same id as git mktree");
    git_fsck_strict(dir.path());
}

#[test]
fn write_tree_returns_typed_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_bare(dir.path(), HashAlgo::Sha1);
    let a = blob(&repo, b"hello\n");
    let err = repo
        .objects()
        .write_tree([file(b"same", a), file(b"same", a)])
        .expect_err("duplicate");
    assert!(matches!(
        err,
        Error::InvalidTreeEntry(TreeEntryError::DuplicateName { ref name }) if name == b"same"
    ));
    let err = repo
        .objects()
        .write_tree([file(b"a/b", a)])
        .expect_err("slash");
    assert!(matches!(
        err,
        Error::InvalidTreeEntry(TreeEntryError::InvalidName {
            reason: PathComponentError::ContainsSlash,
            ..
        })
    ));
    let err = repo
        .objects()
        .write_tree([TreeEntry {
            mode: 0o100600,
            name: b"a".to_vec(),
            oid: a,
        }])
        .expect_err("mode");
    assert!(matches!(
        err,
        Error::InvalidTreeEntry(TreeEntryError::InvalidMode { mode: 0o100600, .. })
    ));
    let empty = repo
        .objects()
        .write_tree(std::iter::empty())
        .expect("empty tree");
    assert_eq!(empty, ObjectId::empty_tree());
}

#[test]
fn write_commit_round_trips_through_git() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_bare(dir.path(), HashAlgo::Sha1);
    let a = blob(&repo, b"hello\n");
    let tree = repo
        .objects()
        .write_tree([file(b"README", a)])
        .expect("tree");
    let root = repo
        .objects()
        .write_commit(&NewCommit::new(
            tree,
            ada(1_700_000_000),
            ada(1_700_000_000),
            "root",
        ))
        .expect("root commit");
    let committer = Signature::new("C O Mitter", "c@example.com", when(1_700_000_100, -480))
        .expect("committer");
    let child = repo
        .objects()
        .write_commit(
            &NewCommit::new(tree, ada(1_700_000_050), committer, "child\n\nbody\n")
                .with_parents([root]),
        )
        .expect("child commit");

    git_fsck_strict(dir.path());
    let pretty = git_stdout(dir.path(), &["cat-file", "-p", &child.to_hex()]);
    assert!(
        pretty.starts_with(&format!("tree {tree}\nparent {root}\n")),
        "{pretty}"
    );
    assert!(pretty.contains("\nauthor Ada Lovelace <ada@example.com> 1700000050 +0530\n"));
    assert!(pretty.contains("\ncommitter C O Mitter <c@example.com> 1700000100 -0800\n"));
    assert!(
        pretty.contains("\nchange-id "),
        "change id by default: {pretty}"
    );
    assert!(pretty.ends_with("\n\nchild\n\nbody\n"), "{pretty}");
    assert_eq!(
        git_stdout(dir.path(), &["log", "--format=%s", &child.to_hex()]),
        "child\nroot\n"
    );

    let parsed = repo.objects().read_commit(&child).expect("read back");
    assert_eq!(
        parse_signature(&parsed.author).expect("author"),
        ada(1_700_000_050)
    );
}

#[test]
fn write_commit_change_id_follows_policy_and_override() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_bare(dir.path(), HashAlgo::Sha1);
    let kept = ChangeId::parse_reverse_hex("zyxwvutsrqponmlkzyxwvutsrqponmlk").expect("id");
    let preserved = repo
        .objects()
        .write_commit(
            &NewCommit::new(ObjectId::empty_tree(), ada(1), ada(1), "kept")
                .with_change_id(kept.clone()),
        )
        .expect("commit with explicit id");
    let data = repo.objects().read_commit(&preserved).expect("read");
    assert_eq!(change_id(&data), Some(kept));

    git_cmd(&["config", "grit.changeIds", "false"])
        .in_dir(dir.path())
        .suc();
    let reopened = Repository::open(dir.path(), &Default::default()).expect("reopen");
    let plain = reopened
        .objects()
        .write_commit(&NewCommit::new(
            ObjectId::empty_tree(),
            ada(1),
            ada(1),
            "plain",
        ))
        .expect("commit");
    let data = reopened.objects().read_commit(&plain).expect("read");
    assert_eq!(change_id(&data), None);
    assert!(data.extra_headers.is_empty());
    git_fsck_strict(dir.path());
}

#[test]
fn write_commit_checks_tree_and_parent_kinds() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_bare(dir.path(), HashAlgo::Sha1);
    let a = blob(&repo, b"hello\n");
    let err = repo
        .objects()
        .write_commit(&NewCommit::new(a, ada(1), ada(1), "blob as tree"))
        .expect_err("tree must be a tree");
    assert!(matches!(
        err,
        Error::UnexpectedObjectKind {
            expected: ObjectKind::Tree,
            actual: ObjectKind::Blob,
            ..
        }
    ));
    let err = repo
        .objects()
        .write_commit(
            &NewCommit::new(ObjectId::empty_tree(), ada(1), ada(1), "tree as parent")
                .with_parents([ObjectId::empty_tree()]),
        )
        .expect_err("parent must be a commit");
    assert!(matches!(
        err,
        Error::UnexpectedObjectKind {
            expected: ObjectKind::Commit,
            actual: ObjectKind::Tree,
            ..
        }
    ));
    let missing = ObjectId::from_hex("1111111111111111111111111111111111111111").expect("oid");
    let err = repo
        .objects()
        .write_commit(&NewCommit::new(missing, ada(1), ada(1), "missing tree"))
        .expect_err("tree must exist");
    assert!(matches!(err, Error::ObjectNotFound(_)), "{err:?}");
}

#[test]
fn write_commit_works_in_sha256_repository() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_bare(dir.path(), HashAlgo::Sha256);
    let a = blob(&repo, b"hello\n");
    assert_eq!(a.algo(), HashAlgo::Sha256);
    let tree = repo
        .objects()
        .write_tree([file(b"README", a)])
        .expect("tree");
    let commit = repo
        .objects()
        .write_commit(&NewCommit::new(tree, ada(2), ada(2), "sha256"))
        .expect("commit");
    git_fsck_strict(dir.path());
    assert_eq!(
        git_stdout(dir.path(), &["rev-parse", &format!("{commit}^{{tree}}")]).trim(),
        tree.to_hex()
    );
}

#[test]
fn signature_matches_identity_git_writes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_bare(dir.path(), HashAlgo::Sha1);
    let commit = git_cmd(&[
        "commit-tree",
        "-m",
        "from git",
        &ObjectId::empty_tree().to_hex(),
    ])
    .in_dir(dir.path())
    .env("GIT_AUTHOR_NAME", "Ada Lovelace")
    .env("GIT_AUTHOR_EMAIL", "ada@example.com")
    .env("GIT_AUTHOR_DATE", "1700000000 -0030")
    .suc()
    .stdout;
    let oid = ObjectId::from_hex(commit.trim()).expect("oid");
    let data = repo.objects().read_commit(&oid).expect("read git commit");
    let expected =
        Signature::new("Ada Lovelace", "ada@example.com", when(1_700_000_000, -30)).expect("sig");
    assert_eq!(data.author, expected.header_line());
    assert_eq!(parse_signature(&data.author), Some(expected));
}

#[test]
fn signature_new_rejects_header_breaking_input() {
    let t = when(0, 0);
    assert_eq!(
        Signature::new("Evil <x>", "e@example.com", t),
        Err(SignatureError::ForbiddenCharacter {
            field: SignatureField::Name,
            ch: '<'
        })
    );
    assert_eq!(
        Signature::new("Evil", "e@example.com>\nparent 0", t),
        Err(SignatureError::ForbiddenCharacter {
            field: SignatureField::Email,
            ch: '>'
        })
    );
    assert_eq!(
        Signature::new("A", "a@example.com", when(-1, 0)),
        Err(SignatureError::BeforeEpoch)
    );
    let odd_offset = OffsetDateTime::from_unix_timestamp(0)
        .expect("epoch")
        .to_offset(UtcOffset::from_hms(1, 0, 30).expect("offset"));
    assert_eq!(
        Signature::new("A", "a@example.com", odd_offset),
        Err(SignatureError::SubMinuteOffset)
    );
    let err: Error = SignatureError::BeforeEpoch.into();
    assert!(matches!(
        err,
        Error::InvalidSignature(SignatureError::BeforeEpoch)
    ));
}
