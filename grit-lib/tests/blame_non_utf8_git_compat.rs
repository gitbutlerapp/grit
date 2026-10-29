//! Reversible status paths must identify the same file throughout blame (issue #2045).
#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

use grit_lib::blame::{
    compute_blame, resolve_blame_line_range, BlameDiffAlgorithm, BlameResult, CopyDetection,
};
use grit_lib::diff::merge_diff::{blob_oid_at_path, read_blob_at_path};
use grit_lib::objects::{parse_commit, ObjectId};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;

use grit_lib::blame::{blame_file, BlameOptions};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::status::{status, StatusOptions};
use grit_lib::repo::Repository;
use grit_lib::repo_relpath::{storage_bytes, storage_string};
use grit_test_support::git;

#[test]
fn status_path_can_be_passed_to_blame() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "Tool"]);
    git(root, &["config", "user.email", "tool@example.com"]);
    let raw: &[u8] = b"raw\xff.txt";
    let file = root.join(OsStr::from_bytes(raw));
    std::fs::write(&file, "hello\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "initial"]);
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).unwrap();
    let path = storage_string(raw);
    let result = blame_file(&repo, &path, &BlameOptions::default()).unwrap();
    assert_eq!(result.lines.len(), 1);
    assert_eq!(storage_bytes(&result.lines[0].original_path), raw);

    std::fs::write(&file, "hello\nmodified\n").unwrap();
    let model = status(
        &repo,
        &StatusOptions::default(),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .unwrap();
    let reported = model
        .unstaged
        .iter()
        .find(|e| storage_bytes(e.path()) == raw)
        .unwrap();
    let result = blame_file(&repo, reported.path(), &BlameOptions::default()).unwrap();
    assert_eq!(result.lines[0].content, "hello");
    assert_eq!(storage_bytes(&result.lines[0].original_path), raw);
}

fn setup(root: &Path) {
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "Tool"]);
    git(root, &["config", "user.email", "tool@example.com"]);
    git(root, &["config", "gc.auto", "0"]);
    git(root, &["config", "maintenance.auto", "false"]);
}

fn write(root: &Path, raw: &[u8], content: &str) {
    let path = root.join(OsStr::from_bytes(raw));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn commit(root: &Path) {
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "update"]);
}

