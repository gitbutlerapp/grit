//! s1: `rev-list --objects v1.0.0` on upstream git.git (bare).
//!
//! Fixture path: `GRIT_GIT_GIT_BARE` or `/tmp/git.git`. When missing, benches skip.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use grit_lib::repo::Repository;
use grit_lib::rev_list::{rev_list, RevListOptions};

pub struct S1Fixture {
    pub repo: Repository,
}

impl S1Fixture {
    pub fn global() -> &'static Self {
        static FIXTURE: OnceLock<Option<S1Fixture>> = OnceLock::new();
        FIXTURE
            .get_or_init(|| {
                let bare = git_git_bare();
                if !bare.is_dir() {
                    eprintln!("SKIP s1: bare repo not found at {}", bare.display());
                    return None;
                }
                let repo = Repository::open(&bare, None).ok()?;
                Some(S1Fixture { repo })
            })
            .as_ref()
            .expect("s1 fixture requested but git.git bare repo is unavailable")
    }
}

#[must_use]
pub fn git_git_bare() -> PathBuf {
    std::env::var("GRIT_GIT_GIT_BARE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp/git.git"))
}

#[must_use]
pub fn fixture_available() -> bool {
    git_git_bare().is_dir()
}

pub fn run_rev_list_objects_v1(repo: &Repository) -> usize {
    let opts = RevListOptions {
        objects: true,
        ..Default::default()
    };
    let result = rev_list(repo, &[String::from("v1.0.0")], &[], &opts).expect("rev-list s1");
    result.objects.len()
}

pub fn open_repo(path: &Path) -> Repository {
    Repository::open(path, None).expect("open bare git.git")
}
