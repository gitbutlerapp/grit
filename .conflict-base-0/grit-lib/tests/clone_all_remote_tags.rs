//! Clone must fetch all remote tags, including those on commits unreachable from
//! any fetched branch (GitHub issue #954).

use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::Environment;
use grit_lib::refs;
use grit_lib::remote::clone::{clone as clone_repo, CloneOptions};
use grit_lib::remote::fetch::NoProgress;

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

fn git_tags(dir: &Path) -> Vec<String> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["tag"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git tag");
    assert!(out.status.success(), "git tag failed");
    let mut tags: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    tags.sort();
    tags
}

fn build_upstream_with_orphan_tags(upstream: &Path) {
    assert!(git_in(upstream, &["init", "-q", "-b", "main", "."]));
    std::fs::write(upstream.join("a.txt"), b"a\n").unwrap();
    git_in(upstream, &["add", "-A"]);
    git_in(
        upstream,
        &[
            "-c",
            "user.email=r@e.com",
            "-c",
            "user.name=R",
            "commit",
            "-qm",
            "one",
        ],
    );
    assert!(git_in(upstream, &["switch", "-qc", "side"]));
    std::fs::write(upstream.join("b.txt"), b"b\n").unwrap();
    git_in(upstream, &["add", "-A"]);
    git_in(
        upstream,
        &[
            "-c",
            "user.email=r@e.com",
            "-c",
            "user.name=R",
            "commit",
            "-qm",
            "side",
        ],
    );
    assert!(git_in(upstream, &["tag", "side-tag"]));
    assert!(git_in(
        upstream,
        &[
            "-c",
            "user.email=r@e.com",
            "-c",
            "user.name=R",
            "tag",
            "-a",
            "side-ann",
            "-m",
            "x",
        ]
    ));
    assert!(git_in(upstream, &["switch", "-q", "main"]));
    assert!(git_in(upstream, &["branch", "-D", "side"]));
}

#[test]
fn clone_fetches_tags_on_unreachable_commits() {
    let upstream = tempfile::tempdir().expect("upstream");
    build_upstream_with_orphan_tags(upstream.path());

    let git_clone = tempfile::tempdir().expect("git clone");
    assert!(git_in(
        git_clone.path(),
        &["clone", "-q", upstream.path().to_str().unwrap(), "."]
    ));
    let expected = git_tags(git_clone.path());
    assert_eq!(
        expected,
        vec!["side-ann".to_owned(), "side-tag".to_owned()],
        "fixture must expose both tags via git clone"
    );

    let grit_dest = tempfile::tempdir().expect("grit clone");
    clone_repo(
        CloneOptions {
            url: upstream.path().to_string_lossy().into_owned(),
            dest: grit_dest.path().to_path_buf(),
            environment: Environment::capture_process(),
            remote_name: grit_lib::remote::DEFAULT_REMOTE.to_owned(),
            initial_branch: "main".to_owned(),
            thread_policy: grit_lib::parallel::ThreadPolicy::default(),
        },
        &mut NoProgress,
        None,
    )
    .expect("grit clone");

    let git_dir = grit_dest.path().join(".git");
    refs::resolve_ref(&git_dir, "refs/tags/side-tag").expect("lightweight side-tag");
    refs::resolve_ref(&git_dir, "refs/tags/side-ann").expect("annotated side-ann");

    assert_eq!(git_tags(grit_dest.path()), expected);

    assert!(git_in(grit_dest.path(), &["fsck", "--strict"]));
}

#[test]
fn clone_file_url_fetches_unreachable_tags() {
    let upstream = tempfile::tempdir().expect("upstream");
    build_upstream_with_orphan_tags(upstream.path());

    let grit_dest = tempfile::tempdir().expect("grit clone");
    let url = format!(
        "file://{}",
        upstream.path().canonicalize().expect("canonical").display()
    );
    clone_repo(
        CloneOptions {
            url,
            dest: grit_dest.path().to_path_buf(),
            environment: Environment::capture_process(),
            remote_name: grit_lib::remote::DEFAULT_REMOTE.to_owned(),
            initial_branch: "main".to_owned(),
            thread_policy: grit_lib::parallel::ThreadPolicy::default(),
        },
        &mut NoProgress,
        None,
    )
    .expect("grit clone file://");

    let git_dir = grit_dest.path().join(".git");
    refs::resolve_ref(&git_dir, "refs/tags/side-tag").expect("side-tag via file://");
    refs::resolve_ref(&git_dir, "refs/tags/side-ann").expect("side-ann via file://");
}
