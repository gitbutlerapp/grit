//! tree-sha256 digest compatibility with plain Git reproduction and golden vectors.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::objects::{serialize_tree, HashAlgo, ObjectId, ObjectKind, TreeEntry};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::tree_sha256::{
    path_matches_glob, tree_sha256, MissingSubmoduleReason, TreeSha256, TreeSha256Options,
};

fn init_repo(dir: &Path) -> Repository {
    Repository::init(dir, &InitOptions::default()).expect("init")
}

fn git(dir: &Path, args: &[&str]) -> String {
    grit_test_support::git(dir, args)
}

fn plain_git_tree_sha256(repo_dir: &Path, rev: &str) -> String {
    let listing = git(repo_dir, &["ls-tree", "-r", "-z", "--full-name", rev]);
    let mut records: Vec<Vec<u8>> = Vec::new();
    for chunk in listing.split('\0').filter(|s| !s.is_empty()) {
        let sp = chunk.find(' ').expect("mode");
        let rest = &chunk[sp + 1..];
        let sp2 = rest.find(' ').expect("type");
        let kind = &rest[..sp2];
        let rest2 = &rest[sp2 + 1..];
        let sp3 = rest2.find('\t').expect("tab");
        let oid_hex = &rest2[..sp3];
        let path = &rest2[sp3 + 1..];
        if kind == "tree" || kind == "commit" {
            continue;
        }
        let blob = git(repo_dir, &["cat-file", "-p", oid_hex]);
        let hex = hex_bytes(&HashAlgo::Sha256.checksum_digest(blob.as_bytes()));
        records.push(file_record_bytes(&hex, path));
    }
    records.sort_by(|a, b| record_path_key(a).cmp(record_path_key(b)));
    let mut concat = Vec::new();
    for r in &records {
        concat.extend_from_slice(r);
    }
    hex_bytes(&HashAlgo::Sha256.checksum_digest(&concat))
}

fn hex_bytes(d: &[u8]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect::<String>()
}

fn file_record_bytes(content_hex: &str, path: &str) -> Vec<u8> {
    let mut rec = Vec::new();
    rec.extend_from_slice(content_hex.as_bytes());
    rec.push(b' ');
    rec.extend_from_slice(path.as_bytes());
    rec.push(0);
    rec
}

fn record_path_key(record: &[u8]) -> &[u8] {
    let sp = record.iter().position(|&b| b == b' ').unwrap();
    let end = record.iter().rposition(|&b| b == 0).unwrap();
    &record[sp + 1..end]
}

#[test]
fn digest_matches_plain_git_reproduction() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main"]);
    fs::write(dir.join("a.txt"), b"alpha\n").unwrap();
    fs::create_dir(dir.join("sub")).unwrap();
    fs::write(dir.join("sub/b.txt"), b"beta\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-m", "seed"]);
    let repo = Repository::discover(Some(dir), &RepositoryOptions::empty()).unwrap();
    let oid: ObjectId = git(dir, &["rev-parse", "HEAD"]).trim().parse().unwrap();
    let report = tree_sha256(&repo, oid, &TreeSha256Options::default()).unwrap();
    let plain = plain_git_tree_sha256(dir, "HEAD");
    assert_eq!(report.digest.to_hex(), plain);
}

#[test]
fn empty_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_repo(tmp.path());
    let empty_tree = repo
        .odb()
        .write_loose_materialize(ObjectKind::Tree, b"")
        .unwrap();
    let commit_body =
        format!("tree {empty_tree}\nauthor T <t@e.com> 1 +0000\ncommitter T <t@e.com> 1 +0000\n\n");
    let commit = repo
        .odb()
        .write_loose_materialize(ObjectKind::Commit, commit_body.as_bytes())
        .unwrap();
    let empty_digest = <TreeSha256 as std::str::FromStr>::from_str(
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
    )
    .unwrap();
    let report = tree_sha256(&repo, commit, &TreeSha256Options::default()).unwrap();
    assert_eq!(report.files, 0);
    assert_eq!(report.digest, empty_digest);
}

