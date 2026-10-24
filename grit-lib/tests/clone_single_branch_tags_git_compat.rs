//! `CloneOptions` single-branch, tag mode, and remote name parity with `git clone`.

use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::progress::NeverCancel;
use grit_lib::remote::clone::{
    clone as clone_repo, clone_effective_single_branch, CloneError, CloneOptions, CloneTags,
};
use grit_lib::remote::fetch::NoProgress;
use grit_test_support::git;

fn git_config_fetch_all(work_tree: &Path) -> Vec<String> {
    let out = Command::new("git")
        .current_dir(work_tree)
        .args(["config", "--get-all", "remote.origin.fetch"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git config fetch");
    assert!(
        out.status.success(),
        "git config --get-all remote.origin.fetch: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

fn remote_tracking_ref_names(git_dir: &Path) -> Vec<String> {
    let wt = git_dir.parent().expect("work tree");
    let out = Command::new("git")
        .current_dir(wt)
        .args(["for-each-ref", "--format=%(refname:short)", "refs/remotes/"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("for-each-ref");
    assert!(out.status.success());
    let mut names: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    names.sort();
    names
}

fn build_two_branch_bare(bare: &Path) {
    let wt = bare.with_extension("wt");
    std::fs::create_dir_all(&wt).unwrap();
    git(&wt, &["init", "-q", "-b", "main", "."]);
    std::fs::write(wt.join("f"), b"a\n").unwrap();
    git(&wt, &["add", "f"]);
    git(
        &wt,
        &[
            "-c",
            "user.email=u@e.com",
            "-c",
            "user.name=U",
            "commit",
            "-qm",
            "main",
        ],
    );
    git(&wt, &["switch", "-qc", "other"]);
    std::fs::write(wt.join("g"), b"b\n").unwrap();
    git(&wt, &["add", "g"]);
    git(
        &wt,
        &[
            "-c",
            "user.email=u@e.com",
            "-c",
            "user.name=U",
            "commit",
            "-qm",
            "other",
        ],
    );
    git(&wt, &["switch", "-q", "main"]);
    git(&wt, &["tag", "v1"]);
    git(&wt, &["clone", "-q", "--bare", ".", bare.to_str().unwrap()]);
}

fn file_url(bare: &Path) -> String {
    format!(
        "file://{}",
        bare.canonicalize().expect("canonical").display()
    )
}

#[derive(Clone, Copy, Debug)]
struct CloneCase {
    depth: Option<u32>,
    branch: Option<&'static str>,
    single_branch: Option<bool>,
}

fn git_clone_with(case: CloneCase, url: &str, dest: &Path) {
    let mut cmd = Command::new("git");
    cmd.arg("clone").arg("-q");
    let depth_str;
    if let Some(d) = case.depth {
        depth_str = d.to_string();
        cmd.arg("--depth").arg(&depth_str);
    }
    if case.single_branch == Some(true) {
        cmd.arg("--single-branch");
    }
    if case.single_branch == Some(false) {
        cmd.arg("--no-single-branch");
    }
    if let Some(branch) = case.branch {
        cmd.arg("-b").arg(branch);
    }
    cmd.arg(url).arg(dest);
    let status = cmd
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git clone");
    assert!(status.success(), "git clone failed for {case:?}");
}

fn grit_clone_with(case: CloneCase, url: String, dest: PathBuf) {
    let opts = CloneOptions {
        depth: case.depth,
        branch: case.branch.map(str::to_owned),
        single_branch: case.single_branch,
        ..CloneOptions::new(url, dest)
    };
    clone_repo(opts, &mut NoProgress, &NeverCancel, None).expect("grit clone");
}

#[test]
fn clone_single_branch_refspec_matches_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("remote.git");
    build_two_branch_bare(&bare);
    let url = file_url(&bare);

    let cases = [
        CloneCase {
            depth: None,
            branch: None,
            single_branch: None,
        },
        CloneCase {
            depth: Some(1),
            branch: None,
            single_branch: None,
        },
        CloneCase {
            depth: None,
            branch: Some("other"),
            single_branch: None,
        },
        CloneCase {
            depth: Some(1),
            branch: Some("other"),
            single_branch: None,
        },
    ];

    for (i, case) in cases.into_iter().enumerate() {
        let git_dest = tmp.path().join(format!("git-{i}"));
        let grit_dest = tmp.path().join(format!("grit-{i}"));
        git_clone_with(case, &url, &git_dest);
        grit_clone_with(case, url.clone(), grit_dest.clone());

        assert_eq!(
            git_config_fetch_all(&git_dest),
            git_config_fetch_all(&grit_dest),
            "remote.origin.fetch must match git for case {case:?}"
        );
        assert_eq!(
            remote_tracking_ref_names(&git_dest.join(".git")),
            remote_tracking_ref_names(&grit_dest.join(".git")),
            "remote-tracking refs must match git for case {case:?}"
        );

        // Follow-up fetch should update the same refs as in a git clone.
        git(&git_dest, &["fetch", "-q", "origin"]);
        git(&grit_dest, &["fetch", "-q", "origin"]);
        assert_eq!(
            remote_tracking_ref_names(&git_dest.join(".git")),
            remote_tracking_ref_names(&grit_dest.join(".git")),
            "after fetch, remote-tracking refs must still match for case {case:?}"
        );
    }
}

#[test]
fn clone_no_tags_sets_tagopt_and_fetches_no_tags() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("remote.git");
    build_two_branch_bare(&bare);
    let url = file_url(&bare);

    let git_dest = tmp.path().join("git");
    let grit_dest = tmp.path().join("grit");

    let status = Command::new("git")
        .args(["clone", "-q", "--no-tags", &url, git_dest.to_str().unwrap()])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git clone --no-tags");
    assert!(status.success());

    clone_repo(
        CloneOptions {
            tags: Some(CloneTags::None),
            ..CloneOptions::new(url, grit_dest.clone())
        },
        &mut NoProgress,
        &NeverCancel,
        None,
    )
    .expect("grit clone --no-tags");

    for dest in [&git_dest, &grit_dest] {
        let tag_opt = Command::new("git")
            .current_dir(dest)
            .args(["config", "--get", "remote.origin.tagOpt"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("tagOpt");
        assert!(tag_opt.status.success());
        assert_eq!(String::from_utf8_lossy(&tag_opt.stdout).trim(), "--no-tags");
        assert!(
            git(dest, &["tag", "-l"]).trim().is_empty(),
            "no tags fetched"
        );
    }

    git(&git_dest, &["fetch", "-q"]);
    git(&grit_dest, &["fetch", "-q"]);
    assert!(git(&git_dest, &["tag", "-l"]).trim().is_empty());
    assert!(git(&grit_dest, &["tag", "-l"]).trim().is_empty());
}

#[test]
fn clone_origin_name_used_for_tracking_refs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("remote.git");
    build_two_branch_bare(&bare);
    let url = bare.to_string_lossy().into_owned();

    let dest = tmp.path().join("upstream-clone");
    clone_repo(
        CloneOptions {
            remote_name: "upstream".to_owned(),
            ..CloneOptions::new(url, dest.clone())
        },
        &mut NoProgress,
        &NeverCancel,
        None,
    )
    .expect("clone with custom remote name");

    let tracking = remote_tracking_ref_names(&dest.join(".git"));
    assert!(
        tracking.iter().any(|r| r.starts_with("upstream/")),
        "expected upstream/* tracking refs, got {tracking:?}"
    );
    assert!(
        !tracking.iter().any(|r| r.starts_with("origin/")),
        "must not use origin when -o upstream: {tracking:?}"
    );

    let fetch_lines = Command::new("git")
        .current_dir(&dest)
        .args(["config", "--get-all", "remote.upstream.fetch"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("fetch config");
    assert!(fetch_lines.status.success());
    let fetch = String::from_utf8_lossy(&fetch_lines.stdout);
    assert!(fetch.contains("refs/remotes/upstream/"));
}

#[test]
fn clone_invalid_remote_on_nonempty_destination_preserves_files() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("remote.git");
    build_two_branch_bare(&bare);

    let dest = tmp.path().join("dest");
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::write(dest.join("keep.txt"), b"stay").unwrap();

    let err = clone_repo(
        CloneOptions {
            remote_name: "bad name".to_owned(),
            ..CloneOptions::new(bare.to_string_lossy(), dest.clone())
        },
        &mut NoProgress,
        &NeverCancel,
        None,
    )
    .expect_err("nonempty destination must fail before remote-name validation matters");

    assert!(
        matches!(err, CloneError::DestNotEmpty(_)),
        "expected DestNotEmpty, got {err:?}"
    );
    assert!(
        dest.join("keep.txt").is_file(),
        "destination must be preserved"
    );
}

#[test]
fn clone_follow_up_fetch_sees_new_remote_branch() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("remote.git");
    build_two_branch_bare(&bare);
    let url = file_url(&bare);
    let wt = bare.with_extension("wt");

    let git_dest = tmp.path().join("git");
    let grit_dest = tmp.path().join("grit");
    git_clone_with(
        CloneCase {
            depth: None,
            branch: None,
            single_branch: None,
        },
        &url,
        &git_dest,
    );
    grit_clone_with(
        CloneCase {
            depth: None,
            branch: None,
            single_branch: None,
        },
        url.clone(),
        grit_dest.clone(),
    );

    git(&wt, &["switch", "-qc", "added"]);
    std::fs::write(wt.join("h.txt"), b"h\n").unwrap();
    git(&wt, &["add", "h.txt"]);
    git(
        &wt,
        &[
            "-c",
            "user.email=u@e.com",
            "-c",
            "user.name=U",
            "commit",
            "-qm",
            "added",
        ],
    );
    git(
        &wt,
        &["push", "-q", bare.to_str().unwrap(), "refs/heads/added"],
    );

    git(&git_dest, &["fetch", "-q", "origin"]);
    git(&grit_dest, &["fetch", "-q", "origin"]);
    assert_eq!(
        remote_tracking_ref_names(&git_dest.join(".git")),
        remote_tracking_ref_names(&grit_dest.join(".git")),
        "follow-up fetch must match git after a new branch on the remote"
    );
    assert!(
        remote_tracking_ref_names(&grit_dest.join(".git"))
            .iter()
            .any(|r| r == "origin/added"),
        "expected origin/added after fetch"
    );
}

#[test]
fn clone_rejects_invalid_remote_name() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("remote.git");
    build_two_branch_bare(&bare);

    let err = clone_repo(
        CloneOptions {
            remote_name: "bad name".to_owned(),
            ..CloneOptions::new(bare.to_string_lossy(), tmp.path().join("dest"))
        },
        &mut NoProgress,
        &NeverCancel,
        None,
    )
    .expect_err("invalid remote name");

    match err {
        CloneError::Remote(grit_lib::remote::RemoteError::InvalidRemoteName(name))
            if name == "bad name" => {}
        other => panic!("expected InvalidRemoteName, got {other:?}"),
    }
}

#[test]
fn clone_effective_single_branch_matches_git_defaults() {
    let full = CloneOptions::new("x", PathBuf::from("d"));
    assert!(!clone_effective_single_branch(&full));

    let shallow = CloneOptions {
        depth: Some(1),
        ..CloneOptions::new("x", PathBuf::from("d"))
    };
    assert!(clone_effective_single_branch(&shallow));

    let shallow_all = CloneOptions {
        depth: Some(1),
        single_branch: Some(false),
        ..CloneOptions::new("x", PathBuf::from("d"))
    };
    assert!(!clone_effective_single_branch(&shallow_all));
}
