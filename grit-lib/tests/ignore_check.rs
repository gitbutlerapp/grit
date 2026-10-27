//! `Repository::ignore_check` vs `git check-ignore -v --non-matching`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "ignore_t0008_common/mod.rs"]
mod t0008;

use grit_lib::index::Index;
use grit_lib::repo::Repository;
use grit_lib::worktree::ignore::{IgnoreCheck, IgnoreSource};
use t0008::{assert_match_eq, git_check_ignore_verbose_batch, t0008_corpus, T0008Repo};

fn grit_ignore_check(repo: &Repository, index: &Index, paths: &[String]) -> Vec<IgnoreCheck> {
    repo.ignore_check(paths, index).expect("ignore_check")
}

#[test]
fn normalize_ignore_paths_preserves_boundaries_and_directory_hints() {
    use grit_lib::worktree::ignore::normalize_repo_relative;

    let fx = T0008Repo::setup();
    let repo = fx.open_grit();
    let root = repo.work_tree().expect("work tree");
    let cwd = root.join("a");
    for (input, expected) in [
        ("b/../missing.txt", "a/missing.txt"),
        ("../new-dir/", "new-dir/"),
        ("./b/./four", "a/b/four"),
        ("../", ""),
        ("../café/new.txt", "café/new.txt"),
        (":literal", ":literal"),
    ] {
        assert_eq!(
            normalize_repo_relative(&repo, &cwd, input).expect("normalize relative path"),
            expected,
            "{input}"
        );
    }
    let absolute = root.join("missing/file");
    assert_eq!(
        normalize_repo_relative(&repo, &cwd, absolute.to_str().expect("path"))
            .expect("normalize absolute path"),
        "missing/file"
    );
    for input in ["../../outside", "../a/../../outside"] {
        assert!(matches!(
            normalize_repo_relative(&repo, &cwd, input),
            Err(grit_lib::error::Error::PathError(_))
        ));
    }
    let sibling = root.with_file_name(format!(
        "{}-neighbor",
        root.file_name().expect("root name").to_string_lossy()
    ));
    assert!(matches!(
        normalize_repo_relative(&repo, &cwd, sibling.join("file").to_str().expect("path")),
        Err(grit_lib::error::Error::PathError(_))
    ));
}

#[cfg(unix)]
#[test]
fn normalized_ignore_query_keeps_symlink_spelling_and_matches_git() {
    use grit_lib::worktree::ignore::normalize_repo_relative;

    let fx = T0008Repo::setup();
    let repo = fx.open_grit();
    let root = repo.work_tree().expect("work tree");
    let outside = tempfile::tempdir().expect("outside directory");
    std::os::unix::fs::symlink(outside.path(), root.join("ignored-link"))
        .expect("directory symlink");
    let path =
        normalize_repo_relative(&repo, root, "ignored-link").expect("normalize symlink spelling");
    assert_eq!(path, "ignored-link");
    assert_eq!(
        normalize_repo_relative(&repo, root, "ignored-link/missing.txt")
            .expect("normalize missing descendant lexically"),
        "ignored-link/missing.txt"
    );
    let index = repo.index_handle().load().expect("index");
    let results = repo.ignore_check(&[path.clone()], &index).expect("check");
    let git = git_check_ignore_verbose_batch(&fx.home, root, &[&path]);
    assert_match_eq(&path, &git[&path], results[0].ignored, &results[0].matched);
}

#[cfg(target_os = "macos")]
#[test]
fn normalize_ignore_query_accepts_macos_tmp_alias() {
    use grit_lib::worktree::ignore::normalize_repo_relative;

    let dir = tempfile::tempdir_in("/tmp").expect("temporary directory");
    let repo =
        grit_lib::repo::Repository::init(dir.path(), &grit_lib::repo::InitOptions::default())
            .expect("init");
    let canonical = dir.path().canonicalize().expect("physical path");
    let alias = std::path::Path::new("/tmp").join(dir.path().file_name().expect("name"));
    for cwd in [&canonical, &alias] {
        assert_eq!(
            normalize_repo_relative(&repo, cwd, "missing/file/").expect("normalize alias"),
            "missing/file/"
        );
    }
}

#[test]
fn matches_git_check_ignore_verbose() {
    let fx = T0008Repo::setup();
    let repo = fx.open_grit();
    let index = repo.index_handle().load().expect("index");
    let corpus = t0008_corpus();
    let paths: Vec<String> = corpus.iter().map(|(p, _)| (*p).to_owned()).collect();
    let path_refs: Vec<&str> = paths.iter().map(String::as_str).collect();

    let git_map = git_check_ignore_verbose_batch(&fx.home, &fx.root, &path_refs);
    let grit_results = grit_ignore_check(&repo, &index, &paths);
    assert_eq!(grit_results.len(), paths.len());

    for (path, _) in corpus {
        let git = git_map
            .get(path)
            .unwrap_or_else(|| panic!("git missing {path}"));
        let grit = grit_results
            .iter()
            .find(|c| c.path == path)
            .unwrap_or_else(|| panic!("grit missing {path}"));
        assert_match_eq(path, git, grit.ignored, &grit.matched);
    }
}

