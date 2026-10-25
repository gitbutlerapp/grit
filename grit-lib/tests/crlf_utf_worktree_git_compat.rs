//! `working-tree-encoding` clean/smudge matches system Git (UTF-16/UTF-32 and ISO-8859-1).

use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::Environment;
use grit_lib::config::ConfigSet;
use grit_lib::error::FilterError;
use grit_lib::progress::NeverCancel;
use grit_lib::remote::clone::{clone as clone_repo, CloneOptions};
use grit_lib::remote::fetch::NoProgress;
use grit_lib::worktree::crlf::{
    convert_to_git, convert_to_worktree_eager, ConversionConfig, FileAttrs,
};

fn default_conv() -> ConversionConfig {
    ConversionConfig::from_config(&ConfigSet::new())
}

const GIT_UTF16_WIDE: &[u8] = &[
    0xff, 0xfe, 0x77, 0x00, 0x69, 0x00, 0x64, 0x00, 0x65, 0x00, 0x0a, 0x00,
];

const GIT_UTF32_WIDE: &[u8] = &[
    0xff, 0xfe, 0x00, 0x00, 0x77, 0x00, 0x00, 0x00, 0x69, 0x00, 0x00, 0x00, 0x64, 0x00, 0x00, 0x00,
    0x65, 0x00, 0x00, 0x00, 0x0a, 0x00, 0x00, 0x00,
];

fn attrs_utf16() -> FileAttrs {
    FileAttrs {
        working_tree_encoding: Some("UTF-16".to_owned()),
        ..FileAttrs::default()
    }
}

fn attrs_utf32() -> FileAttrs {
    FileAttrs {
        working_tree_encoding: Some("UTF-32".to_owned()),
        ..FileAttrs::default()
    }
}

const LATIN1_HELLO_WORKTREE: &[u8] = b"h\xe9llo\n";
const LATIN1_HELLO_UTF8_BLOB: &[u8] = b"h\xc3\xa9llo\n";

fn attrs_latin1(label: &str) -> FileAttrs {
    FileAttrs {
        working_tree_encoding: Some(label.to_owned()),
        ..FileAttrs::default()
    }
}

#[test]
fn latin1_clean_and_smudge_roundtrip() {
    let conv = default_conv();
    let attrs = attrs_latin1("ISO-8859-1");
    let cleaned =
        convert_to_git(LATIN1_HELLO_WORKTREE, "a.l1", &conv, &attrs).expect("latin1 clean");
    assert_eq!(cleaned, LATIN1_HELLO_UTF8_BLOB);
    let smudged = convert_to_worktree_eager(&cleaned, "a.l1", &conv, &attrs, None, None, None)
        .expect("latin1 smudge");
    assert_eq!(smudged, LATIN1_HELLO_WORKTREE);
}

#[test]
fn latin1_alias_spellings_roundtrip() {
    let conv = default_conv();
    for label in ["latin1", "ISO8859-1", "ISO-8859-1"] {
        let attrs = attrs_latin1(label);
        let cleaned = convert_to_git(LATIN1_HELLO_WORKTREE, "a.l1", &conv, &attrs)
            .unwrap_or_else(|e| panic!("clean with {label}: {e:?}"));
        assert_eq!(cleaned, LATIN1_HELLO_UTF8_BLOB, "UTF-8 blob for {label}");
        let smudged = convert_to_worktree_eager(&cleaned, "a.l1", &conv, &attrs, None, None, None)
            .unwrap_or_else(|e| panic!("smudge with {label}: {e:?}"));
        assert_eq!(smudged, LATIN1_HELLO_WORKTREE, "worktree bytes for {label}");
    }
}

#[test]
fn latin1_smudge_matches_git_bytes() {
    let conv = default_conv();
    let out = convert_to_worktree_eager(
        LATIN1_HELLO_UTF8_BLOB,
        "a.l1",
        &conv,
        &attrs_latin1("ISO-8859-1"),
        None,
        None,
        None,
    )
    .unwrap();
    assert_eq!(out, LATIN1_HELLO_WORKTREE);
}

