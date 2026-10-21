//! Hostile tree entry names must be rejected at checkout/index population, matching system `git`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::error::Error;
use grit_lib::objects::parse_commit;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::checkout::checkout_between_trees;
use grit_lib::porcelain::reset::{reset_to_commit, ResetMode, ResetOptions};
use grit_lib::porcelain::restore::{restore_paths, RestoreOptions, RestoreSource};
use grit_lib::porcelain::switch::{resolve_switch_target, switch_repository, SwitchOptions};
use grit_lib::remote::clone::{clone as clone_repo, CloneOptions};
use grit_lib::remote::fetch::NoProgress;
use grit_lib::repo::{InitOptions, Repository};
use grit_test_support::{git, git_hash_object_literally, HashAlgo};

const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

fn tree_entry(mode: &str, name: &[u8], oid_hex: &str) -> Vec<u8> {
    let oid = hex::decode(oid_hex).expect("hex");
    let mut out = format!("{mode} ").into_bytes();
    out.extend_from_slice(name);
    out.push(0);
    out.extend_from_slice(&oid);
    out
}

fn write_tree(repo: &Path, body: &[u8]) -> String {
    git_hash_object_literally(repo, HashAlgo::Sha1, "tree", body).expect("hash-object tree")
}

fn write_blob(repo: &Path, data: &[u8]) -> String {
    git_hash_object_literally(repo, HashAlgo::Sha1, "blob", data).expect("hash-object blob")
}

fn commit_tree(repo: &Path, tree: &str, parent: Option<&str>) -> String {
    let mut args = vec!["commit-tree", tree, "-m", "evil"];
    if let Some(p) = parent {
        args.push("-p");
        args.push(p);
    }
    git(repo, &args).trim().to_string()
}

fn append_local_config(git_dir: &Path, section: &str) {
    let path = git_dir.join("config");
    let mut body = fs::read_to_string(&path).unwrap_or_default();
    if !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str(section);
    if !body.ends_with('\n') {
        body.push('\n');
    }
    fs::write(path, body).expect("write config");
}

struct HostileCase {
    label: &'static str,
    tree_body: Vec<u8>,
}

fn build_hostile_cases(_repo: &Path) -> Vec<HostileCase> {
    vec![
        HostileCase {
            label: "dotdot directory",
            tree_body: Vec::new(),
        },
        HostileCase {
            label: "dot",
            tree_body: tree_entry("100644", b".", EMPTY_TREE),
        },
        HostileCase {
            label: "dotgit",
            tree_body: tree_entry("100644", b".git", EMPTY_TREE),
        },
        HostileCase {
            label: "dotgit upper",
            tree_body: tree_entry("100644", b".GIT", EMPTY_TREE),
        },
        HostileCase {
            label: "git tilde1",
            tree_body: tree_entry("100644", b"git~1", EMPTY_TREE),
        },
        HostileCase {
            label: "ntfs trailing dot",
            tree_body: tree_entry("100644", b".git.", EMPTY_TREE),
        },
        HostileCase {
            label: "ntfs trailing space",
            tree_body: tree_entry("100644", b".git ", EMPTY_TREE),
        },
        HostileCase {
            label: "ntfs index allocation",
            tree_body: tree_entry("100644", b".git::$INDEX_ALLOCATION", EMPTY_TREE),
        },
        HostileCase {
            label: "hfs dotgit",
            tree_body: tree_entry("100644", ".gI\u{200c}T".as_bytes(), EMPTY_TREE),
        },
        HostileCase {
            label: "ntfs backslash",
            tree_body: tree_entry("100644", br"a\..\x", EMPTY_TREE),
        },
        HostileCase {
            label: "symlink gitmodules",
            tree_body: tree_entry("120000", b".gitmodules", EMPTY_TREE),
        },
    ]
}

fn init_bare_upstream(upstream: &Path, evil_tree: &str) -> (String, String, String) {
    let safe_commit = commit_tree(upstream, EMPTY_TREE, None);
    git(upstream, &["update-ref", "refs/heads/main", &safe_commit]);
    let evil_commit = commit_tree(upstream, evil_tree, None);
    git(upstream, &["update-ref", "refs/heads/evil", &evil_commit]);
    git(upstream, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    (safe_commit, evil_commit, evil_tree.to_string())
}

fn dir_bytes_snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    snapshot_walk(root, root, &mut out);
    out
}

