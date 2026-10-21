//! Parse/serialize round-trips for Git objects (t1006/t1007 object bodies).

use std::io::Write;
use std::process::{Command, Stdio};

use grit_lib::error::Error;
use grit_lib::objects::{
    parse_commit, parse_tag, parse_tree, serialize_commit, serialize_tag, serialize_tree,
    tree_entry_cmp, CommitData, HashAlgo, ObjectId, ObjectKind, TagData, TreeEntry,
};
use grit_test_support::objects::{git_supports_sha256, HashAlgo as FixtureAlgo, RepoFixture};

fn init_repo(algo: HashAlgo) -> RepoFixture {
    RepoFixture::init(match algo {
        HashAlgo::Sha1 => FixtureAlgo::Sha1,
        HashAlgo::Sha256 => FixtureAlgo::Sha256,
    })
    .expect("init")
}

fn git_bytes(repo: &RepoFixture, args: &[&str]) -> Vec<u8> {
    repo.git(args).stdout.into_bytes()
}

fn git_cat_file_object(repo: &RepoFixture, kind: &str, oid: &str) -> Vec<u8> {
    let out = Command::new("git")
        .current_dir(repo.path())
        .args(["cat-file", kind, oid])
        .output()
        .expect("cat-file");
    assert!(
        out.status.success(),
        "cat-file {kind} {oid}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn git_trim(repo: &RepoFixture, args: &[&str]) -> String {
    repo.git(args).stdout.trim().to_string()
}

fn git_hash_object_stdin(repo: &RepoFixture, kind: &str, body: &[u8]) -> String {
    let object_format = match repo.algo() {
        FixtureAlgo::Sha1 => "sha1",
        FixtureAlgo::Sha256 => "sha256",
    };
    let mut child = Command::new("git")
        .current_dir(repo.path())
        .args(["hash-object", "-w", "-t", kind, "--stdin"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_OBJECT_FORMAT", object_format)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn hash-object");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(body)
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait");
    assert!(
        out.status.success(),
        "hash-object: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn git_mktree(repo: &RepoFixture, lines: &str) -> String {
    let mut child = Command::new("git")
        .current_dir(repo.path())
        .args(["mktree"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mktree");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(lines.as_bytes())
        .expect("write mktree");
    let out = child.wait_with_output().expect("wait mktree");
    assert!(
        out.status.success(),
        "mktree: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn sort_tree_entries(entries: &mut [TreeEntry]) {
    entries
        .sort_by(|a, b| tree_entry_cmp(&a.name, a.mode == 0o040000, &b.name, b.mode == 0o040000));
}

#[test]
fn commit_roundtrip_matches_git_bytes() {
    let repo = init_repo(HashAlgo::Sha1);
    let empty = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    let parent1 = git_trim(&repo, &["commit-tree", empty, "-m", "p1"]);
    let parent2 = git_trim(&repo, &["commit-tree", empty, "-m", "p2"]);
    let commit_oid = git_trim(
        &repo,
        &[
            "commit-tree",
            empty,
            "-p",
            &parent1,
            "-p",
            &parent2,
            "-m",
            "subject\n\nbody",
        ],
    );
    let git_body = git_bytes(&repo, &["cat-file", "-p", &commit_oid]);
    let parsed = parse_commit(&git_body).expect("parse git commit");
    assert_eq!(parsed.parents.len(), 2);
    assert_eq!(serialize_commit(&parsed), git_body);
}

#[test]
fn commit_encoding_and_gpgsig_continuation_roundtrip() {
    let raw = concat!(
        "tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n",
        "author A <a@example.com> 1 +0000\n",
        "committer C <c@example.com> 1 +0000\n",
        "encoding ISO-8859-1\n",
        "gpgsig -----BEGIN PGP SIGNATURE-----\n",
        " line2\n",
        " -----END PGP SIGNATURE-----\n",
        "mergetag object 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n",
        " type commit\n",
        " tag merge-tag\n",
        " tagger T <t@example.com> 1 +0000\n",
        " -----BEGIN PGP SIGNATURE-----\n",
        " inner\n",
        " -----END PGP SIGNATURE-----\n",
        "\n",
        "msg-only\n",
    );
    let parsed = parse_commit(raw.as_bytes()).expect("parse");
    assert_eq!(parsed.encoding.as_deref(), Some("ISO-8859-1"));
    assert_eq!(parsed.message, "msg-only\n");
    assert_eq!(serialize_commit(&parsed), raw.as_bytes());
}

#[test]
fn serialize_commit_reflects_tree_edit_after_parse() {
    let raw = concat!(
        "tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n",
        "author A <a@example.com> 1 +0000\n",
        "committer C <c@example.com> 1 +0000\n",
        "\n",
        "msg\n",
    );
    let mut parsed = parse_commit(raw.as_bytes()).expect("parse");
    let zero = ObjectId::from_hex("0000000000000000000000000000000000000000").unwrap();
    parsed.tree = zero;
    let out = serialize_commit(&parsed);
    assert!(
        out.starts_with(format!("tree {}\n", zero.to_hex()).as_bytes()),
        "serialize_commit ignored the updated public tree field"
    );
}

#[test]
fn commit_message_without_trailing_newline_roundtrip() {
    let raw = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
author A <a@example.com> 1 +0000\n\
committer C <c@example.com> 1 +0000\n\
\n\
no-final-newline";
    let parsed = parse_commit(raw).expect("parse");
    assert_eq!(parsed.message, "no-final-newline");
    assert_eq!(serialize_commit(&parsed), raw);
}

#[test]
fn tag_roundtrip_all_target_types_and_missing_tagger() {
    let repo = init_repo(HashAlgo::Sha1);
    let empty_tree = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    let commit = git_trim(&repo, &["commit-tree", empty_tree, "-m", "tag-target"]);
    let blob_oid = git_hash_object_stdin(&repo, "blob", b"x");

    for (typ, oid) in [
        ("blob", blob_oid.as_str()),
        ("tree", empty_tree),
        ("commit", commit.as_str()),
    ] {
        let body = format!(
            "object {oid}\ntype {typ}\ntag v-{typ}\ntagger T <t@example.com> 1 +0000\n\nannotated {typ}\n"
        );
        let tag_oid = git_hash_object_stdin(&repo, "tag", body.as_bytes());
        let git_body = git_cat_file_object(&repo, "tag", &tag_oid);
        let parsed = parse_tag(&git_body).expect("parse tag");
        assert_eq!(parsed.object_type, typ);
        assert_eq!(serialize_tag(&parsed), git_body);
    }

    let no_tagger = format!("object {commit}\ntype commit\ntag no-tagger\n\nmsg\n");
    let parsed = parse_tag(no_tagger.as_bytes()).expect("no tagger");
    assert!(parsed.tagger.is_none());
    assert_eq!(serialize_tag(&parsed), no_tagger.as_bytes());

    let inner_body = format!(
        "object {commit}\ntype commit\ntag inner\ntagger T <t@example.com> 1 +0000\n\ninner msg\n"
    );
    let inner_oid = git_hash_object_stdin(&repo, "tag", inner_body.as_bytes());
    let outer_body = format!(
        "object {inner_oid}\ntype tag\ntag outer\ntagger T <t@example.com> 1 +0000\n\nwraps inner tag\n"
    );
    let outer_oid = git_hash_object_stdin(&repo, "tag", outer_body.as_bytes());
    let git_outer = git_cat_file_object(&repo, "tag", &outer_oid);
    let parsed_outer = parse_tag(&git_outer).expect("parse tag-of-tag");
    assert_eq!(parsed_outer.object_type, "tag");
    assert_eq!(serialize_tag(&parsed_outer), git_outer);
}

#[test]
fn tree_sorting_gitlinks_symlinks_and_mode_100664() {
    let repo = init_repo(HashAlgo::Sha1);
    let blob = git_hash_object_stdin(&repo, "blob", b"f");
    let sub_commit = git_trim(
        &repo,
        &[
            "commit-tree",
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
            "-m",
            "sub",
        ],
    );
    let lines = format!(
        "100644 blob {blob}\tfile-a\n\
120000 blob {blob}\tsymlink-l\n\
160000 commit {sub_commit}\tsubmodule\n\
100664 blob {blob}\tnon-exec\n\
40000 tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\tdir\n"
    );
    let git_tree_oid = git_mktree(&repo, &lines);
    let git_body = git_cat_file_object(&repo, "tree", &git_tree_oid);
    let entries = parse_tree(&git_body).expect("parse git tree");
    let mut rebuilt = entries;
    sort_tree_entries(&mut rebuilt);
    assert_eq!(serialize_tree(&rebuilt), git_body);
}

#[test]
fn object_id_from_hex_errors_are_typed() {
    assert!(matches!(
        ObjectId::from_hex("not-hex"),
        Err(Error::InvalidObjectId(_))
    ));
    assert!(matches!(
        ObjectId::from_hex("abc"),
        Err(Error::InvalidObjectId(_))
    ));
    let short = "a".repeat(39);
    assert!(matches!(
        ObjectId::from_hex(&short),
        Err(Error::InvalidObjectId(_))
    ));
    let valid_len_non_hex = "g".repeat(40);
    assert!(matches!(
        ObjectId::from_hex(&valid_len_non_hex),
        Err(Error::InvalidObjectId(_))
    ));
}

#[test]
fn hash_algo_name_and_len_parse_failures() {
    assert!(HashAlgo::from_name("sha3").is_none());
    assert!(HashAlgo::from_name("").is_none());
    assert!(HashAlgo::from_len(19).is_none());
    assert!(HashAlgo::from_len(21).is_none());
    assert_eq!(HashAlgo::from_name("SHA1"), Some(HashAlgo::Sha1));
    assert_eq!(HashAlgo::from_len(32), Some(HashAlgo::Sha256));
}

#[test]
fn serialize_tag_empty_message_has_no_extra_blank_line() {
    let tree = ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").unwrap();
    let tag = TagData {
        object: tree,
        object_type: "commit".to_string(),
        tag: "empty-msg".to_string(),
        tagger: None,
        message: String::new(),
    };
    let bytes = serialize_tag(&tag);
    let parsed = parse_tag(&bytes).expect("parse");
    assert!(parsed.message.is_empty());
    assert_eq!(serialize_tag(&parsed), bytes);
}

#[test]
fn serialize_parse_commit_tag_tree_identity() {
    let tree = ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").unwrap();
    let commit = CommitData {
        tree,
        parents: vec![],
        author: "A <a@example.com>".to_string(),
        committer: "C <c@example.com>".to_string(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: "hello\n".to_string(),
        raw_message: None,
        extra_headers: Vec::new(),
    };
    let bytes = serialize_commit(&commit);
    let back = parse_commit(&bytes).unwrap();
    assert_eq!(back.tree, tree);
    assert_eq!(back.message, "hello\n");

    let tag = TagData {
        object: tree,
        object_type: "commit".to_string(),
        tag: "v1".to_string(),
        tagger: Some("T <t@example.com> 1 +0000".to_string()),
        message: "tag msg".to_string(),
    };
    let tag_bytes = serialize_tag(&tag);
    let tag_back = parse_tag(&tag_bytes).unwrap();
    assert_eq!(tag_back.tag, "v1");
}

#[test]
fn serialize_commit_with_encoding_and_raw_author_bytes() {
    let tree = ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").unwrap();
    let commit = CommitData {
        tree,
        parents: vec![tree],
        author: "visible".to_string(),
        committer: "committer".to_string(),
        author_raw: b"Raw Author <r@example.com> 1 +0000".to_vec(),
        committer_raw: Vec::new(),
        encoding: Some("UTF-8".to_string()),
        message: String::new(),
        raw_message: Some(b"raw-body".to_vec()),
        extra_headers: Vec::new(),
    };
    let bytes = serialize_commit(&commit);
    let parsed = parse_commit(&bytes).expect("parse");
    assert_eq!(parsed.encoding.as_deref(), Some("UTF-8"));
    assert_eq!(parsed.raw_message.as_deref(), Some(b"raw-body".as_ref()));
}

#[test]
fn serialize_tree_empty_and_reparse() {
    let bytes = serialize_tree(&[]);
    assert!(parse_tree(&bytes).unwrap().is_empty());
}

#[test]
fn object_kind_as_str_roundtrip() {
    for kind in [
        ObjectKind::Blob,
        ObjectKind::Tree,
        ObjectKind::Commit,
        ObjectKind::Tag,
    ] {
        assert_eq!(
            ObjectKind::from_bytes(kind.as_str().as_bytes()).unwrap(),
            kind
        );
    }
}

#[test]
fn object_id_display_and_is_full_hex() {
    let oid = ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").unwrap();
    assert!(ObjectId::is_full_hex(&oid.to_hex()));
    assert_eq!(format!("{oid}"), oid.to_hex());
}

#[test]
fn parse_errors_use_typed_variants() {
    assert!(matches!(
        parse_commit(b"author x\n\n".as_ref()),
        Err(Error::CorruptObject(_))
    ));
    assert!(matches!(
        parse_tag(b"object".as_ref()),
        Err(Error::CorruptObject(_))
    ));
    assert!(matches!(
        parse_tag(
            b"object 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
type not-a-kind\ntag t\n\n"
        ),
        Err(Error::CorruptObject(_))
    ));
    assert!(matches!(
        parse_tree(b"100644 incomplete".as_ref()),
        Err(Error::CorruptObject(_))
    ));
    assert!(matches!(
        ObjectKind::from_bytes(b"not-a-type"),
        Err(Error::UnknownObjectType(_))
    ));
}

#[test]
fn tree_entry_cmp_matches_git_directory_ordering() {
    assert_eq!(
        tree_entry_cmp(b"foo", true, b"foo-bar", false),
        std::cmp::Ordering::Greater
    );
    assert_eq!(
        tree_entry_cmp(b"foo", false, b"foo", true),
        std::cmp::Ordering::Less
    );
}

#[test]
fn sha256_commit_roundtrip_when_git_supports() {
    if !git_supports_sha256() {
        return;
    }
    let repo = init_repo(HashAlgo::Sha256);
    let empty_tree = git_hash_object_stdin(&repo, "tree", b"");
    let commit_oid = git_trim(
        &repo,
        &["commit-tree", &empty_tree, "-m", "sha256 commit body\n"],
    );
    let git_body = git_cat_file_object(&repo, "commit", &commit_oid);
    let parsed = parse_commit(&git_body).expect("parse sha256 commit");
    assert_eq!(serialize_commit(&parsed), git_body);
}