#[test]
fn bare_utf16_smudge_matches_git_bytes() {
    let conv = default_conv();
    let out =
        convert_to_worktree_eager(b"wide\n", "f.txt", &conv, &attrs_utf16(), None, None, None)
            .unwrap();
    assert_eq!(out, GIT_UTF16_WIDE, "UTF-16 smudge must match git checkout");
}

#[test]
fn bare_utf32_smudge_matches_git_bytes() {
    let conv = default_conv();
    let out =
        convert_to_worktree_eager(b"wide\n", "g.txt", &conv, &attrs_utf32(), None, None, None)
            .unwrap();
    assert_eq!(out, GIT_UTF32_WIDE, "UTF-32 smudge must match git checkout");
}

#[test]
fn unknown_working_tree_encoding_is_typed_unsupported_on_clean() {
    let conv = default_conv();
    let mut attrs = FileAttrs::default();
    attrs.working_tree_encoding = Some("not-a-real-encoding".to_owned());
    let err =
        grit_lib::worktree::crlf::convert_to_git(b"\xff\xfe", "f.txt", &conv, &attrs).unwrap_err();
    assert!(matches!(
        err,
        FilterError::NotFilteredProperly { detail, .. }
            if detail.contains("not-a-real-encoding")
    ));
}

#[test]
fn unknown_working_tree_encoding_is_typed_unsupported_on_smudge() {
    let conv = default_conv();
    let mut attrs = FileAttrs::default();
    attrs.working_tree_encoding = Some("not-a-real-encoding".to_owned());
    let err =
        convert_to_worktree_eager(b"hi\n", "f.txt", &conv, &attrs, None, None, None).unwrap_err();
    assert!(matches!(
        err,
        FilterError::NotFilteredProperly { detail, .. }
            if detail.contains("not-a-real-encoding")
    ));
}

fn git_env() -> [(&'static str, &'static str); 2] {
    [
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_SYSTEM", "/dev/null"),
    ]
}

/// System `git checkout` smudge bytes for a path with bare `UTF-16` / `UTF-32` attributes.
fn git_checkout_worktree_bytes(
    root: &Path,
    attr_line: &str,
    path: &str,
    encoded_on_disk: &[u8],
) -> Vec<u8> {
    fs::write(root.join(".gitattributes"), format!("{path} {attr_line}\n")).expect("gitattributes");
    Command::new("git")
        .current_dir(root)
        .args(["init", "-q"])
        .envs(git_env())
        .status()
        .expect("git init");
    for (key, value) in [("user.email", "test@example.com"), ("user.name", "Test")] {
        Command::new("git")
            .current_dir(root)
            .args(["config", key, value])
            .envs(git_env())
            .status()
            .expect("git config user");
    }
    Command::new("git")
        .current_dir(root)
        .args(["add", ".gitattributes"])
        .envs(git_env())
        .status()
        .expect("git add attrs");
    Command::new("git")
        .current_dir(root)
        .args(["commit", "-q", "-m", "attrs"])
        .envs(git_env())
        .status()
        .expect("git commit attrs");
    fs::write(root.join(path), encoded_on_disk).expect("encoded worktree file");
    Command::new("git")
        .current_dir(root)
        .args(["add", path])
        .envs(git_env())
        .status()
        .expect("git add file");
    Command::new("git")
        .current_dir(root)
        .args(["commit", "-q", "-m", "file"])
        .envs(git_env())
        .status()
        .expect("git commit file");
    fs::remove_file(root.join(path)).expect("remove worktree file");
    Command::new("git")
        .current_dir(root)
        .args(["checkout", "HEAD", "--", path])
        .envs(git_env())
        .status()
        .expect("git checkout smudge");
    fs::read(root.join(path)).expect("read smudged file")
}

#[test]
fn bare_utf16_smudge_matches_system_git_checkout() {
    let root = tempfile::tempdir().expect("tempdir");
    let git_bytes = git_checkout_worktree_bytes(
        root.path(),
        "working-tree-encoding=UTF-16",
        "f.txt",
        GIT_UTF16_WIDE,
    );
    assert_eq!(git_bytes, GIT_UTF16_WIDE);

    let conv = default_conv();
    let grit_bytes =
        convert_to_worktree_eager(b"wide\n", "f.txt", &conv, &attrs_utf16(), None, None, None)
            .unwrap();
    assert_eq!(grit_bytes, git_bytes);
}