fn snapshot_walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
    let entries = fs::read_dir(dir).expect("read_dir");
    for ent in entries {
        let ent = ent.expect("dirent");
        let path = ent.path();
        let rel = path
            .strip_prefix(base)
            .expect("prefix")
            .to_string_lossy()
            .into_owned();
        if ent.file_type().expect("type").is_dir() {
            snapshot_walk(base, &path, out);
        } else {
            out.insert(rel, fs::read(&path).expect("read"));
        }
    }
}

fn assert_invalid_path_component(err: Error) {
    assert!(
        matches!(err, Error::InvalidPathComponent { .. }),
        "expected InvalidPathComponent, got {err:?}"
    );
}

fn grit_clone_then_checkout_evil(
    upstream: &Path,
    dest: &Path,
    evil_tree: &str,
    case_label: &str,
) -> (grit_lib::remote::clone::CloneOutcome, Error) {
    git(upstream, &["symbolic-ref", "HEAD", "refs/heads/evil"]);
    let opts = CloneOptions {
        url: upstream.to_string_lossy().into_owned(),
        dest: dest.to_path_buf(),
        environment: Environment::empty(),
        ..CloneOptions::new(upstream.to_string_lossy(), dest.to_path_buf())
    };
    let outcome = clone_repo(opts, &mut NoProgress, None).expect("clone fetch");
    let tree_oid: ObjectId = evil_tree.parse().expect("tree oid");
    let checkout_result = checkout_between_trees(&outcome.repo, None, &tree_oid);
    assert!(
        checkout_result.is_err(),
        "{case_label}: checkout should fail for hostile tree {evil_tree}: {checkout_result:?}"
    );
    let err = checkout_result.unwrap_err();
    (outcome, err)
}

fn git_clone_fails(upstream: &Path, dest: &Path) -> bool {
    git(upstream, &["symbolic-ref", "HEAD", "refs/heads/evil"]);
    let parent = dest.parent().unwrap_or(dest);
    fs::create_dir_all(parent).ok();
    let name = dest.file_name().unwrap().to_str().unwrap();
    let status = Command::new("git")
        .current_dir(parent)
        .args(["clone", "-q", upstream.to_str().unwrap(), name])
        .status()
        .expect("git clone status");
    !status.success()
}

