//! s1: `rev-list --objects v1.0.0` on upstream git.git (bare).
//!
//! Fixture path: `GRIT_GIT_GIT_BARE` or `/tmp/git.git`. When missing, benches skip.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use grit_lib::progress::{CancelToken, NullProgress};

use std::path::PathBuf;
use std::sync::OnceLock;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_list::RevListOptions;

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
                let repo = Repository::open(&bare, &RepositoryOptions::empty()).ok()?;
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
    let opts = RevListOptions::default().with_objects(true);
    let result = repo
        .revwalk()
        .rev_list(
            &[String::from("v1.0.0")],
            &[],
            &opts,
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("rev-list s1");
    result.objects.len()
}