#[test]
fn symlink_target_hashed_mode_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main"]);
    fs::write(dir.join("target"), b"link-target\n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("target", dir.join("the-link")).unwrap();
    #[cfg(not(unix))]
    return;
    git(dir, &["add", "."]);
    git(dir, &["commit", "-m", "symlink"]);
    let repo = Repository::discover(Some(dir), &RepositoryOptions::empty()).unwrap();
    let head: ObjectId = git(dir, &["rev-parse", "HEAD"]).trim().parse().unwrap();
    let report = tree_sha256(&repo, head, &TreeSha256Options::default()).unwrap();
    let plain = plain_git_tree_sha256(dir, "HEAD");
    assert_eq!(report.digest.to_hex(), plain);
    assert_eq!(report.files, 2);
}

#[test]
fn excluded_submodule_recorded_with_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let super_dir = tmp.path();
    let sub_dir = super_dir.join("vendor");
    fs::create_dir(&sub_dir).unwrap();
    git(&sub_dir, &["init", "-q", "-b", "main"]);
    fs::write(sub_dir.join("x.txt"), b"x\n").unwrap();
    git(&sub_dir, &["add", "x.txt"]);
    git(&sub_dir, &["commit", "-m", "sub"]);
    let sub_head = git(&sub_dir, &["rev-parse", "HEAD"]).trim().to_string();
    git(super_dir, &["init", "-q", "-b", "main"]);
    fs::write(
        super_dir.join(".gitmodules"),
        format!(
            "[submodule \"vendor\"]\n\tpath = vendor\n\turl = file://{}\n",
            sub_dir.display()
        ),
    )
    .unwrap();
    Command::new("git")
        .current_dir(super_dir)
        .args([
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{sub_head},vendor"),
        ])
        .status()
        .unwrap();
    git(super_dir, &["add", ".gitmodules"]);
    git(super_dir, &["commit", "-m", "super"]);
    let repo = Repository::discover(Some(super_dir), &RepositoryOptions::empty()).unwrap();
    let head: ObjectId = git(super_dir, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();
    let mut opts = TreeSha256Options::default();
    opts.exclude_globs = vec!["vendor".into()];
    let report = tree_sha256(&repo, head, &opts).unwrap();
    assert_eq!(report.submodules, 0);
    assert_eq!(report.excluded.len(), 1);
    assert_eq!(report.excluded[0].0, "vendor");
    assert_eq!(report.excluded[0].1.to_hex(), sub_head);
}

#[test]
fn missing_submodules_all_listed() {
    let tmp = tempfile::tempdir().unwrap();
    let super_dir = tmp.path();
    git(super_dir, &["init", "-q", "-b", "main"]);
    let fake: ObjectId = "0000000000000000000000000000000000000001".parse().unwrap();
    Command::new("git")
        .current_dir(super_dir)
        .args([
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{},missing-a", fake.to_hex()),
        ])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(super_dir)
        .args([
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{},nested/missing-b", fake.to_hex()),
        ])
        .status()
        .unwrap();
    git(super_dir, &["commit", "-m", "links"]);
    let repo = Repository::discover(Some(super_dir), &RepositoryOptions::empty()).unwrap();
    let head: ObjectId = git(super_dir, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();
    let err = tree_sha256(&repo, head, &TreeSha256Options::default()).unwrap_err();
    match err {
        Error::TreeSha256MissingSubmodules(list) => {
            assert_eq!(list.len(), 2);
            let paths: Vec<_> = list.iter().map(|m| m.path.as_str()).collect();
            assert!(paths.contains(&"missing-a"));
            assert!(paths.contains(&"nested/missing-b"));
            assert!(list
                .iter()
                .all(|m| m.reason == MissingSubmoduleReason::NotCheckedOut));
        }
        other => panic!("expected missing submodules, got {other:?}"),
    }
}

#[test]
fn nested_submodules_recurse() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let inner = tmp.path().join("inner-src");
    fs::create_dir(&inner).unwrap();
    git(&inner, &["init", "-q", "-b", "main"]);
    fs::write(inner.join("leaf.txt"), b"leaf\n").unwrap();
    git(&inner, &["add", "leaf.txt"]);
    git(&inner, &["commit", "-m", "inner"]);
    let inner_head = git(&inner, &["rev-parse", "HEAD"]).trim().to_string();
    git(root, &["init", "-q", "-b", "main"]);
    fs::write(
        root.join(".gitmodules"),
        format!(
            "[submodule \"inner\"]\n\tpath = inner\n\turl = file://{}\n",
            inner.display()
        ),
    )
    .unwrap();
    Command::new("git")
        .current_dir(root)
        .args([
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{inner_head},inner"),
        ])
        .status()
        .unwrap();
    git(root, &["add", ".gitmodules"]);
    git(root, &["commit", "-m", "root"]);
    install_submodule_checkout(root, "inner", &inner, &inner_head);
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).unwrap();
    let head: ObjectId = git(root, &["rev-parse", "HEAD"]).trim().parse().unwrap();
    let report = tree_sha256(&repo, head, &TreeSha256Options::default()).unwrap();
    assert_eq!(report.submodules, 1);
    assert!(report.files >= 1);
}

