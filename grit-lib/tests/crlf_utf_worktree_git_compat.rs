//! Bare UTF-16/UTF-32 `working-tree-encoding` smudge bytes match system Git checkout.

use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::config::ConfigSet;
use grit_lib::crlf::{convert_to_worktree_eager, ConversionConfig, FileAttrs};
use grit_lib::error::FilterError;

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

#[test]
fn bare_utf16_smudge_matches_git_bytes() {
    let conv = default_conv();
    let out =
        convert_to_worktree_eager(b"wide\n", "f.txt", &conv, &attrs_utf16(), None, None).unwrap();
    assert_eq!(out, GIT_UTF16_WIDE, "UTF-16 smudge must match git checkout");
}

#[test]
fn bare_utf32_smudge_matches_git_bytes() {
    let conv = default_conv();
    let out =
        convert_to_worktree_eager(b"wide\n", "g.txt", &conv, &attrs_utf32(), None, None).unwrap();
    assert_eq!(out, GIT_UTF32_WIDE, "UTF-32 smudge must match git checkout");
}

#[test]
fn unknown_working_tree_encoding_is_typed_unsupported_on_clean() {
    let conv = default_conv();
    let mut attrs = FileAttrs::default();
    attrs.working_tree_encoding = Some("not-a-real-encoding".to_owned());
    let err = grit_lib::crlf::convert_to_git(b"\xff\xfe", "f.txt", &conv, &attrs).unwrap_err();
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
    let err = convert_to_worktree_eager(b"hi\n", "f.txt", &conv, &attrs, None, None).unwrap_err();
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
        convert_to_worktree_eager(b"wide\n", "f.txt", &conv, &attrs_utf16(), None, None).unwrap();
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
        convert_to_worktree_eager(b"wide\n", "g.txt", &conv, &attrs_utf32(), None, None).unwrap();
    assert_eq!(grit_bytes, git_bytes);
}
