//! Replacing a gitlink (submodule) with a regular file, then staging and committing it,
//! records a blob in the new commit, whether the repository was opened through an absolute
//! path, a relative path, or `RepositoryBuilder` with a relative `OdbBuilder` path.
//!
//! This binary holds a single test because it changes the process working directory.

use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::objects::odb::OdbBuilder;
use grit_lib::porcelain::add::{stage, StageMode, StageOptions};
use grit_lib::porcelain::commit::{commit_with_scope, CommitRequest, CommitScope};
use grit_lib::progress::NullProgress;
use grit_lib::repo::{Repository, RepositoryBuilder};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
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
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A repository whose `HEAD` records `sub` as a gitlink, with `sub` then replaced by a file
/// in the work tree.
fn gitlink_replaced_by_file(root: &Path) {
    fs::create_dir_all(root.join("sub")).expect("sub dir");
    git(root, &["init", "-q", "-b", "main"]);
    let sub = root.join("sub");
    git(&sub, &["init", "-q", "-b", "main"]);
    fs::write(sub.join("inner.txt"), "inner\n").expect("inner");
    git(&sub, &["add", "inner.txt"]);
    git(&sub, &["commit", "-q", "-m", "inner"]);
    let sub_oid = git(&sub, &["rev-parse", "HEAD"]);
    git(
        root,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{sub_oid},sub"),
        ],
    );
    git(root, &["commit", "-q", "-m", "gitlink"]);

    fs::remove_dir_all(&sub).expect("remove submodule");
    fs::write(&sub, "plain file\n").expect("write file");
}

fn stage_and_commit(repo: &Repository) {
    stage(
        repo,
        &StageOptions {
            pathspecs: vec!["sub".to_owned()],
            pathspec_sources: vec!["sub".to_owned()],
            mode: StageMode::All,
            ..StageOptions::default()
        },
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("stage sub");
    let ident = "T <t@example.com> 1700000000 +0000".to_owned();
    commit_with_scope(
        repo,
        &{
            let mut o = CommitRequest::default();
            o.message = "replace with file".to_owned();
            o.author = Some(ident.parse().expect("signature"));
            o.committer = Some(ident.parse().expect("signature"));
            o
        },
        CommitScope::Staged,
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("the gitlink-to-file typechange must be committable");
}

fn assert_committed_as_blob(root: &Path) {
    let entry = git(root, &["ls-tree", "HEAD", "sub"]);
    assert!(entry.starts_with("100644 blob "), "HEAD:sub is {entry:?}");
    assert_eq!(git(root, &["cat-file", "-p", "HEAD:sub"]), "plain file");
    assert_eq!(
        git(root, &["ls-tree", "HEAD~1", "sub"]).split(' ').nth(1),
        Some("commit")
    );
    git(root, &["fsck", "--no-progress"]);
}

#[test]
fn gitlink_to_file_typechange_stages_and_commits_through_every_open_path() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().canonicalize().expect("canonical tempdir");

    let absolute = base.join("absolute");
    gitlink_replaced_by_file(&absolute);
    let repo = Repository::open(
        &absolute.join(".git"),
        &RepositoryOptions::empty().with_work_tree(&absolute),
    )
    .expect("open absolute");
    stage_and_commit(&repo);
    assert_committed_as_blob(&absolute);

    std::env::set_current_dir(&base).expect("chdir");
    let options = RepositoryOptions::with_environment(Environment::capture_process());

    let relative = base.join("relative");
    gitlink_replaced_by_file(&relative);
    let repo = Repository::open(
        Path::new("relative/.git"),
        &options.clone().with_work_tree("relative"),
    )
    .expect("open relative");
    stage_and_commit(&repo);
    assert_committed_as_blob(&relative);

    let built = base.join("built");
    gitlink_replaced_by_file(&built);
    let repo = RepositoryBuilder::new(options)
        .odb_builder(OdbBuilder::files("built/.git/objects"))
        .open(Path::new("built/.git"), Some(Path::new("built")))
        .expect("open through RepositoryBuilder with relative paths");
    stage_and_commit(&repo);
    assert_committed_as_blob(&built);
}
