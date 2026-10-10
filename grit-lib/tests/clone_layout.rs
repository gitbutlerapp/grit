//! Clone disk layout: pack retention, packed-refs, origin/HEAD, reflogs, tags.

use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::pack::verify_pack_and_collect;
use grit_lib::repo::init_repository;
use grit_lib::transfer::{fetch_local, CloneReflog, FetchOptions, TagMode};

fn git_in(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn for_each_ref(dir: &Path) -> Vec<String> {
    git_out(
        dir,
        &["for-each-ref", "--format=%(refname) %(objectname)", "refs/"],
    )
    .map(|s| {
        let mut lines: Vec<String> = s.lines().map(str::to_owned).collect();
        lines.sort();
        lines
    })
    .unwrap_or_default()
}

fn collect_reflog_files(logs_root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![logs_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                files.push(path.strip_prefix(logs_root).unwrap().to_path_buf());
            }
        }
    }
    files.sort();
    files
}

#[test]
fn clone_fetch_keeps_pack_and_matches_git_layout() {
    let upstream = tempfile::tempdir().expect("upstream");
    assert!(git_in(upstream.path(), &["init", "-q", "-b", "main", "."]));
    std::fs::write(upstream.path().join("README"), b"clone layout test\n").unwrap();
    git_in(upstream.path(), &["add", "README"]);
    git_in(
        upstream.path(),
        &[
            "-c",
            "user.email=t@e.com",
            "-c",
            "user.name=T",
            "commit",
            "-qm",
            "init",
        ],
    );
    git_in(
        upstream.path(),
        &[
            "-c",
            "user.email=t@e.com",
            "-c",
            "user.name=T",
            "tag",
            "-a",
            "v1.0",
            "-m",
            "annotated release",
        ],
    );
    git_in(upstream.path(), &["tag", "lightweight-tip"]);
    git_in(
        upstream.path(),
        &[
            "-c",
            "user.email=t@e.com",
            "-c",
            "user.name=T",
            "checkout",
            "-b",
            "topic",
        ],
    );
    std::fs::write(upstream.path().join("topic.txt"), b"topic branch\n").unwrap();
    git_in(upstream.path(), &["add", "topic.txt"]);
    git_in(
        upstream.path(),
        &[
            "-c",
            "user.email=t@e.com",
            "-c",
            "user.name=T",
            "commit",
            "-qm",
            "topic work",
        ],
    );
    git_in(upstream.path(), &["checkout", "main"]);
    git_in(
        upstream.path(),
        &[
            "-c",
            "user.email=t@e.com",
            "-c",
            "user.name=T",
            "tag",
            "-a",
            "on-topic",
            "-m",
            "points at topic",
            "topic",
        ],
    );

    let git_clone = tempfile::tempdir().expect("git clone dir");
    assert!(git_in(
        git_clone.path(),
        &["clone", "-q", upstream.path().to_str().unwrap(), ".",],
    ));

    let clone = tempfile::tempdir().expect("clone");
    init_repository(
        clone.path(),
        false,
        "main",
        None,
        grit_lib::RefStorageFormat::Files,
    )
    .expect("init");
    let clone_git = clone.path().join(".git");

    fetch_local(
        &clone_git,
        &upstream.path().join(".git"),
        &FetchOptions {
            refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
            tags: TagMode::Following,
            initial_remote_fetch: true,
            remote_name: Some("origin".to_owned()),
            clone_reflog: Some(CloneReflog {
                identity: "T <t@e.com> 1 +0000".to_owned(),
                message: "clone: from file://fixture".to_owned(),
            }),
            ..Default::default()
        },
    )
    .expect("fetch");

    let default_branch = "main";
    let tracking = format!("refs/remotes/origin/{default_branch}");
    let oid_hex = grit_lib::refs::resolve_ref(&clone_git, &tracking)
        .expect("main tracking ref")
        .to_hex();
    let branch_ref = format!("refs/heads/{default_branch}");
    let oid = grit_lib::objects::ObjectId::from_hex(&oid_hex).unwrap();
    grit_lib::refs::write_ref(&clone_git, &branch_ref, &oid).expect("local branch");
    let zero = grit_lib::objects::ObjectId::zero();
    let identity = "T <t@e.com> 1 +0000";
    let msg = "clone: from file://fixture";
    grit_lib::refs::append_reflog(&clone_git, &branch_ref, &zero, &oid, identity, msg, false)
        .expect("branch reflog");
    grit_lib::refs::write_symbolic_ref(&clone_git, "HEAD", &branch_ref).expect("HEAD");
    grit_lib::refs::append_reflog(&clone_git, "HEAD", &zero, &oid, identity, msg, false)
        .expect("HEAD reflog");

    let count = git_out(&clone.path(), &["count-objects", "-v"]).expect("count-objects");
    assert!(
        count.contains("packs: 1"),
        "expected one pack with default fetch policy, got:\n{count}"
    );
    assert!(
        count.contains("count: 0") || count.lines().any(|l| l.starts_with("count: 0")),
        "expected no loose objects, got:\n{count}"
    );

    assert!(
        clone_git.join("packed-refs").exists(),
        "expected packed-refs"
    );
    assert!(
        clone_git.join("refs/remotes/origin/HEAD").exists(),
        "expected refs/remotes/origin/HEAD"
    );

    let grit_refs = for_each_ref(&clone.path());
    let git_refs = for_each_ref(git_clone.path());
    assert_eq!(
        grit_refs, git_refs,
        "for-each-ref must match parallel git clone"
    );

    let reflogs = collect_reflog_files(&clone_git.join("logs"));
    let expected = [
        PathBuf::from("HEAD"),
        PathBuf::from("refs/heads/main"),
        PathBuf::from("refs/remotes/origin/HEAD"),
    ];
    assert_eq!(
        reflogs.len(),
        expected.len(),
        "expected exactly {expected:?} reflog files, got {reflogs:?}"
    );
    for path in expected {
        assert!(reflogs.contains(&path), "missing reflog file logs/{path:?}");
    }

    let pack_dir = clone_git.join("objects/pack");
    let idx_path = std::fs::read_dir(&pack_dir)
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "idx"))
        .expect("pack index after clone fetch");
    verify_pack_and_collect(&idx_path).expect("pack must pass grit verify-pack before fsck");

    let fsck = Command::new("git")
        .current_dir(&clone.path())
        .args(["fsck", "--strict"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("fsck");
    assert!(
        fsck.status.success(),
        "git fsck --strict failed: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );
}