fn head(root: &Path) -> ObjectId {
    let out = Command::new("git")
        .current_dir(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert!(out.status.success());
    std::str::from_utf8(&out.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

// Porcelain filenames can be C-quoted even when argv was passed as raw bytes.
fn unquote_filename(input: &[u8]) -> Vec<u8> {
    if !input.starts_with(b"\"") {
        return input.to_vec();
    }
    assert!(input.ends_with(b"\""));
    let mut out = Vec::new();
    let mut rest = &input[1..input.len() - 1];
    while let Some((&b, tail)) = rest.split_first() {
        rest = tail;
        if b != b'\\' {
            out.push(b);
            continue;
        }
        let (&escaped, tail) = rest.split_first().unwrap();
        rest = tail;
        out.push(match escaped {
            b'0'..=b'3' => {
                assert!(rest.len() >= 2 && rest[..2].iter().all(|b| (b'0'..=b'7').contains(b)));
                let value = (escaped - b'0') * 64 + (rest[0] - b'0') * 8 + (rest[1] - b'0');
                rest = &rest[2..];
                value
            }
            b'a' => 7,
            b'b' => 8,
            b't' => b'\t',
            b'n' => b'\n',
            b'v' => 11,
            b'f' => 12,
            b'r' => b'\r',
            b'\\' | b'"' => escaped,
            _ => panic!("unexpected filename escape {escaped}"),
        });
    }
    out
}

type Attribution = (ObjectId, usize, usize, Vec<u8>);

fn git_blame(root: &Path, path: &[u8], copies: bool) -> Vec<Attribution> {
    let mut cmd = Command::new("git");
    cmd.current_dir(root).args(["blame", "--line-porcelain"]);
    if copies {
        cmd.args(["-C", "-C1"]);
    }
    let out = cmd
        .arg("HEAD")
        .arg("--")
        .arg(OsStr::from_bytes(path))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut attribution = None;
    let mut filename = Vec::new();
    let mut result = Vec::new();
    for line in out.stdout.split(|&b| b == b'\n') {
        if line.starts_with(b"\t") {
            let (oid, original, final_line) = attribution.take().unwrap();
            result.push((oid, original, final_line, filename.clone()));
        } else if let Some(path) = line.strip_prefix(b"filename ") {
            filename = unquote_filename(path);
        } else {
            let parts: Vec<_> = line.split(|&b| b == b' ').collect();
            if parts.len() >= 3 && parts[0].len() == 40 {
                attribution = Some((
                    std::str::from_utf8(parts[0]).unwrap().parse().unwrap(),
                    std::str::from_utf8(parts[1]).unwrap().parse().unwrap(),
                    std::str::from_utf8(parts[2]).unwrap().parse().unwrap(),
                ));
            }
        }
    }
    result
}

fn assert_attribution(result: &BlameResult, expected: &[Attribution]) {
    let actual: Vec<_> = result
        .lines
        .iter()
        .map(|l| {
            (
                l.commit,
                l.original_line,
                l.final_line,
                storage_bytes(&l.original_path).into_owned(),
            )
        })
        .collect();
    assert_eq!(actual, expected);
    let expanded_hunks: Vec<_> = result
        .hunks()
        .iter()
        .flat_map(|h| {
            (0..h.count).map(move |i| {
                (
                    h.commit,
                    h.original_start + i,
                    h.final_start + i,
                    storage_bytes(&h.original_path).into_owned(),
                )
            })
        })
        .collect();
    assert_eq!(expanded_hunks, expected);
}

#[test]
fn distinct_byte_paths_match_git_with_and_without_bloom_filters() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    setup(root);
    let paths: &[&[u8]] = &[
        b"raw\xff.txt",
        b"raw\xfe.txt",
        "raw\u{fffd}.txt".as_bytes(),
        b"dir\xff/file\xfe.txt",
        b"dir\xfe/file\xff.txt",
        b"grit+pathhex:726177ff2e747874",
        b"grit+pathhex:6162",
        b"ab",
        b"normal/grit+pathhex:6162",
        b"normal/ab",
        b"grit+pathhex:646972/raw\xff.txt",
    ];
    for (i, path) in paths.iter().enumerate() {
        write(
            root,
            path,
            &format!("first unique content {i}\nsecond unique content {i}\n"),
        );
        commit(root);
    }
    // A recent change must not be skipped by a Bloom filter hashed from the storage text.
    for (i, path) in paths.iter().enumerate() {
        write(
            root,
            path,
            &format!("first unique content {i}\nsecond changed content {i}\n"),
        );
        commit(root);
    }
    for i in 0..4 {
        write(root, b"noise", &i.to_string());
        commit(root);
    }
    let start = head(root);
    for graph in [false, true] {
        if graph {
            git(
                root,
                &["commit-graph", "write", "--reachable", "--changed-paths"],
            );
        }
        let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).unwrap();
        let tree = parse_commit(&repo.odb().read(&start).unwrap().data)
            .unwrap()
            .tree;
        for (i, raw) in paths.iter().enumerate() {
            let path = storage_string(raw);
            let expected = git_blame(root, raw, false);
            let result = blame_file(&repo, &path, &BlameOptions::default()).unwrap();
            assert_attribution(&result, &expected);
            assert_eq!(
                read_blob_at_path(repo.odb(), &tree, &path).unwrap(),
                format!("first unique content {i}\nsecond changed content {i}\n").as_bytes()
            );
            assert_eq!(
                repo.odb()
                    .read(&blob_oid_at_path(repo.odb(), &tree, &path).unwrap())
                    .unwrap()
                    .data,
                read_blob_at_path(repo.odb(), &tree, &path).unwrap()
            );
            assert_eq!(
                resolve_blame_line_range(&repo, start, &path, "/second/,+1").unwrap(),
                2..=2
            );
            let range = BlameOptions {
                line_range: Some(2..=2),
                ..Default::default()
            };
            assert_attribution(&blame_file(&repo, &path, &range).unwrap(), &expected[1..]);
            let computed = compute_blame(
                repo.odb(),
                start,
                &path,
                &HashSet::new(),
                BlameDiffAlgorithm::Myers,
                None,
                false,
                0,
                false,
                &HashMap::new(),
                false,
                None,
                Default::default(),
                &Default::default(),
            )
            .unwrap();
            let actual: Vec<_> = computed
                .iter()
                .map(|l| {
                    (
                        l.oid,
                        l.orig_lineno,
                        l.final_lineno,
                        storage_bytes(&l.original_path).into_owned(),
                    )
                })
                .collect();
            assert_eq!(actual, expected);
        }
    }
    git(root, &["fsck", "--strict"]);
}

#[test]
fn rename_and_copy_origins_preserve_bytes_and_hunks() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    setup(root);
    let source = b"old\xff/quote\"tab\tback\\\xfe.txt";
    let renamed = b"new\xfe/renamed\xff.txt";
    let copied = b"copy\xfd/grit+pathhex:6162";
    let content = (0..4)
        .map(|i| format!("line {i} {}\n", "meaningfulcontent".repeat(6)))
        .collect::<String>();
    write(root, source, &content);
    commit(root);
    write(root, renamed, &content);
    std::fs::remove_file(root.join(OsStr::from_bytes(source))).unwrap();
    commit(root);
    write(root, renamed, &format!("{content}new line after rename\n"));
    commit(root);
    write(root, copied, &content);
    commit(root);
    for graph in [false, true] {
        if graph {
            git(
                root,
                &["commit-graph", "write", "--reachable", "--changed-paths"],
            );
        }
        let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).unwrap();
        for (path, copies) in [(renamed.as_slice(), false), (copied.as_slice(), true)] {
            let options = BlameOptions {
                copy_detection: if copies {
                    CopyDetection::FromCreatingCommit
                } else {
                    CopyDetection::Off
                },
                ..Default::default()
            };
            let result = blame_file(&repo, &storage_string(path), &options).unwrap();
            let expected = git_blame(root, path, copies);
            assert_attribution(&result, &expected);
            assert_eq!(expected[0].3, source);
            assert_eq!(
                storage_bytes(&result.hunks()[0].original_path).as_ref(),
                source
            );
        }
    }
    git(root, &["fsck", "--strict"]);
}

