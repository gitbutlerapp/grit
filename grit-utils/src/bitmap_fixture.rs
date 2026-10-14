//! Cached `git.git` repository prepared for reachability bitmap benchmarks.

pub use crate::real_repo_fixture::{
    assert_tag_pinned_fixture_refs, bitmap_isolated_env,
    ensure_git_git_bitmaps as ensure_bitmaps_git_repo, list_branch_head_refs, DEFAULT_GIT_GIT_TAG,
};

/// Path to the bitmap-prepared bare `git.git` clone.
pub fn bitmap_repo_cache_path() -> std::path::PathBuf {
    crate::real_repo_fixture::git_git_bitmaps_cache_path()
}
