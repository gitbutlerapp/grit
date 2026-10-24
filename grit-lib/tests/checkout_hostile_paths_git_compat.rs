//! Hostile tree entry names must be rejected at checkout/index population, matching system `git`.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::Environment;
use grit_lib::error::Error;
use grit_lib::objects::parse_commit;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::checkout::checkout_between_trees;
use grit_lib::porcelain::merge::tree_to_index_entries;
use grit_lib::porcelain::reset::{reset_to_commit, ResetMode, ResetOptions};
use grit_lib::porcelain::restore::{restore_paths, RestoreOptions, RestoreSource};
use grit_lib::porcelain::switch::{resolve_switch_target, switch_repository, SwitchOptions};
use grit_lib::progress::NeverCancel;
use grit_lib::remote::clone::{clone as clone_repo, CloneError, CloneOptions};
use grit_lib::remote::fetch::NoProgress;
use grit_lib::repo::{InitOptions, Repository};
use grit_test_support::{git, git_hash_object_literally, HashAlgo};

fn grit_bin() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("GRIT_BIN") {
        let path = std::path::PathBuf::from(p);
        if path.is_file() {
            return path;
        }
    }
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest.parent().expect("workspace root");
    let target_dir = std::env::var("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| workspace.join("target"));
    for subpath in ["debug/grit", "release/grit"] {
        let candidate = target_dir.join(subpath);
        if candidate.is_file() {
            return candidate;
        }
    }
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_grit") {
        let path = std::path::PathBuf::from(&p);
        if path.is_absolute() && path.is_file() {
            return path;
        }
        let from_workspace = workspace.join(&path);
        if from_workspace.is_file() {
            return from_workspace;
        }
    }
    panic!(
        "grit binary not found — run `cargo build -p grit-cli` or set GRIT_BIN (looked under {})",
        target_dir.display()
    );
}

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

struct HostileCaseMeta {
    label: &'static str,
    upstream_config: &'static str,
}

fn hostile_case_metas() -> &'static [HostileCaseMeta] {
    &[
        HostileCaseMeta {
            label: "dotdot directory",
            upstream_config: "",
        },
        HostileCaseMeta {
            label: "dot",
            upstream_config: "",
        },
        HostileCaseMeta {
            label: "dotgit",
            upstream_config: "",
        },
        HostileCaseMeta {
            label: "dotgit upper",
            upstream_config: "",
        },
        HostileCaseMeta {
            label: "git tilde1",
            upstream_config: "",
        },
        HostileCaseMeta {
            label: "ntfs trailing dot",
            upstream_config: "",
        },
        HostileCaseMeta {
            label: "ntfs trailing space",
            upstream_config: "",
        },
        HostileCaseMeta {
            label: "ntfs index allocation",
            upstream_config: "",
        },
        HostileCaseMeta {
            label: "hfs dotgit",
            upstream_config: "[core]\n\tprotectHFS = true\n",
        },
        HostileCaseMeta {
            label: "ntfs backslash",
            upstream_config: "",
        },
        HostileCaseMeta {
            label: "ntfs backslash dotgit config",
            upstream_config: "",
        },
        HostileCaseMeta {
            label: "symlink gitmodules",
            upstream_config: "",
        },
    ]
}

fn hostile_tree_body(
    label: &str,
    upstream: &Path,
    blob_hex: &str,
    symlink_target_hex: &str,
) -> Vec<u8> {
    match label {
        "dotdot directory" => {
            let escaped_blob = write_blob(upstream, b"escaped");
            let inner = write_tree(upstream, &tree_entry("100644", b"escaped", &escaped_blob));
            tree_entry("40000", b"..", &inner)
        }
        "dot" => tree_entry("100644", b".", blob_hex),
        "dotgit" => tree_entry("100644", b".git", blob_hex),
        "dotgit upper" => tree_entry("100644", b".GIT", blob_hex),
        "git tilde1" => tree_entry("100644", b"git~1", blob_hex),
        "ntfs trailing dot" => tree_entry("100644", b".git.", blob_hex),
        "ntfs trailing space" => tree_entry("100644", b".git ", blob_hex),
        "ntfs index allocation" => tree_entry("100644", b".git::$INDEX_ALLOCATION", blob_hex),
        "hfs dotgit" => tree_entry("100644", ".gI\u{200c}T".as_bytes(), blob_hex),
        "ntfs backslash" => tree_entry("100644", br"a\..\x", blob_hex),
        "ntfs backslash dotgit config" => tree_entry("100644", b".git\\config", blob_hex),
        "symlink gitmodules" => tree_entry("120000", b".gitmodules", symlink_target_hex),
        other => panic!("unknown hostile case {other}"),
    }
}