fn open_cloned_main(parent: &Path, upstream: &Path) -> Repository {
    git(upstream, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    let dest = parent.join("work");
    if dest.exists() {
        fs::remove_dir_all(&dest).expect("rm work");
    }
    let opts = CloneOptions {
        url: upstream.to_string_lossy().into_owned(),
        dest: dest.clone(),
        environment: Environment::empty(),
        ..CloneOptions::new(upstream.to_string_lossy(), dest.clone())
    };
    let outcome = clone_repo(opts, &mut NoProgress, None).expect("clone main");
    let commit = outcome
        .repo
        .odb()
        .read(&outcome.checkout_oid)
        .expect("commit");
    let parsed = parse_commit(&commit.data).expect("parse");
    checkout_between_trees(&outcome.repo, None, &parsed.tree).expect("checkout main");
    outcome.repo
}

#[test]
fn dotdot_tree_rejected_at_index_population() {
    let upstream = tempfile::tempdir().expect("upstream");
    git(
        upstream.path(),
        &["init", "--bare", upstream.path().to_str().unwrap()],
    );
    let escaped_blob = write_blob(upstream.path(), b"escaped");
    let inner = write_tree(
        upstream.path(),
        &tree_entry("100644", b"escaped", &escaped_blob),
    );
    let outer = write_tree(upstream.path(), &tree_entry("40000", b"..", &inner));
    let commit = commit_tree(upstream.path(), &outer, None);
    git(upstream.path(), &["update-ref", "refs/heads/evil", &commit]);
    let dest = tempfile::tempdir().expect("dest");
    let outcome = clone_repo(
        CloneOptions {
            url: upstream.path().to_string_lossy().into_owned(),
            dest: dest.path().to_path_buf(),
            environment: Environment::empty(),
            ..CloneOptions::new(upstream.path().to_string_lossy(), dest.path().to_path_buf())
        },
        &mut NoProgress,
        None,
    )
    .expect("clone");
    let tree_oid: ObjectId = outer.parse().expect("tree");
    let err = grit_lib::porcelain::merge::tree_to_index_entries(&outcome.repo, &tree_oid, "")
        .expect_err("dotdot tree");
    assert_invalid_path_component(err);
}

#[test]
fn hostile_paths_rejected_on_clone_switch_reset_restore() {
    let parent = tempfile::tempdir().expect("parent");
    let upstream = parent.path().join("upstream.git");

    for case in build_hostile_cases(Path::new("/")) {
        fs::remove_dir_all(&upstream).ok();
        fs::create_dir_all(upstream.parent().unwrap()).expect("parent dir");
        git(
            upstream.parent().unwrap(),
            &[
                "init",
                "--bare",
                upstream.file_name().unwrap().to_str().unwrap(),
            ],
        );
        let tree_body = match case.label {
            "dotdot directory" => {
                let escaped_blob = write_blob(&upstream, b"escaped");
                let inner = write_tree(&upstream, &tree_entry("100644", b"escaped", &escaped_blob));
                tree_entry("40000", b"..", &inner)
            }
            _ => case.tree_body,
        };
        let evil_tree = write_tree(&upstream, &tree_body);
        if case.label == "hfs dotgit" {
            let dest = parent.path().join("work");
            if dest.exists() {
                fs::remove_dir_all(&dest).ok();
            }
        }
        let (_safe, evil_commit, _evil_tree_hex) = init_bare_upstream(&upstream, &evil_tree);
        let evil_tree_oid: ObjectId = evil_tree.parse().expect("tree");
        let evil_commit_oid: ObjectId = evil_commit.parse().expect("commit");

        let clone_dest = parent
            .path()
            .join(format!("clone-{}", case.label.replace(' ', "-")));
        if case.label != "hfs dotgit" {
            let (_clone_outcome, err) =
                grit_clone_then_checkout_evil(&upstream, &clone_dest, &evil_tree, case.label);
            assert_invalid_path_component(err);
        }

        if case.label != "hfs dotgit" {
            let git_dir = clone_dest.join(".git");
            let snap_wt_after = dir_bytes_snapshot(&clone_dest);
            assert!(
                !parent.path().join("escaped").exists(),
                "{}: file escaped outside clone parent",
                case.label
            );
            assert!(
                !snap_wt_after
                    .keys()
                    .any(|p| p.contains("escaped") || p.starts_with("..")),
                "{}: hostile path materialized in work tree: {:?}",
                case.label,
                snap_wt_after.keys().collect::<Vec<_>>()
            );
        }

        let git_dest = parent
            .path()
            .join(format!("git-{}", case.label.replace(' ', "-")));
        assert!(
            git_clone_fails(&upstream, &git_dest),
            "{}: system git should refuse clone of evil default branch",
            case.label
        );

        let repo = open_cloned_main(parent.path(), &upstream);
        if case.label == "hfs dotgit" {
            append_local_config(repo.git_dir(), "[core]\n\tprotectHFS = true\n");
            repo.reload_config().expect("reload protectHFS");
        }
        let wt_snap = dir_bytes_snapshot(repo.work_tree().expect("wt"));
        let git_snap = dir_bytes_snapshot(repo.git_dir());

        let mode = resolve_switch_target(&repo, "evil").expect("resolve evil");
        let switch_err = switch_repository(&repo, &SwitchOptions { mode }).unwrap_err();
        assert_invalid_path_component(switch_err);
        assert_eq!(dir_bytes_snapshot(repo.work_tree().expect("wt")), wt_snap);
        assert_eq!(dir_bytes_snapshot(repo.git_dir()), git_snap);

        let reset_err = reset_to_commit(
            &repo,
            &ResetOptions {
                target: evil_commit_oid,
                mode: ResetMode::Hard,
                identity: "T <t@e.com>".into(),
                message: "reset test".into(),
            },
        )
        .unwrap_err();
        assert_invalid_path_component(reset_err);
        assert_eq!(dir_bytes_snapshot(repo.work_tree().expect("wt")), wt_snap);
        assert_eq!(dir_bytes_snapshot(repo.git_dir()), git_snap);

        let restore_err = restore_paths(
            &repo,
            &RestoreOptions {
                pathspecs: vec![".".into()],
                pathspec_sources: vec![".".into()],
                source: RestoreSource::Tree(evil_tree_oid),
                staged: false,
                worktree: true,
            },
        )
        .unwrap_err();
        assert_invalid_path_component(restore_err);
        assert_eq!(dir_bytes_snapshot(repo.work_tree().expect("wt")), wt_snap);
        assert_eq!(dir_bytes_snapshot(repo.git_dir()), git_snap);
    }
}

#[test]
fn git_tilde1_allowed_when_protect_ntfs_disabled() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    git(dir.path(), &["config", "core.protectNTFS", "false"]);
    repo.reload_config().expect("reload config");

    let tree_body = tree_entry("100644", b"git~1", EMPTY_TREE);
    let tree = write_tree(repo.git_dir(), &tree_body);
    let tree_oid: ObjectId = tree.parse().expect("oid");
    checkout_between_trees(&repo, None, &tree_oid).expect("checkout git~1 with protectNTFS off");
}