#[test]
fn tracked_paths_never_ignored() {
    let fx = T0008Repo::setup();
    let repo = fx.open_grit();
    let index = repo.index_handle().load().expect("index");
    for path in ["ignored-but-in-index", "a/ignored-but-in-index"] {
        let results = grit_ignore_check(&repo, &index, &[path.to_owned()]);
        assert_eq!(results.len(), 1);
        assert!(!results[0].ignored, "{path} is tracked");
        assert!(results[0].matched.is_none());
    }
}

#[test]
fn trailing_slash_on_existing_file_matches_git() {
    use std::fs;

    use grit_lib::config::environment::RepositoryOptions;
    use grit_test_support::unique_tmp;

    let home = unique_tmp("ignore", "file-slash-home");
    let root = unique_tmp("ignore", "file-slash");
    fs::create_dir_all(&home).expect("home");
    assert!(
        t0008::hermetic_git(&home, &root, &["init"])
            .status
            .success(),
        "git init"
    );
    assert!(
        t0008::hermetic_git(&home, &root, &["config", "user.email", "ignore@test"])
            .status
            .success()
            && t0008::hermetic_git(&home, &root, &["config", "user.name", "Ignore Test"])
                .status
                .success(),
        "git identity"
    );
    fs::write(root.join(".gitignore"), "*.txt\n").expect("gitignore");
    fs::write(root.join("untracked.txt"), b"x").expect("untracked");
    fs::write(root.join("tracked.txt"), b"y").expect("tracked");
    assert!(
        t0008::hermetic_git(&home, &root, &["add", "-f", "tracked.txt"])
            .status
            .success(),
        "git add -f"
    );

    let repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(&root),
    )
    .expect("open");
    let index = repo.index_handle().load().expect("index");

    for path in ["untracked.txt/", "tracked.txt/"] {
        let git_map = git_check_ignore_verbose_batch(&home, &root, &[path]);
        let git = git_map
            .get(path)
            .unwrap_or_else(|| panic!("git missing {path}"));
        let results = grit_ignore_check(&repo, &index, &[path.to_owned()]);
        assert_eq!(results.len(), 1, "one result for {path}");
        assert_match_eq(path, git, results[0].ignored, &results[0].matched);
    }
}

#[test]
fn trailing_slash_directory_hint_matches_git() {
    use std::fs;

    use grit_lib::config::environment::RepositoryOptions;
    use grit_test_support::unique_tmp;

    let home = unique_tmp("ignore", "slash-home");
    let root = unique_tmp("ignore", "slash-dir");
    fs::create_dir_all(&home).expect("home");
    assert!(
        t0008::hermetic_git(&home, &root, &["init"])
            .status
            .success(),
        "git init"
    );
    fs::write(root.join(".gitignore"), "missing-dir/\n").expect("gitignore");

    let repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(&root),
    )
    .expect("open");
    let index = repo.index_handle().load().expect("index");
    let path = "missing-dir/";
    let git_map = git_check_ignore_verbose_batch(&home, &root, &[path]);
    let git = git_map.get(path).expect("git result for missing-dir/");
    assert!(git.ignored, "git should treat missing-dir/ as ignored");

    let rel =
        grit_lib::worktree::ignore::normalize_repo_relative(&repo, &root, path).expect("normalize");
    assert_eq!(rel, path, "trailing slash must be preserved");

    let results = grit_ignore_check(&repo, &index, &[path.to_owned()]);
    assert_eq!(results.len(), 1);
    assert_match_eq(path, git, results[0].ignored, &results[0].matched);
}

#[test]
fn ignore_source_is_typed() {
    let fx = T0008Repo::setup();
    let repo = fx.open_grit();
    let index = repo.index_handle().load().expect("index");
    let results = grit_ignore_check(
        &repo,
        &index,
        &[
            "one".into(),
            "per-repo".into(),
            "globalone".into(),
            "a/two".into(),
        ],
    );
    let one = results.iter().find(|c| c.path == "one").unwrap();
    assert!(matches!(
        one.matched.as_ref().map(|m| &m.source),
        Some(IgnoreSource::PerDirectory(p)) if p == ".gitignore"
    ));
    let per_repo = results.iter().find(|c| c.path == "per-repo").unwrap();
    assert!(matches!(
        per_repo.matched.as_ref().map(|m| &m.source),
        Some(IgnoreSource::InfoExclude)
    ));
    let global = results.iter().find(|c| c.path == "globalone").unwrap();
    assert!(matches!(
        global.matched.as_ref().map(|m| &m.source),
        Some(IgnoreSource::CoreExcludesFile(_))
    ));
    let nested = results.iter().find(|c| c.path == "a/two").unwrap();
    assert!(matches!(
        nested.matched.as_ref().map(|m| &m.source),
        Some(IgnoreSource::PerDirectory(p)) if p == "a/.gitignore"
    ));
}