#[test]
fn grit_written_raw_path_objects_are_readable_by_git_blame() {
    use grit_lib::objects::{serialize_tree, ObjectKind, TreeEntry};
    use grit_lib::repo::InitOptions;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let repo = Repository::init(root, &InitOptions::default()).unwrap();
    let blob = repo
        .odb()
        .write(ObjectKind::Blob, b"hello from grit\n")
        .unwrap();
    let subtree = repo
        .odb()
        .write(
            ObjectKind::Tree,
            &serialize_tree(&[TreeEntry {
                mode: 0o100644,
                name: b"raw\xff.txt".to_vec(),
                oid: blob,
            }]),
        )
        .unwrap();
    let tree = repo
        .odb()
        .write(
            ObjectKind::Tree,
            &serialize_tree(&[TreeEntry {
                mode: 0o040000,
                name: b"dir\xfe".to_vec(),
                oid: subtree,
            }]),
        )
        .unwrap();
    let body = format!("tree {tree}\nauthor Tool <tool@example.com> 1700000000 +0000\ncommitter Tool <tool@example.com> 1700000000 +0000\n\ninitial\n");
    let tip = repo
        .odb()
        .write(ObjectKind::Commit, body.as_bytes())
        .unwrap();
    git(root, &["update-ref", "HEAD", &tip.to_hex()]);
    let raw = b"dir\xfe/raw\xff.txt";
    let result = blame_file(&repo, &storage_string(raw), &BlameOptions::default()).unwrap();
    assert_attribution(&result, &git_blame(root, raw, false));
    assert_eq!(result.lines[0].commit, tip);
    git(root, &["fsck", "--strict"]);
}

fn assert_literal_prefix_blame(root: &Path, cwd: &Path, operand: &str) {
    let mut options = RepositoryOptions::empty();
    options.environment.cwd = cwd.to_owned();
    let repo = Repository::discover(Some(root), &options).unwrap();
    let result = blame_file(&repo, operand, &BlameOptions::default()).unwrap();
    assert_eq!(result.lines[0].content, "literal");
    let expected = git_blame(root, b"grit+pathhex:6162", false);
    assert_attribution(&result, &expected);
    let range = resolve_blame_line_range(&repo, head(root), operand, "/literal/,+1").unwrap();
    assert_eq!(range, 1..=1);
    let ranged = blame_file(
        &repo,
        operand,
        &BlameOptions {
            line_range: Some(range),
            ..Default::default()
        },
    )
    .unwrap();
    assert_attribution(&ranged, &expected);
}

#[test]
fn dot_relative_literal_prefix_does_not_decode_after_normalization() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    setup(root);
    write(root, b"ab", "decoy\n");
    write(root, b"grit+pathhex:6162", "literal\n");
    commit(root);
    std::fs::create_dir(root.join("sub")).unwrap();
    for operand in ["./grit+pathhex:6162", "sub/../grit+pathhex:6162"] {
        assert_literal_prefix_blame(root, root, operand);
    }
    git(root, &["fsck", "--strict"]);
}

#[test]
fn cwd_relative_literal_prefix_does_not_decode_after_normalization() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    setup(root);
    write(root, b"ab", "decoy\n");
    write(root, b"grit+pathhex:6162", "literal\n");
    commit(root);
    let cwd = root.join("sub");
    std::fs::create_dir(&cwd).unwrap();
    assert_literal_prefix_blame(root, &cwd, "./../grit+pathhex:6162");
    // An explicitly stored path still bypasses cwd resolution and decodes only once.
    assert_literal_prefix_blame(root, &cwd, &storage_string(b"grit+pathhex:6162"));
    git(root, &["fsck", "--strict"]);
}
