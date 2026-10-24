//! `Repository::paths_match_pathspecs` evaluates `:(attr:...)` magic against the repository's
//! `.gitattributes`, cross-checked against `git ls-files <specs>` (t6135 subset).

use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::repo::Repository;
use grit_lib::worktree::pathspec::pathspec_has_attr_magic;
use tempfile::TempDir;

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e.com")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn git_ls_files(root: &Path, specs: &[&str]) -> Vec<String> {
    let mut args = vec!["ls-files", "--"];
    args.extend_from_slice(specs);
    git(root, &args).lines().map(str::to_owned).collect()
}

fn init_attr_repo() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    let files: &[(&str, &str)] = &[
        (
            ".gitattributes",
            "src/**/*.rs diff=rust linguist-generated\n*.bin -diff\nnotes/* !diff\n",
        ),
        ("src/sub/a.rs", "a\n"),
        ("src/b.rs", "b\n"),
        ("lib/c.rs", "c\n"),
        ("data.bin", "bin\n"),
        ("notes/n.txt", "n\n"),
        ("README.md", "r\n"),
        ("sub/.gitattributes", "*.txt diff=text\n"),
        ("sub/t.txt", "t\n"),
    ];
    for (path, content) in files {
        let full = root.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, content).unwrap();
    }
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "init"]);
    dir
}

fn grit_matches(repo: &Repository, all: &[String], specs: &[&str]) -> Vec<String> {
    let specs: Vec<String> = specs.iter().map(|s| (*s).to_owned()).collect();
    all.iter()
        .filter(|p| repo.paths_match_pathspecs(p, &specs).expect("match"))
        .cloned()
        .collect()
}

#[test]
fn attr_pathspecs_match_git_ls_files() {
    let dir = init_attr_repo();
    let root = dir.path();
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let all = git_ls_files(root, &[]);

    let cases: &[&[&str]] = &[
        &[":(attr:diff=rust)"],
        &[":(attr:-diff)"],
        &[":(attr:!diff)"],
        &[":(attr:diff)"],
        &[":(attr:linguist-generated)"],
        &[":(attr:diff=rust linguist-generated)"],
        &[":(attr:diff=text)"],
        &[":(attr:diff=rust)src/sub"],
        &[":(attr:diff=rust)", ":(exclude)src/sub"],
        &["*", ":(exclude,attr:diff=rust)"],
        &[":(exclude,attr:-diff)"],
        &[":(exclude,attr:!diff)"],
        &[":(attr:-diff)", "README.md"],
        &[":(top,attr:diff=rust)"],
        &[":(icase,attr:diff=rust)SRC"],
    ];
    for specs in cases {
        let expected = git_ls_files(root, specs);
        let got = grit_matches(&repo, &all, specs);
        assert_eq!(got, expected, "pathspecs {specs:?}");
    }
    assert_eq!(
        git_ls_files(root, &[":(attr:diff=rust)"]),
        vec!["src/b.rs".to_owned(), "src/sub/a.rs".to_owned()],
        "fixture sanity: git must select the rust files"
    );
}

#[test]
fn non_attr_pathspecs_still_match_git_ls_files() {
    let dir = init_attr_repo();
    let root = dir.path();
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let all = git_ls_files(root, &[]);
    let cases: &[&[&str]] = &[
        &["src"],
        &["*.rs"],
        &[":(glob)src/**/*.rs"],
        &[":(exclude)src"],
        &[":(icase)readme.MD"],
        &[":(literal)src/b.rs"],
    ];
    for specs in cases {
        assert_eq!(
            grit_matches(&repo, &all, specs),
            git_ls_files(root, specs),
            "pathspecs {specs:?}"
        );
    }
}

#[test]
fn attr_pathspecs_see_gitattributes_edits_on_the_same_handle() {
    let dir = init_attr_repo();
    let root = dir.path();
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let spec = vec![":(attr:diff=rust)".to_owned()];
    assert!(!repo.paths_match_pathspecs("lib/c.rs", &spec).unwrap());
    std::fs::write(root.join(".gitattributes"), "*.rs diff=rust\n").unwrap();
    assert!(repo.paths_match_pathspecs("lib/c.rs", &spec).unwrap());
}

#[test]
fn attr_pathspecs_in_bare_repository_use_info_attributes() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q", "--bare", "-b", "main", "."]);
    std::fs::create_dir_all(root.join("info")).unwrap();
    std::fs::write(root.join("info/attributes"), "*.rs diff=rust\n").unwrap();
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let spec = vec![":(attr:diff=rust)".to_owned()];
    assert!(repo.paths_match_pathspecs("src/a.rs", &spec).unwrap());
    assert!(!repo.paths_match_pathspecs("README.md", &spec).unwrap());
}

#[test]
fn pathspec_matcher_matches_paths_and_renames() {
    let dir = init_attr_repo();
    let root = dir.path();
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let all = git_ls_files(root, &[]);

    let specs = vec![":(attr:diff=rust)".to_owned()];
    let matcher = repo.pathspec_matcher(&specs).expect("matcher");
    assert!(!matcher.is_empty());
    let got: Vec<_> = all.iter().filter(|p| matcher.matches(p)).cloned().collect();
    assert_eq!(got, git_ls_files(root, &[":(attr:diff=rust)"]));
    assert!(matcher.matches_change("lib/c.rs", Some("src/b.rs")));
    assert!(matcher.matches_change("src/b.rs", None));
    assert!(!matcher.matches_change("lib/c.rs", Some("lib/c.rs")));
    assert!(!matcher.matches_change("README.md", None));

    let empty: Vec<String> = Vec::new();
    let all_paths = repo.pathspec_matcher(&empty).expect("matcher");
    assert!(all_paths.is_empty());
    assert!(all_paths.matches_change("anything", None));

    let plain = vec!["src".to_owned(), ":(exclude)src/sub".to_owned()];
    let plain_matcher = repo.pathspec_matcher(&plain).expect("matcher");
    assert!(plain_matcher.matches("src/b.rs"));
    assert!(!plain_matcher.matches("src/sub/a.rs"));
    assert!(plain_matcher.matches_change("lib/x.rs", Some("src/b.rs")));
}

#[test]
fn has_attr_magic_detects_long_magic_only() {
    assert!(pathspec_has_attr_magic(":(attr:diff=rust)"));
    assert!(pathspec_has_attr_magic(":(exclude,attr:-diff)src"));
    assert!(pathspec_has_attr_magic(":(top,attr:!diff)"));
    assert!(!pathspec_has_attr_magic(":(exclude)src"));
    assert!(!pathspec_has_attr_magic(":!attr"));
    assert!(!pathspec_has_attr_magic("attr:diff"));
    assert!(!pathspec_has_attr_magic("src/*.rs"));
}
