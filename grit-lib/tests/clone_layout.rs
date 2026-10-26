//! Clone disk layout: pack retention, packed-refs, origin/HEAD, reflogs, tags.

#![cfg(feature = "http-ureq")]

use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::objects::pack::verify_pack_and_collect;
use grit_lib::porcelain::checkout::checkout_between_trees;
use grit_lib::progress::NeverCancel;
use grit_lib::remote::clone::{clone as clone_repo, CloneOptions};
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::transfer::{fetch_local, CloneReflog, FetchOptions, TagMode};
use grit_lib::repo::{InitOptions, Repository};

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
    assert!(git_in(
        upstream.path(),
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@e.com",
            "tag",
            "-a",
            "nested",
            "-m",
            "nested tag",
            "v1.0"
        ]
    ));
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

    // Exercise clone's pack-retention path; small loose-only sources use loose transfer.
    assert!(git_in(upstream.path(), &["repack", "-ad"]));

    let git_clone = tempfile::tempdir().expect("git clone dir");
    assert!(git_in(
        git_clone.path(),
        &["clone", "-q", upstream.path().to_str().unwrap(), ".",],
    ));

    let clone = tempfile::tempdir().expect("clone");
    Repository::init(
        clone.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
    )
    .expect("init");
    let clone_git = clone.path().join(".git");

    fetch_local(
        &clone_git,
        &upstream.path().join(".git"),
        &FetchOptions {
            refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
            tags: TagMode::All,
            initial_remote_fetch: true,
            remote_name: Some("origin".to_owned()),
            clone_reflog: Some(CloneReflog {
                identity: "T <t@e.com> 1 +0000".parse().expect("identity"),
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

    let packed = std::fs::read_to_string(clone_git.join("packed-refs")).unwrap();
    for name in ["v1.0", "nested", "lightweight-tip", "on-topic"] {
        assert!(!clone_git.join(format!("refs/tags/{name}")).exists());
        assert!(packed.contains(&format!(" refs/tags/{name}\n")), "{packed}");
    }

    for name in ["v1.0", "nested", "on-topic"] {
        let oid = git_out(clone.path(), &["rev-parse", &format!("refs/tags/{name}")]).unwrap();
        let peel = git_out(
            clone.path(),
            &["rev-parse", &format!("refs/tags/{name}^{{}}")],
        )
        .unwrap();
        assert!(
            packed.contains(&format!("{oid} refs/tags/{name}\n^{peel}\n")),
            "{packed}"
        );
    }

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

#[test]
fn library_clone_api_passes_fsck_and_status() {
    let upstream = tempfile::tempdir().expect("upstream");
    assert!(git_in(upstream.path(), &["init", "-q", "-b", "main", "."]));
    std::fs::write(upstream.path().join("tracked.txt"), b"hello\n").unwrap();
    git_in(upstream.path(), &["add", "tracked.txt"]);
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

    let dest = tempfile::tempdir().expect("clone dest");
    let outcome = clone_repo(
        CloneOptions {
            ..CloneOptions::new(upstream.path().to_string_lossy(), dest.path().to_path_buf())
        },
        &mut NoProgress,
        &NeverCancel,
        None,
    )
    .expect("library clone");

    let commit = outcome
        .repo
        .odb()
        .read(&outcome.checkout_oid)
        .expect("commit obj");
    let parsed = grit_lib::objects::parse_commit(&commit.data).expect("parse commit");
    checkout_between_trees(&outcome.repo, None, &parsed.tree).expect("checkout");

    assert!(git_in(dest.path(), &["fsck", "--strict"]));
    let status = git_out(dest.path(), &["status", "--porcelain"]).unwrap_or_default();
    assert!(
        status.trim().is_empty(),
        "expected clean working tree after clone checkout, got:\n{status}"
    );
}

#[test]
fn clone_tag_layout_covers_bare_no_tags_shallow_and_empty() {
    use grit_lib::remote::clone::CloneTags;
    let upstream = tempfile::tempdir().unwrap();
    assert!(git_in(upstream.path(), &["init", "-q", "-b", "main"]));
    assert!(git_in(
        upstream.path(),
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@e.com",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "root"
        ]
    ));
    assert!(git_in(upstream.path(), &["tag", "light"]));
    for (name, target) in [("inner", "HEAD"), ("outer", "inner")] {
        assert!(git_in(
            upstream.path(),
            &[
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@e.com",
                "tag",
                "-a",
                name,
                "-m",
                name,
                target
            ]
        ));
    }
    let url = format!("file://{}", upstream.path().display());
    for mode in ["normal", "bare", "no-tags", "shallow-single"] {
        let dest = tempfile::tempdir().unwrap();
        let control = tempfile::tempdir().unwrap();
        let mut opts = CloneOptions::new(&url, dest.path().to_path_buf());
        let mut git_args = vec!["clone", "-q"];
        match mode {
            "bare" => {
                opts.bare = true;
                git_args.push("--bare");
            }
            "no-tags" => {
                opts.tags = Some(CloneTags::None);
                git_args.push("--no-tags");
            }
            "shallow-single" => {
                opts.depth = Some(1);
                opts.single_branch = Some(true);
                git_args.extend(["--depth", "1", "--single-branch"]);
            }
            _ => {}
        }
        git_args.extend([&url, "."]);
        assert!(git_in(control.path(), &git_args));
        let result = clone_repo(opts, &mut NoProgress, &NeverCancel, None).unwrap();
        let git_dir = result.repo.git_dir();
        let tags = git_out(
            dest.path(),
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/tags/",
            ],
        )
        .unwrap();
        assert_eq!(
            tags,
            git_out(
                control.path(),
                &[
                    "for-each-ref",
                    "--format=%(refname) %(objectname)",
                    "refs/tags/"
                ]
            )
            .unwrap(),
            "{mode}"
        );
        let packed = std::fs::read_to_string(git_dir.join("packed-refs")).unwrap_or_default();
        for line in tags.lines() {
            let (name, oid) = line.split_once(' ').unwrap();
            assert!(!git_dir.join(name).exists(), "{mode}: {name}");
            assert!(
                packed.contains(&format!("{oid} {name}\n")),
                "{mode}: {packed}"
            );
            if name != "refs/tags/light" {
                let peel = git_out(dest.path(), &["rev-parse", &format!("{name}^{{}}")]).unwrap();
                assert!(
                    packed.contains(&format!("{oid} {name}\n^{peel}\n")),
                    "{mode}: {packed}"
                );
            }
        }
        assert!(std::fs::read_to_string(git_dir.join("HEAD"))
            .unwrap()
            .starts_with("ref: "));
        assert!(git_in(dest.path(), &["fsck", "--strict"]));
    }
    let empty = tempfile::tempdir().unwrap();
    assert!(git_in(empty.path(), &["init", "-q", "-b", "main"]));
    let dest = tempfile::tempdir().unwrap();
    let result = clone_repo(
        CloneOptions::new(empty.path().to_string_lossy(), dest.path().to_path_buf()),
        &mut NoProgress,
        &NeverCancel,
        None,
    )
    .unwrap();
    assert!(!result.repo.git_dir().join("packed-refs").exists());
    assert!(for_each_ref(dest.path()).is_empty());
    assert!(git_in(dest.path(), &["fsck", "--strict"]));
}