#[test]
fn bare_utf32_smudge_matches_system_git_checkout() {
    let root = tempfile::tempdir().expect("tempdir");
    let git_bytes = git_checkout_worktree_bytes(
        root.path(),
        "working-tree-encoding=UTF-32",
        "g.txt",
        GIT_UTF32_WIDE,
    );
    assert_eq!(git_bytes, GIT_UTF32_WIDE);

    let conv = default_conv();
    let grit_bytes =
        convert_to_worktree_eager(b"wide\n", "g.txt", &conv, &attrs_utf32(), None, None, None)
            .unwrap();
    assert_eq!(grit_bytes, git_bytes);
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .envs(git_env())
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?} failed: {:?}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn init_git_latin1_source(root: &Path) {
    fs::write(
        root.join(".gitattributes"),
        b"*.l1 text working-tree-encoding=ISO-8859-1\n",
    )
    .expect("gitattributes");
    fs::write(root.join("a.l1"), LATIN1_HELLO_WORKTREE).expect("a.l1");
    Command::new("git")
        .current_dir(root)
        .args(["init", "-q"])
        .envs(git_env())
        .status()
        .expect("git init");
    for (key, value) in [("user.email", "test@example.com"), ("user.name", "Test")] {
        Command::new("git")
            .current_dir(root)
            .args(["config", key, value])
            .envs(git_env())
            .status()
            .expect("git config");
    }
    Command::new("git")
        .current_dir(root)
        .args(["add", "-A"])
        .envs(git_env())
        .status()
        .expect("git add");
    Command::new("git")
        .current_dir(root)
        .args(["commit", "-qm", "gitmade"])
        .envs(git_env())
        .status()
        .expect("git commit");
}

#[test]
fn latin1_clean_matches_git_stored_blob() {
    let root = tempfile::tempdir().expect("tempdir");
    init_git_latin1_source(root.path());
    let git_blob = git_out(root.path(), &["cat-file", "-p", "HEAD:a.l1"]);
    let conv = default_conv();
    let cleaned = convert_to_git(
        LATIN1_HELLO_WORKTREE,
        "a.l1",
        &conv,
        &attrs_latin1("ISO-8859-1"),
    )
    .expect("grit clean");
    assert_eq!(cleaned, git_blob.as_bytes());
}

#[test]
fn latin1_smudge_matches_system_git_checkout() {
    let root = tempfile::tempdir().expect("tempdir");
    let git_bytes = git_checkout_worktree_bytes(
        root.path(),
        "working-tree-encoding=ISO-8859-1",
        "a.l1",
        LATIN1_HELLO_WORKTREE,
    );
    assert_eq!(git_bytes, LATIN1_HELLO_WORKTREE);

    let conv = default_conv();
    let grit_bytes = convert_to_worktree_eager(
        LATIN1_HELLO_UTF8_BLOB,
        "a.l1",
        &conv,
        &attrs_latin1("ISO-8859-1"),
        None,
        None,
        None,
    )
    .unwrap();
    assert_eq!(grit_bytes, git_bytes);
}

#[test]
fn grit_clone_checkouts_latin1_from_git_repo() {
    let upstream = tempfile::tempdir().expect("upstream");
    init_git_latin1_source(upstream.path());

    let dest = tempfile::tempdir().expect("dest");
    let opts = CloneOptions {
        url: upstream.path().to_string_lossy().into_owned(),
        dest: dest.path().to_path_buf(),
        environment: Environment::empty(),
        ..CloneOptions::new(upstream.path().to_string_lossy(), dest.path().to_path_buf())
    };
    clone_repo(opts, &mut NoProgress, &NeverCancel, None).expect("grit clone");

    let bytes = fs::read(dest.path().join("a.l1")).expect("read a.l1");
    assert_eq!(bytes, LATIN1_HELLO_WORKTREE);
    let status = git_out(dest.path(), &["status", "--short"]);
    assert!(
        status.is_empty(),
        "git status in grit clone should be clean after Latin-1 checkout: {status:?}"
    );
}