fn install_submodule_checkout(super_dir: &Path, name: &str, source_repo: &Path, gitlink: &str) {
    let wt = super_dir.join(name);
    fs::create_dir_all(&wt).unwrap();
    copy_worktree_from(source_repo, &wt);
    let modules = super_dir.join(".git/modules").join(name);
    if modules.exists() {
        fs::remove_dir_all(&modules).ok();
    }
    copy_dir_all(&source_repo.join(".git"), &modules).unwrap();
    grit_lib::config::gitmodules::gitdir::write_submodule_gitfile(&wt, &modules).unwrap();
    grit_lib::config::gitmodules::gitdir::init_submodule_head_from_gitlink(&modules, gitlink)
        .unwrap();
}

fn copy_worktree_from(source: &Path, dest: &Path) {
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let from = entry.path();
        let to = dest.join(name);
        if from.is_dir() {
            copy_dir_all(&from, &to).ok();
        } else {
            fs::copy(&from, &to).ok();
        }
    }
}

fn copy_dir_all(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dest_path = to.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &dest_path)?;
        } else {
            fs::copy(entry.path(), dest_path)?;
        }
    }
    Ok(())
}

#[test]
fn golden_vectors_match_reference_tool() {
    let golden_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tree_sha256/golden.json");
    if !golden_path.is_file() {
        eprintln!("SKIP golden_vectors: run scripts/gen_tree_sha256_golden.sh first");
        return;
    }
    let raw = fs::read_to_string(&golden_path).unwrap();
    let entries: Vec<GoldenEntry> = serde_json::from_str(&raw).unwrap();
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for entry in entries {
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = open_golden_repo(&manifest, &entry, tmp.path());
        let repo = Repository::discover(Some(&repo_path), &RepositoryOptions::empty()).unwrap();
        let oid: ObjectId = entry.rev.parse().unwrap();
        let report = tree_sha256(&repo, oid, &TreeSha256Options::default()).unwrap();
        assert_eq!(
            report.digest.to_hex(),
            entry.digest,
            "golden mismatch for {}",
            entry.name
        );
    }
}

fn open_golden_repo(manifest: &Path, entry: &GoldenEntry, scratch: &Path) -> PathBuf {
    if let Some(bundle) = &entry.bundle {
        let bundle_path = if bundle.is_absolute() {
            bundle.clone()
        } else {
            manifest.join(bundle)
        };
        let dest = scratch.join("repo");
        let status = Command::new("git")
            .args([
                "clone",
                "--quiet",
                bundle_path.to_str().expect("bundle path utf-8"),
                dest.to_str().expect("dest utf-8"),
            ])
            .status()
            .expect("git clone bundle");
        assert!(status.success(), "git clone from bundle failed");
        dest
    } else {
        let repo = entry
            .repo
            .as_ref()
            .expect("golden entry needs repo or bundle");
        if repo.is_absolute() {
            repo.clone()
        } else {
            manifest.join(repo)
        }
    }
}

#[derive(serde::Deserialize)]
struct GoldenEntry {
    name: String,
    #[serde(default)]
    repo: Option<PathBuf>,
    #[serde(default)]
    bundle: Option<PathBuf>,
    rev: String,
    digest: String,
}

