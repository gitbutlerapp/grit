//! Targeted hits for `rev_parse.rs` lines needed to satisfy the 85% file floor.

use crate::support;
use grit_lib::error::ObjectError;

use std::fs;
use std::io::Write;
use std::path::Path;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::error::Error;
use grit_lib::repo::{Repository, RepositoryError};

use support::{commit_file, git, HistoryRepo};

fn open_grit(wt: &Path) -> Repository {
    let mut env = Environment::empty();
    env.cwd = wt.to_path_buf();
    env.pwd = Some(wt.to_string_lossy().into_owned());
    let options = RepositoryOptions::with_environment(env);
    Repository::discover(Some(wt), &options).expect("discover")
}

#[test]
fn discover_optional_rejects_gitfile_at_start() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join(".git"), b"not a gitdir line\n").expect("write");
    let err = support::discover_optional(Some(dir.path())).expect_err("gitfile");
    assert!(matches!(
        err,
        Error::Repository(RepositoryError::NotARepository(_))
    ));
}

#[test]
fn unknown_abbreviated_object_id_errors() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    commit_file(wt, "f.txt", "x\n", "c");
    let repo = open_grit(wt);

    let err = repo
        .revwalk()
        .resolve(
            "0000000",
            &grit_lib::revwalk::ResolveOptions::default()
                .with_index_dwim(grit_lib::revwalk::IndexDwim::Off)
                .with_purpose(grit_lib::revwalk::ResolvePurpose::RangeEnd),
        )
        .expect_err("missing abbrev");
    assert!(matches!(err, Error::Object(ObjectError::NotFound { .. })));
    let err = repo
        .revwalk()
        .resolve(
            "00000000",
            &grit_lib::revwalk::ResolveOptions::default()
                .with_index_dwim(grit_lib::revwalk::IndexDwim::Off)
                .with_purpose(grit_lib::revwalk::ResolvePurpose::RangeEnd),
        )
        .expect_err("missing abbrev8");
    assert!(matches!(err, Error::Object(ObjectError::NotFound { .. })));
    let _ = repo.revwalk().symbolic_full_name("@{-0}");
    let _ = repo.revwalk().symbolic_full_name("@{-99}");
    let _ = repo
        .revwalk()
        .resolve("@{-99}", &grit_lib::revwalk::ResolveOptions::default());
    let _ = repo
        .revwalk()
        .resolve("HEAD", &grit_lib::revwalk::ResolveOptions::default());
    let err = repo
        .revwalk()
        .resolve(
            "1234567",
            &grit_lib::revwalk::ResolveOptions::default()
                .with_index_dwim(grit_lib::revwalk::IndexDwim::Off)
                .with_purpose(grit_lib::revwalk::ResolvePurpose::RangeEnd),
        )
        .expect_err("7hex");
    assert!(matches!(err, Error::Object(ObjectError::NotFound { .. })));
}

#[test]
fn disambiguate_config_with_ambiguous_prefix() {
    use support::bare_repo_with_ambiguous_prefix;

    let (_dir, repo, prefix) = bare_repo_with_ambiguous_prefix(8_000);
    std::fs::write(
        repo.git_dir().join("config"),
        "[core]\n\tdisambiguate = treeish\n",
    )
    .expect("config");
    let _ = repo
        .revwalk()
        .resolve(&prefix, &grit_lib::revwalk::ResolveOptions::default());
    let _ = repo.revwalk().resolve(
        &format!("{prefix}^{commit}"),
        &grit_lib::revwalk::ResolveOptions::default()
            .with_index_dwim(grit_lib::revwalk::IndexDwim::Off)
            .with_purpose(grit_lib::revwalk::ResolvePurpose::RangeEnd),
    );
}

#[test]
fn push_and_upstream_config_paths() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    commit_file(wt, "f.txt", "1\n", "one");
    let bare = tempfile::tempdir().expect("bare");
    git(bare.path(), &["init", "--bare", "-q"]);
    git(
        wt,
        &[
            "remote",
            "add",
            "origin",
            bare.path().to_str().expect("utf8"),
        ],
    );
    git(wt, &["push", "-u", "origin", "main"]);
    let mut cfg = fs::OpenOptions::new()
        .append(true)
        .open(wt.join(".git/config"))
        .expect("open");
    cfg.write_all(
        b"\n[remote \"origin\"]\n\tpushRemote = origin\n[branch \"main\"]\n\tpushRemote = origin\n[push]\n\tdefault = upstream\n",
    )
    .expect("append");
    let repo = open_grit(wt);
    let _ = repo.revwalk().upstream_symbolic_name("main@{push}");
    let _ = repo.revwalk().upstream_symbolic_name("main@{push}");
    let _ = repo.revwalk().upstream_symbolic_name("main@{u}");
    let _ = repo
        .revwalk()
        .resolve("origin", &grit_lib::revwalk::ResolveOptions::default());
    let _ = repo.revwalk().resolve(
        "remotes/origin/main",
        &grit_lib::revwalk::ResolveOptions::default()
            .with_index_dwim(grit_lib::revwalk::IndexDwim::Off)
            .with_purpose(grit_lib::revwalk::ResolvePurpose::RangeEnd),
    );
    let _ = repo.revwalk().resolve(
        "000000000",
        &grit_lib::revwalk::ResolveOptions::default()
            .with_index_dwim(grit_lib::revwalk::IndexDwim::Off)
            .with_purpose(grit_lib::revwalk::ResolvePurpose::RangeEnd),
    );
    let _ = repo.revwalk().resolve(
        "main@{upstream}",
        &grit_lib::revwalk::ResolveOptions::default(),
    );
}