fn init_bare_upstream(upstream: &Path, evil_tree: &str, extra_config: &str) -> (String, String) {
    if !extra_config.is_empty() {
        append_local_config(upstream, extra_config);
    }
    let safe_commit = commit_tree(upstream, &write_tree(upstream, b""), None);
    git(upstream, &["update-ref", "refs/heads/main", &safe_commit]);
    let evil_commit = commit_tree(upstream, evil_tree, None);
    git(upstream, &["update-ref", "refs/heads/evil", &evil_commit]);
    git(upstream, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    (safe_commit, evil_commit)
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

fn grit_lib_clone_hostile_path_rejected(
    upstream: &Path,
    dest: &Path,
    evil_tree: &str,
    case_label: &str,
    local_config: &str,
) -> Error {
    git(upstream, &["symbolic-ref", "HEAD", "refs/heads/evil"]);
    let url = upstream.to_string_lossy().into_owned();
    let mut opts = CloneOptions {
        url: url.clone(),
        dest: dest.to_path_buf(),
        environment: Environment::empty(),
        ..CloneOptions::new(url, dest.to_path_buf())
    };
    // HFS alias names are enforced at explicit checkout on this platform; system git
    // also skips clone-failure assertions for this case (see below).
    if case_label == "hfs dotgit" {
        opts.checkout = false;
        let outcome = clone_repo(opts, &mut NoProgress, &NeverCancel, None)
            .unwrap_or_else(|e| panic!("{case_label}: clone fetch: {e:?}"));
        if !local_config.is_empty() {
            append_local_config(outcome.repo.git_dir(), local_config);
            outcome.repo.reload_config().expect("reload config");
        }
        let tree_oid: ObjectId = evil_tree.parse().expect("tree oid");
        let checkout_result = checkout_between_trees(&outcome.repo, None, &tree_oid);
        assert!(
            checkout_result.is_err(),
            "{case_label}: checkout should fail for hostile tree {evil_tree}: {checkout_result:?}"
        );
        return checkout_result.unwrap_err();
    }
    match clone_repo(opts, &mut NoProgress, &NeverCancel, None) {
        Ok(outcome) => panic!(
            "{case_label}: clone should not succeed with hostile default branch (checked out {} files)",
            outcome.files
        ),
        Err(CloneError::Library(err)) => err,
        Err(other) => {
            panic!("{case_label}: expected InvalidPathComponent via Library error, got {other:?}")
        }
    }
}

fn grit_cli_clone_fails(upstream: &Path, dest: &Path) -> bool {
    git(upstream, &["symbolic-ref", "HEAD", "refs/heads/evil"]);
    let parent = dest.parent().unwrap_or(dest);
    fs::create_dir_all(parent).ok();
    if dest.exists() {
        fs::remove_dir_all(dest).ok();
    }
    let name = dest.file_name().unwrap().to_str().unwrap();
    let status = Command::new(grit_bin())
        .current_dir(parent)
        .args(["clone", upstream.to_str().unwrap(), name])
        .status()
        .expect("grit clone status");
    !status.success()
}

fn git_clone_fails(upstream: &Path, dest: &Path) -> bool {
    git(upstream, &["symbolic-ref", "HEAD", "refs/heads/evil"]);
    let parent = dest.parent().unwrap_or(dest);
    fs::create_dir_all(parent).ok();
    if dest.exists() {
        fs::remove_dir_all(dest).ok();
    }
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
    let outcome = clone_repo(opts, &mut NoProgress, &NeverCancel, None).expect("clone main");
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
        &NeverCancel,
        None,
    )
    .expect("clone");
    let tree_oid: ObjectId = outer.parse().expect("tree");
    let err = tree_to_index_entries(&outcome.repo, &tree_oid, "").expect_err("dotdot tree");
    assert_invalid_path_component(err);
}

#[test]
fn hostile_paths_rejected_on_clone_switch_reset_restore() {
    let parent = tempfile::tempdir().expect("parent");
    let upstream = parent.path().join("upstream.git");
    fs::create_dir_all(upstream.parent().unwrap()).expect("parent dir");

    for case in hostile_case_metas() {
        if case.label == "ntfs backslash" && !cfg!(windows) {
            continue;
        }
        fs::remove_dir_all(&upstream).ok();
        git(
            upstream.parent().unwrap(),
            &[
                "init",
                "--bare",
                upstream.file_name().unwrap().to_str().unwrap(),
            ],
        );
        let blob_hex = write_blob(&upstream, b"x");
        let symlink_hex = write_blob(&upstream, b"target");
        let tree_body = hostile_tree_body(case.label, &upstream, &blob_hex, &symlink_hex);
        let evil_tree = write_tree(&upstream, &tree_body);
        let (_safe, evil_commit) = init_bare_upstream(&upstream, &evil_tree, case.upstream_config);
        let evil_tree_oid: ObjectId = evil_tree.parse().expect("tree");
        let evil_commit_oid: ObjectId = evil_commit.parse().expect("commit");

        let slug = case.label.replace(' ', "-");
        let clone_dest = parent.path().join(format!("clone-{slug}"));
        let err = grit_lib_clone_hostile_path_rejected(
            &upstream,
            &clone_dest,
            &evil_tree,
            case.label,
            case.upstream_config,
        );
        assert_invalid_path_component(err);

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

        if case.label != "hfs dotgit" {
            let git_dest = parent.path().join(format!("git-{slug}"));
            assert!(
                git_clone_fails(&upstream, &git_dest),
                "{}: system git should refuse clone of evil default branch",
                case.label
            );

            let grit_cli_dest = parent.path().join(format!("grit-cli-{slug}"));
            assert!(
                grit_cli_clone_fails(&upstream, &grit_cli_dest),
                "{}: grit clone should refuse evil default branch",
                case.label
            );
        }

        let repo = open_cloned_main(parent.path(), &upstream);
        if !case.upstream_config.is_empty() {
            append_local_config(repo.git_dir(), case.upstream_config);
            repo.reload_config().expect("reload config");
        }
        let wt_snap = dir_bytes_snapshot(repo.work_tree().expect("wt"));
        let git_snap = dir_bytes_snapshot(repo.git_dir());

        let mode = resolve_switch_target(&repo, "evil").expect("resolve evil");
        let switch_err = switch_repository(
            &repo,
            &SwitchOptions {
                mode,
                dry_run: false,
            },
        )
        .unwrap_err();
        assert_invalid_path_component(switch_err);
        assert_eq!(dir_bytes_snapshot(repo.work_tree().expect("wt")), wt_snap);
        assert_eq!(dir_bytes_snapshot(repo.git_dir()), git_snap);

        let reset_err = reset_to_commit(
            &repo,
            &ResetOptions::new(
                evil_commit_oid,
                ResetMode::Hard,
                "T <t@e.com>",
                "reset test",
            ),
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
                stage: None,
                remerge: None,
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

    let blob = write_blob(repo.git_dir(), b"x");
    let tree_body = tree_entry("100644", b"git~1", &blob);
    let tree = write_tree(repo.git_dir(), &tree_body);
    let tree_oid: ObjectId = tree.parse().expect("oid");
    checkout_between_trees(&repo, None, &tree_oid).expect("checkout git~1 with protectNTFS off");
}