#[test]
fn sha256_object_format_repo() {
    if !grit_test_support::git_supports_sha256() {
        eprintln!("SKIP: system git lacks sha256 object format");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    git(tmp.path(), &["init", "-q", "--object-format=sha256"]);
    fs::write(tmp.path().join("f"), b"sha256 repo\n").unwrap();
    git(tmp.path(), &["add", "f"]);
    git(tmp.path(), &["commit", "-m", "c"]);
    let repo = Repository::discover(Some(tmp.path()), &RepositoryOptions::empty()).unwrap();
    let head: ObjectId = git(tmp.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();
    let report = tree_sha256(&repo, head, &TreeSha256Options::default()).unwrap();
    assert_eq!(report.files, 1);
    let plain = plain_git_tree_sha256(tmp.path(), "HEAD");
    assert_eq!(report.digest.to_hex(), plain);
}

#[test]
fn glob_exclusion_unit() {
    assert!(path_matches_glob("vendor/foo", "vendor/*"));
    assert!(!path_matches_glob("other", "vendor/*"));
}

#[test]
fn non_utf8_and_backslash_tree_entry_names() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_repo(tmp.path());
    let odb = repo.odb();
    let blob_a = odb
        .write_loose_materialize(ObjectKind::Blob, b"a\n")
        .unwrap();
    let blob_b = odb
        .write_loose_materialize(ObjectKind::Blob, b"b\n")
        .unwrap();
    let tree_body = serialize_tree(&[
        TreeEntry {
            mode: 0o100644,
            name: b"non-utf8-\xff".to_vec(),
            oid: blob_a,
        },
        TreeEntry {
            mode: 0o100644,
            name: b"back\\slash".to_vec(),
            oid: blob_b,
        },
    ]);
    let tree = odb
        .write_loose_materialize(ObjectKind::Tree, &tree_body)
        .unwrap();
    let commit_body =
        format!("tree {tree}\nauthor T <t@e.com> 1 +0000\ncommitter T <t@e.com> 1 +0000\n\n");
    let commit = odb
        .write_loose_materialize(ObjectKind::Commit, commit_body.as_bytes())
        .unwrap();
    let report = tree_sha256(&repo, commit, &TreeSha256Options::default()).unwrap();
    assert_eq!(report.files, 2);
    let expected = plain_tree_sha256_from_records(
        &repo,
        &[(blob_a, b"non-utf8-\xff"), (blob_b, b"back\\slash")],
    );
    assert_eq!(report.digest.to_hex(), expected);
}

fn plain_tree_sha256_from_records(repo: &Repository, entries: &[(ObjectId, &[u8])]) -> String {
    let mut records: Vec<Vec<u8>> = Vec::with_capacity(entries.len());
    for (oid, path) in entries {
        let obj = repo.odb().read(oid).unwrap();
        let hex = hex_bytes(&HashAlgo::Sha256.checksum_digest(&obj.data));
        records.push(file_record_bytes_raw(&hex, path));
    }
    records.sort_by(|a, b| record_path_key(a).cmp(record_path_key(b)));
    let mut concat = Vec::new();
    for r in &records {
        concat.extend_from_slice(r);
    }
    hex_bytes(&HashAlgo::Sha256.checksum_digest(&concat))
}

fn file_record_bytes_raw(content_hex: &str, path: &[u8]) -> Vec<u8> {
    let mut rec = Vec::new();
    rec.extend_from_slice(content_hex.as_bytes());
    rec.push(b' ');
    rec.extend_from_slice(path);
    rec.push(0);
    rec
}

#[test]
fn gitlink_commit_in_super_odb_still_requires_checkout() {
    let tmp = tempfile::tempdir().unwrap();
    let sub_dir = tmp.path().join("sub-src");
    fs::create_dir(&sub_dir).unwrap();
    git(&sub_dir, &["init", "-q", "-b", "main"]);
    fs::write(sub_dir.join("m.txt"), b"m\n").unwrap();
    git(&sub_dir, &["add", "m.txt"]);
    git(&sub_dir, &["commit", "-m", "sub"]);
    let sub_head: ObjectId = git(&sub_dir, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();

    let super_dir = tmp.path().join("super");
    fs::create_dir(&super_dir).unwrap();
    git(&super_dir, &["init", "-q", "-b", "main"]);
    fs::write(
        super_dir.join(".gitmodules"),
        format!(
            "[submodule \"module\"]\n\tpath = module\n\turl = file://{}\n",
            sub_dir.display()
        ),
    )
    .unwrap();
    Command::new("git")
        .current_dir(&super_dir)
        .args([
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{},module", sub_head.to_hex()),
        ])
        .status()
        .unwrap();
    git(&super_dir, &["add", ".gitmodules"]);
    git(&super_dir, &["commit", "-m", "super"]);

    // Pin the gitlink commit into the superproject ODB without checking out `module/`.
    let sub_commit_path = sub_dir
        .join(".git/objects")
        .join(&sub_head.to_hex()[..2])
        .join(&sub_head.to_hex()[2..]);
    let super_obj = super_dir
        .join(".git/objects")
        .join(&sub_head.to_hex()[..2])
        .join(&sub_head.to_hex()[2..]);
    fs::create_dir_all(super_obj.parent().unwrap()).unwrap();
    fs::copy(sub_commit_path, super_obj).unwrap();

    let repo = Repository::discover(Some(&super_dir), &RepositoryOptions::empty()).unwrap();
    let head: ObjectId = git(&super_dir, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();
    let err = tree_sha256(&repo, head, &TreeSha256Options::default()).unwrap_err();
    match err {
        Error::TreeSha256MissingSubmodules(list) => {
            assert_eq!(list.len(), 1);
            assert_eq!(list[0].path, "module");
            assert_eq!(list[0].reason, MissingSubmoduleReason::NotCheckedOut);
        }
        other => panic!("expected missing submodule, got {other:?}"),
    }
}

#[test]
fn nested_submodule_excluded_paths_are_qualified() {
    let tmp = tempfile::tempdir().unwrap();
    let nest = tmp.path().join("nest-src");
    fs::create_dir(&nest).unwrap();
    git(&nest, &["init", "-q", "-b", "main"]);
    fs::write(nest.join("n.txt"), b"n\n").unwrap();
    git(&nest, &["add", "n.txt"]);
    git(&nest, &["commit", "-m", "nest"]);
    let nest_head = git(&nest, &["rev-parse", "HEAD"]).trim().to_string();

    let inner = tmp.path().join("inner-src");
    fs::create_dir(&inner).unwrap();
    git(&inner, &["init", "-q", "-b", "main"]);
    fs::write(
        inner.join(".gitmodules"),
        format!(
            "[submodule \"nest\"]\n\tpath = nest\n\turl = file://{}\n",
            nest.display()
        ),
    )
    .unwrap();
    Command::new("git")
        .current_dir(&inner)
        .args([
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{nest_head},nest"),
        ])
        .status()
        .unwrap();
    git(&inner, &["add", ".gitmodules"]);
    git(&inner, &["commit", "-m", "inner"]);
    let inner_head = git(&inner, &["rev-parse", "HEAD"]).trim().to_string();

    let root = tmp.path().join("root");
    fs::create_dir(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(
        root.join(".gitmodules"),
        format!(
            "[submodule \"inner\"]\n\tpath = inner\n\turl = file://{}\n",
            inner.display()
        ),
    )
    .unwrap();
    Command::new("git")
        .current_dir(&root)
        .args([
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{inner_head},inner"),
        ])
        .status()
        .unwrap();
    git(&root, &["add", ".gitmodules"]);
    git(&root, &["commit", "-m", "root"]);
    install_submodule_checkout(&root, "inner", &inner, &inner_head);

    let repo = Repository::discover(Some(&root), &RepositoryOptions::empty()).unwrap();
    let head: ObjectId = git(&root, &["rev-parse", "HEAD"]).trim().parse().unwrap();
    let mut opts = TreeSha256Options::default();
    opts.exclude_exact = vec!["nest".into()];
    let report = tree_sha256(&repo, head, &opts).unwrap();
    assert_eq!(report.excluded.len(), 1);
    assert_eq!(report.excluded[0].0, "inner/nest");
}
