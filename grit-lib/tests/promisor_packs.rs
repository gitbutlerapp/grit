//! Promisor packs and partial-clone object visibility — t5616 / t5330 object-level scenarios.

use grit_lib::config::ConfigSet;
use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::odb::Odb;
use grit_lib::pack::clear_pack_cache;
use grit_lib::promisor::{
    promisor_expanded_object_ids, promisor_pack_and_tag_targets, promisor_pack_object_ids,
    promisor_pack_peeled_tag_targets, read_promisor_missing_oids, repo_treats_promisor_packs,
    write_promisor_marker, PROMISOR_MISSING_FILE,
};
use grit_lib::repo::Repository;
use grit_test_support::git_cmd;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

fn git_in(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_WORK_TREE")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} in {}: {}",
        args,
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn setup_filtering_bare_remote() -> (TempDir, PathBuf, TempDir) {
    let layout = TempDir::new().expect("layout");
    let upstream = layout.path().join("upstream");
    git_in(layout.path(), &["init", "-q", "-b", "main", "upstream"]);
    git_in(&upstream, &["config", "user.email", "t@t"]);
    git_in(&upstream, &["config", "user.name", "T"]);
    fs::write(upstream.join("blob.txt"), b"promisor blob payload\n").expect("blob");
    fs::write(upstream.join("second.txt"), b"another blob\n").expect("blob2");
    git_in(&upstream, &["add", "."]);
    git_in(&upstream, &["commit", "-m", "seed", "-q"]);
    fs::write(upstream.join("third.txt"), b"third\n").expect("blob3");
    git_in(&upstream, &["add", "third.txt"]);
    git_in(&upstream, &["commit", "-m", "tip", "-q"]);

    let bare = layout.path().join("upstream.git");
    git_in(
        layout.path(),
        &[
            "clone",
            "-q",
            "--bare",
            upstream.to_str().unwrap(),
            "upstream.git",
        ],
    );
    git_in(&bare, &["config", "uploadpack.allowFilter", "true"]);

    let partial = TempDir::new().expect("partial");
    let url = format!("file://{}", bare.canonicalize().unwrap().display());
    let dest = partial.path().to_str().unwrap();
    let clone = Command::new("git")
        .args([
            "clone",
            "-q",
            "--filter=blob:none",
            "--no-local",
            &url,
            dest,
        ])
        .env_remove("GIT_DIR")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_WORK_TREE")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("clone");
    assert!(
        clone.status.success(),
        "partial clone: {}",
        String::from_utf8_lossy(&clone.stderr)
    );

    (layout, bare, partial)
}

#[test]
fn promisor_partial_clone_pack_markers_and_config() {
    let (_layout, _bare, partial) = setup_filtering_bare_remote();
    let git_dir = partial.path().join(".git");
    let objects = git_dir.join("objects");
    clear_pack_cache();

    let repo = Repository::open(&git_dir, Some(partial.path())).expect("open");
    let cfg = ConfigSet::load(
        &grit_lib::environment::Environment::capture_process(),
        Some(&git_dir),
        true,
    )
    .expect("config");

    let treats = repo_treats_promisor_packs(&git_dir, &cfg);
    assert!(
        treats,
        "partial clone should enable promisor semantics via extensions.partialclone or remote.promisor"
    );

    let promisor_ids = promisor_pack_object_ids(&objects);
    assert!(
        !promisor_ids.is_empty(),
        "partial clone should have promisor-marked packs"
    );

    let head = git_cmd(&["rev-parse", "HEAD"])
        .in_dir(partial.path())
        .exec()
        .stdout;
    let head_oid = ObjectId::from_hex(head.trim()).expect("head");

    assert!(
        repo.odb.exists(&head_oid),
        "commits remain readable in promisor packs"
    );
    assert!(
        !repo.odb.exists_local(&head_oid),
        "promisor-only commit must not count as local materialization"
    );

    let expanded = promisor_expanded_object_ids(&repo).expect("expanded");
    assert!(expanded.contains(&head_oid));

    let _peeled = promisor_pack_peeled_tag_targets(&repo);
    let _both = promisor_pack_and_tag_targets(&repo).expect("pack+tags");

    let tree_hex = git_cmd(&["rev-parse", "HEAD^{tree}"])
        .in_dir(partial.path())
        .exec()
        .stdout;
    let tree = ObjectId::from_hex(tree_hex.trim()).expect("tree oid");
    assert!(repo.odb.exists(&tree));

    let missing_blob_hex = git_cmd(&["rev-parse", "HEAD~1:second.txt"])
        .in_dir(partial.path())
        .exec()
        .stdout;
    let missing_blob = ObjectId::from_hex(missing_blob_hex.trim()).expect("older blob");
    if repo.odb.exists(&missing_blob) {
        return;
    }
    let err = repo.odb.read(&missing_blob).unwrap_err();
    assert!(
        matches!(err, Error::ObjectNotFound(_)),
        "missing filtered blob must be typed not-found, not fetch: {err:?}"
    );
}

#[test]
fn promisor_missing_marker_round_trip() {
    let dir = TempDir::new().expect("tempdir");
    git_in(dir.path(), &["init", "-q", "-b", "main"]);
    let git_dir = dir.path().join(".git");
    let mut set = HashSet::new();
    set.insert(ObjectId::from_hex("0123456789abcdef0123456789abcdef01234567").expect("a"));
    set.insert(ObjectId::from_hex("fedcba9876543210fedcba9876543210fedcba98").expect("b"));
    write_promisor_marker(&git_dir, &set).expect("write");
    let path = git_dir.join(PROMISOR_MISSING_FILE);
    assert!(path.is_file());
    let read_back: HashSet<_> = read_promisor_missing_oids(&git_dir).into_iter().collect();
    assert_eq!(read_back, set);

    write_promisor_marker(&git_dir, &HashSet::new()).expect("clear");
    assert!(read_promisor_missing_oids(&git_dir).is_empty());
}

#[test]
fn promisor_remote_dot_promisor_config_enables_treatment() {
    let dir = TempDir::new().expect("tempdir");
    git_in(dir.path(), &["init", "-q", "--bare"]);
    let git_dir = dir.path().to_path_buf();
    fs::write(
        git_dir.join("config"),
        "[remote \"origin\"]\n\tpromisor = true\n",
    )
    .expect("cfg");
    let cfg = ConfigSet::load(
        &grit_lib::environment::Environment::capture_process(),
        Some(&git_dir),
        true,
    )
    .expect("load");
    assert!(repo_treats_promisor_packs(&git_dir, &cfg));
}

#[test]
fn promisor_exists_vs_exists_local_for_marked_pack() {
    let dir = TempDir::new().expect("tempdir");
    git_in(dir.path(), &["init", "-q", "-b", "main"]);
    git_in(dir.path(), &["config", "user.email", "t@t"]);
    git_in(dir.path(), &["config", "user.name", "T"]);
    fs::write(dir.path().join("x.txt"), b"payload").expect("file");
    git_in(dir.path(), &["add", "x.txt"]);
    git_in(dir.path(), &["commit", "-m", "c", "-q"]);
    git_in(dir.path(), &["repack", "-a", "-d"]);
    let objects = dir.path().join(".git/objects");
    let blob_hex = git_cmd(&["rev-parse", "HEAD:x.txt"])
        .in_dir(dir.path())
        .exec()
        .stdout;
    let oid = ObjectId::from_hex(blob_hex.trim()).expect("oid");
    let pack = fs::read_dir(objects.join("pack"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "pack"))
        .expect("pack");
    fs::write(pack.with_extension("promisor"), b"").expect("marker");

    clear_pack_cache();
    let odb = Odb::new(&objects);
    assert!(
        odb.exists(&oid),
        "promisor pack objects remain visible to exists/read"
    );
    assert!(odb.read(&oid).is_ok());
    assert!(
        !odb.exists_local(&oid),
        "promisor-marked packs are not locally materialized"
    );
}
